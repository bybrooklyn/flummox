//! Estimates how much space compressing a game would save.
//!
//! The estimate has to model the *backend's* arithmetic, not zstd's raw
//! ratio: btrfs compresses each 128 KiB block on its own and stores the result
//! in 4 KiB sectors, so a block that barely shrinks saves nothing at all.
//!
//! Files are sampled rather than read whole. A 60 GB install would otherwise
//! take as long to estimate as to compress.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use rayon::prelude::*;

use crate::fsprobe::FsInfo;
use crate::inventory::Inventory;

/// How many blocks are sampled from one file at most.
const MAX_BLOCKS_PER_FILE: u64 = 32;

/// How many blocks are sampled from a file whose header names a compressed
/// container.
///
/// Fewer than [`MAX_BLOCKS_PER_FILE`], because such a file usually has nothing
/// to give and sampling it fully is time spent proving that. More than one,
/// because the header describes the container and not the bytes inside it: an
/// archive that stores some of its entries uncompressed still has saving in it.
const CONTAINER_BLOCKS: u64 = 8;

/// How much of a file's head is read to look for a container's magic number.
///
/// Includes the DDS DX10 extension; no payload lengths control allocation.
const MAGIC_PEEK: usize = 148;

/// Sampling stops once this many bytes have been read for one game.
const MAX_SAMPLE_BYTES: u64 = 512 * 1024 * 1024;

/// Whole-sector minimum shared by desktop and CLI native estimates.
const MIN_SAVING_BYTES: u64 = 4096;

/// Models how a backend turns compressed bytes into disk usage.
pub trait UnitModel: Sync {
    /// The unit the backend compresses independently.
    fn block_size(&self) -> u32;

    /// Disk bytes used by one block, given its compressed size.
    fn disk_cost(&self, uncompressed: u32, compressed: u32) -> u64;

    /// The zstd level to sample with.
    fn level(&self) -> i32;
}

/// btrfs: 128 KiB blocks, 4 KiB sectors.
#[derive(Debug, Clone, Copy)]
pub struct BtrfsModel {
    /// zstd level.
    pub level: i32,
}

impl BtrfsModel {
    /// btrfs compresses at most this much at a time.
    pub const BLOCK: u32 = 128 * 1024;
    /// Allocation granularity.
    pub const SECTOR: u32 = 4096;
}

impl UnitModel for BtrfsModel {
    fn block_size(&self) -> u32 {
        Self::BLOCK
    }

    fn disk_cost(&self, uncompressed: u32, compressed: u32) -> u64 {
        // btrfs keeps the compressed copy only if it saves at least one
        // sector; otherwise the block is stored as-is.
        let rounded = |n: u32| u64::from(n.div_ceil(Self::SECTOR)) * u64::from(Self::SECTOR);
        if compressed.saturating_add(Self::SECTOR) > uncompressed {
            rounded(uncompressed)
        } else {
            rounded(compressed)
        }
    }

    fn level(&self) -> i32 {
        self.level
    }
}

/// The writable pack's independently decodable frame model.
#[derive(Debug, Clone, Copy)]
pub struct PackModel {
    /// zstd level used for the projection.
    pub level: i32,
}

impl PackModel {
    /// Largest independently decoded frame in the current store.
    pub const BLOCK: u32 = 4 * 1024 * 1024;
    /// Allocation unit used for conservative projections.
    pub const SECTOR: u32 = 4096;
}

impl UnitModel for PackModel {
    fn block_size(&self) -> u32 {
        Self::BLOCK
    }

    fn disk_cost(&self, uncompressed: u32, compressed: u32) -> u64 {
        let rounded = |n: u32| u64::from(n.div_ceil(Self::SECTOR)) * u64::from(Self::SECTOR);
        if compressed.saturating_add(Self::SECTOR) >= uncompressed {
            rounded(uncompressed)
        } else {
            rounded(compressed)
        }
    }

    fn level(&self) -> i32 {
        self.level
    }
}

/// What sampling one file concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileEstimate {
    /// The file's size.
    pub size: u64,
    /// Estimated disk usage as it is stored now.
    pub disk_now: u64,
    /// Estimated disk usage after compression at the target level.
    pub disk_after: u64,
    /// Bytes actually read while sampling.
    pub sampled: u64,
    /// Header evidence used to choose the sampling effort.
    pub inspection: crate::classify::Inspection,
}

impl FileEstimate {
    /// Bytes this file would save.
    pub fn saving(&self) -> u64 {
        self.disk_now.saturating_sub(self.disk_after)
    }

    /// Whether the saving clears the "worth rewriting" bar.
    pub fn worthwhile(&self) -> bool {
        self.disk_now > 0 && self.saving() >= MIN_SAVING_BYTES
    }
}

/// Counts format evidence gathered from sampled file headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct FormatEvidence {
    /// Files whose header matched a known family.
    pub recognized_files: u64,
    /// Files kept eligible despite an unknown header.
    pub unknown_files: u64,
    /// Encoded media, streams, and block textures.
    pub encoded_files: u64,
    /// Archives and mixed game containers.
    pub container_files: u64,
    /// Texture payloads stored without block compression.
    pub raw_texture_files: u64,
    /// Unencoded audio or video payloads that remain good candidates.
    #[serde(default)]
    pub raw_media_files: u64,
    /// PE and ELF executables.
    pub executable_files: u64,
    /// Encrypted archive members, which are still sampled.
    pub encrypted_files: u64,
}

impl FormatEvidence {
    /// Adds one inspected header to the counters.
    pub fn observe(&mut self, inspection: crate::classify::Inspection) {
        use crate::classify::{Family, Format, Protection};
        if inspection.family == Family::Unknown {
            self.unknown_files = self.unknown_files.saturating_add(1);
        } else {
            self.recognized_files = self.recognized_files.saturating_add(1);
        }
        match inspection.format {
            Format::Encoded | Format::BlockTexture => {
                self.encoded_files = self.encoded_files.saturating_add(1);
            }
            Format::StoredArchive | Format::MixedContainer => {
                self.container_files = self.container_files.saturating_add(1);
            }
            Format::RawTexture => {
                self.raw_texture_files = self.raw_texture_files.saturating_add(1);
            }
            Format::RawMedia => {
                self.raw_media_files = self.raw_media_files.saturating_add(1);
            }
            Format::Executable => {
                self.executable_files = self.executable_files.saturating_add(1);
            }
            Format::Unknown | Format::Malformed => {}
        }
        if inspection.protection == Protection::EncryptedMember {
            self.encrypted_files = self.encrypted_files.saturating_add(1);
        }
    }
}

/// The result for a whole game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Estimate {
    /// Files a compress pass would rewrite.
    ///
    /// Larger than `files`: the job rewrites everything the inventory picked,
    /// while `files` counts only those sampling expects to shrink. Reporting
    /// just the latter made an estimate look like it described a much smaller
    /// job than it does.
    pub rewrite_files: u64,
    /// Files expected to actually get smaller.
    pub files: u64,
    /// Their total size.
    pub bytes: u64,
    /// Estimated disk usage now, for those files.
    pub disk_now: u64,
    /// Estimated disk usage after.
    pub disk_after: u64,
    /// Projected usage in the stronger writable pack over the same files.
    #[serde(default)]
    pub maximum_after: Option<u64>,
    /// Files skipped as too small, already compressed, or not worth it.
    pub skipped_files: u64,
    /// Bytes actually read while sampling.
    pub sampled: u64,
    /// Every byte in the install directory, including files nothing will
    /// touch. The saving means little without it.
    pub install_bytes: u64,
    /// Whether the mount already compresses, making `disk_now` a guess at
    /// what the filesystem has already achieved rather than the raw size.
    pub already_compressed_mount: bool,
    /// Eligible files not sampled because of limits, cancellation, or read errors.
    #[serde(default)]
    pub unsampled_files: u64,
    /// Eligible files with actual content samples, including those with no gain.
    #[serde(default)]
    pub inspected_files: u64,
    /// Header evidence explaining which formats were recognized and sampled.
    #[serde(default)]
    pub format_evidence: FormatEvidence,
}

impl Estimate {
    /// Bytes that would be freed.
    pub fn saving(&self) -> u64 {
        self.disk_now.saturating_sub(self.disk_after)
    }

    /// The saving as a fraction of current usage.
    pub fn saving_ratio(&self) -> f64 {
        if self.disk_now == 0 {
            return 0.0;
        }
        self.saving() as f64 / self.disk_now as f64
    }

    /// Bytes a writable pack is projected to free, when it was sampled.
    pub fn maximum_saving(&self) -> Option<u64> {
        self.maximum_after
            .map(|after| self.disk_now.saturating_sub(after))
    }
}

/// How to estimate.
#[derive(Debug, Clone, Copy)]
pub struct EstimateOpts {
    /// Target zstd level.
    pub level: i32,
    /// The level the mount already applies, if any.
    ///
    /// On a `compress=zstd:1` mount the data on disk is already compressed, so
    /// comparing against the raw file size would promise savings that are
    /// already banked.
    pub mount_level: Option<i32>,
}

impl EstimateOpts {
    /// Builds options for a target level, reading the mount's own setting from
    /// the probed filesystem.
    pub fn new(level: i32, fs: &FsInfo) -> Self {
        let mount_level = fs.mount_compression().and_then(|(algo, level)| {
            // Only zstd is modelled; lzo and zlib would need their own curves.
            (algo == "zstd").then_some(level.unwrap_or(3))
        });
        Self { level, mount_level }
    }
}

/// Samples one file and estimates its disk usage before and after.
/// Measures how much of a file is already stored compressed.
///
/// Without this the estimator can only guess the current state from the
/// mount's options, and that guess was badly wrong in practice: a mount with
/// `compress=zstd:1` still holds plenty of files the kernel never compressed,
/// so a real job freed nearly six times what was predicted.
pub trait DiskProbe: Sync {
    /// Returns (bytes held compressed, bytes mapped), or `None` if unknown.
    fn measure(&self, path: &Path) -> Option<(u64, u64)>;

    /// The zstd level already applied to this file by an earlier pass, if the
    /// caller has a record of one.
    ///
    /// This closes the last gap in the estimate. btrfs stores a block
    /// uncompressed whenever compressing it would not free a whole sector,
    /// and the result is indistinguishable on disk from a block nothing ever
    /// tried. Without a record, an estimate keeps advertising a saving
    /// that an earlier pass already proved is not there. Measured on real
    /// installs: Celeste still claimed about 45 MB immediately after a max
    /// pass, and Balatro predicted 717 kB where a rerun actually freed 369 kB.
    fn attempted_level(&self, _path: &Path) -> Option<i32> {
        None
    }
}

/// A probe that measures nothing, so estimates fall back to the mount options.
pub struct NoProbe;

impl DiskProbe for NoProbe {
    fn measure(&self, _path: &Path) -> Option<(u64, u64)> {
        None
    }
}

/// Samples one file, guessing its current state from the mount options.
pub fn estimate_file(
    path: &Path,
    size: u64,
    model: &dyn UnitModel,
    opts: &EstimateOpts,
) -> io::Result<FileEstimate> {
    estimate_file_with(path, size, model, opts, None)
}

/// Samples one file, given how much of it is already stored compressed.
///
/// `measured` is the fraction from 0.0 to 1.0, usually from a [`DiskProbe`].
pub fn estimate_file_with(
    path: &Path,
    size: u64,
    model: &dyn UnitModel,
    opts: &EstimateOpts,
    measured: Option<f64>,
) -> io::Result<FileEstimate> {
    let file = std::fs::File::open(path)?;
    estimate_open_file(&file, size, model, opts, measured)
}

/// Samples an anchored file handle without resolving its path again.
pub fn estimate_open_file(
    mut file: &std::fs::File,
    size: u64,
    model: &dyn UnitModel,
    opts: &EstimateOpts,
    measured: Option<f64>,
) -> io::Result<FileEstimate> {
    let block = u64::from(model.block_size());
    let blocks = size.div_ceil(block).max(1);
    let mut buf = vec![0u8; model.block_size() as usize];
    // A container's magic number lowers how much of the file is sampled. It
    // used to end the estimate at the first block, which reported no saving
    // for every file starting with one, including archives holding entries
    // that were never compressed.
    let head = read_full(&mut file, buf.get_mut(..MAGIC_PEEK).unwrap_or(&mut []))?;
    let inspection = crate::classify::inspect(buf.get(..head).unwrap_or(&[]));
    let container = inspection.format.cheap_sample();
    let byte_cap = if model.block_size() > BtrfsModel::BLOCK {
        16 * 1024 * 1024u64
    } else {
        u64::from(BtrfsModel::BLOCK) * MAX_BLOCKS_PER_FILE
    };
    let bounded_samples = byte_cap.div_ceil(block).max(1);
    let sample_count = blocks.min(bounded_samples).min(if container {
        CONTAINER_BLOCKS
    } else {
        MAX_BLOCKS_PER_FILE
    });
    // Spread the samples evenly, so a file with a compressible header and
    // incompressible body is not judged by its header alone.
    let mut est = FileEstimate {
        size,
        inspection,
        ..FileEstimate::default()
    };
    let mut sampled_now = 0u64;
    let mut sampled_after = 0u64;
    let mut sampled_in = 0u64;

    for i in 0..sample_count {
        let offset = sample_block(i, blocks, sample_count).saturating_mul(block);
        if offset >= size {
            break;
        }
        file.seek(SeekFrom::Start(offset))?;
        let want = block.min(size - offset) as usize;
        let read = read_full(&mut file, buf.get_mut(..want).unwrap_or(&mut []))?;
        let Some(chunk) = buf.get(..read) else { break };
        if chunk.is_empty() {
            break;
        }
        let uncompressed = chunk.len() as u32;
        let after = zstd::bulk::compress(chunk, opts.level)?.len() as u32;
        let raw_cost = model.disk_cost(uncompressed, uncompressed) as f64;
        // What this block costs today. A measurement beats a guess: the probe
        // says what share of the file really is compressed, and the rest is
        // sitting there raw whatever the mount options claim.
        let mount_cost = |level: i32| -> io::Result<f64> {
            let c = if level == opts.level {
                after
            } else {
                zstd::bulk::compress(chunk, level)?.len() as u32
            };
            Ok(model.disk_cost(uncompressed, c) as f64)
        };
        let cost_now = match measured {
            Some(frac) => {
                let frac = frac.clamp(0.0, 1.0);
                if frac <= 0.0 {
                    raw_cost
                } else {
                    let compressed_cost = mount_cost(opts.mount_level.unwrap_or(3))?;
                    compressed_cost * frac + raw_cost * (1.0 - frac)
                }
            }
            None => match opts.mount_level {
                Some(level) => mount_cost(level)?,
                None => raw_cost,
            },
        };
        sampled_in = sampled_in.saturating_add(read as u64);
        sampled_now = sampled_now.saturating_add(cost_now as u64);
        sampled_after = sampled_after.saturating_add(model.disk_cost(uncompressed, after));
    }

    est.sampled = sampled_in;
    if sampled_in == 0 {
        est.disk_now = size;
        est.disk_after = size;
        return Ok(est);
    }
    // Scale what the samples cost up to the whole file.
    let scale = size as f64 / sampled_in as f64;
    est.disk_now = (sampled_now as f64 * scale) as u64;
    est.disk_after = (sampled_after as f64 * scale) as u64;
    Ok(est)
}

/// Estimates a whole game from an inventory.
///
/// Files are sampled in parallel; `install_dir` is the directory the
/// inventory's relative paths are based on.
pub fn estimate_game(
    install_dir: &Path,
    inv: &Inventory,
    model: &dyn UnitModel,
    opts: &EstimateOpts,
) -> Estimate {
    estimate_game_with(install_dir, inv, model, opts, &NoProbe)
}

/// Estimates a whole game, measuring each file's current state with `probe`.
pub fn estimate_game_with(
    install_dir: &Path,
    inv: &Inventory,
    model: &dyn UnitModel,
    opts: &EstimateOpts,
    probe: &dyn DiskProbe,
) -> Estimate {
    estimate_game_cancellable(install_dir, inv, model, opts, probe, None)
}

/// [`estimate_game_with`], stoppable part way through.
///
/// Sampling reads up to [`MAX_SAMPLE_BYTES`] and compresses every block it
/// reads at the target level, twice where the mount already compresses. At
/// level 15 on a large install that runs for a while, and `estimate` is the
/// first command anyone tries, so it has to answer Ctrl-C. A cancelled
/// estimate returns what it measured so far, which is why the result carries
/// `sampled`.
pub fn estimate_game_cancellable(
    install_dir: &Path,
    inv: &Inventory,
    model: &dyn UnitModel,
    opts: &EstimateOpts,
    probe: &dyn DiskProbe,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Estimate {
    let cancelled = || cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed));
    let candidates: Vec<_> = inv.to_compress().collect();
    let budget = std::sync::atomic::AtomicU64::new(MAX_SAMPLE_BYTES);
    let results: Vec<(FileEstimate, bool)> = candidates
        .par_iter()
        .map(|entry| {
            if cancelled() {
                return (
                    FileEstimate {
                        size: entry.size,
                        ..FileEstimate::default()
                    },
                    false,
                );
            }
            // Read once and clamp. Two threads can both pass a bare `== 0`
            // check and both subtract, wrapping the counter to about 1.8e19,
            // after which the budget stops limiting anything.
            let remaining = budget.load(std::sync::atomic::Ordering::Acquire);
            if remaining == 0 {
                // Out of sampling budget: assume the file behaves like the
                // ones already measured by leaving it out of both totals.
                return (
                    FileEstimate {
                        size: entry.size,
                        ..FileEstimate::default()
                    },
                    false,
                );
            }
            let path = entry.path(install_dir);
            // A pass at this level or higher has already had its chance at
            // this file; whatever it left uncompressed, it left uncompressed
            // for a reason. Claiming a further saving here is how the
            // estimator used to promise space that a rerun could not deliver.
            if probe
                .attempted_level(&path)
                .is_some_and(|applied| applied >= opts.level)
            {
                return (
                    FileEstimate {
                        size: entry.size,
                        disk_now: entry.size,
                        disk_after: entry.size,
                        sampled: 0,
                        inspection: crate::classify::Inspection::default(),
                    },
                    false,
                );
            }
            let measured = probe.measure(&path).and_then(|(compressed, mapped)| {
                (mapped > 0).then(|| compressed as f64 / mapped as f64)
            });
            match estimate_file_with(&path, entry.size, model, opts, measured) {
                Ok(est) => {
                    // Saturating, not wrapping: `remaining` was read before
                    // the file was sampled, so another thread may have spent
                    // the budget in between.
                    let _spent = budget.fetch_update(
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                        |left| Some(left.saturating_sub(est.sampled)),
                    );
                    let worth = est.worthwhile();
                    (est, worth)
                }
                // An unreadable file is simply not a candidate.
                Err(_) => (
                    FileEstimate {
                        size: entry.size,
                        ..FileEstimate::default()
                    },
                    false,
                ),
            }
        })
        .collect();

    let mut out = Estimate {
        rewrite_files: candidates.len() as u64,
        skipped_files: (inv.files.len() - candidates.len()) as u64,
        install_bytes: inv.total_bytes(),
        already_compressed_mount: opts.mount_level.is_some(),
        ..Estimate::default()
    };
    for (est, worthwhile) in results {
        out.sampled = out.sampled.saturating_add(est.sampled);
        out.inspected_files += u64::from(est.sampled > 0);
        if est.sampled > 0 {
            out.format_evidence.observe(est.inspection);
        }
        if est.sampled == 0 && est.disk_now == 0 && est.disk_after == 0 {
            out.unsampled_files += 1;
        }
        if !worthwhile {
            out.skipped_files = out.skipped_files.saturating_add(1);
            continue;
        }
        out.files = out.files.saturating_add(1);
        out.bytes = out.bytes.saturating_add(est.size);
        out.disk_now = out.disk_now.saturating_add(est.disk_now);
        out.disk_after = out.disk_after.saturating_add(est.disk_after);
    }
    out
}

/// How many blocks are sampled when choosing a file's level.
const LEVEL_SAMPLE_BLOCKS: u64 = 8;

/// Maximum keeps any whole-sector improvement in the samples. A percentage
/// floor would discard small ratios that still mean substantial space on a
/// large file. Ties and regressions keep the cheaper level.
fn worth_the_level(cost_low: u64, cost_high: u64, sampled: u64) -> bool {
    sampled > 0 && cost_low.saturating_sub(cost_high) >= 4096
}

/// The level to compress one file at, given a cheap and an expensive choice.
///
/// Takes an open handle, not a path, so a job keeps reaching files only
/// through the directory it holds open.
pub fn choose_level(
    file: &std::fs::File,
    size: u64,
    model: &dyn UnitModel,
    low: i32,
    high: i32,
) -> io::Result<i32> {
    if low >= high {
        return Ok(low);
    }
    let block = u64::from(model.block_size());
    let blocks = size.div_ceil(block).max(1);
    let samples = blocks.min(LEVEL_SAMPLE_BLOCKS);
    let mut handle = file;
    let mut buf = vec![0u8; model.block_size() as usize];
    let (mut cost_low, mut cost_high, mut sampled) = (0u64, 0u64, 0u64);

    for i in 0..samples {
        let offset = sample_block(i, blocks, samples).saturating_mul(block);
        if offset >= size {
            break;
        }
        handle.seek(SeekFrom::Start(offset))?;
        let want = block.min(size - offset) as usize;
        let read = read_full(&mut handle, buf.get_mut(..want).unwrap_or(&mut []))?;
        let Some(chunk) = buf.get(..read) else { break };
        if chunk.is_empty() {
            break;
        }
        let raw = chunk.len() as u32;
        let at_low = zstd::bulk::compress(chunk, low)?.len() as u32;
        let at_high = zstd::bulk::compress(chunk, high)?.len() as u32;
        cost_low = cost_low.saturating_add(model.disk_cost(raw, at_low));
        cost_high = cost_high.saturating_add(model.disk_cost(raw, at_high));
        sampled = sampled.saturating_add(read as u64);
    }
    Ok(if worth_the_level(cost_low, cost_high, sampled) {
        high
    } else {
        low
    })
}

/// Spaces bounded samples across the head, middle, and tail.
fn sample_block(index: u64, blocks: u64, samples: u64) -> u64 {
    if samples <= 1 {
        0
    } else {
        index.saturating_mul(blocks.saturating_sub(1)) / (samples - 1)
    }
}

/// Reads until the buffer is full or the file ends.
fn read_full<R: Read>(file: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let Some(rest) = buf.get_mut(total..) else {
            break;
        };
        match file.read(rest) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    use super::*;
    use crate::inventory;

    #[test]
    fn distributed_samples_include_the_last_block() -> TestResult {
        check_eq(sample_block(0, 1000, 32), 0, "head included")?;
        check_eq(sample_block(31, 1000, 32), 999, "tail included")?;
        for index in 1..32 {
            check(
                sample_block(index, 1000, 32) > sample_block(index - 1, 1000, 32),
                "no duplicate sampled blocks",
            )?;
        }
        check_eq(sample_block(0, 1, 1), 0, "single-block file")
    }

    #[test]
    fn low_ratios_and_small_files_still_contribute_real_savings() -> TestResult {
        let large = FileEstimate {
            size: 1_000_000_000,
            disk_now: 1_000_000_000,
            disk_after: 990_000_000,
            sampled: 1_048_576,
            inspection: crate::classify::Inspection::default(),
        };
        check(
            large.worthwhile(),
            "one percent of a large file is useful space",
        )?;
        let small = FileEstimate {
            size: 65_536,
            disk_now: 65_536,
            disk_after: 4_096,
            sampled: 65_536,
            inspection: crate::classify::Inspection::default(),
        };
        check(
            small.worthwhile(),
            "a file smaller than 128 KiB can save space",
        )?;
        check(
            !FileEstimate {
                disk_after: 65_535,
                ..small
            }
            .worthwhile(),
            "subsector gain is not claimed",
        )
    }

    #[test]
    fn btrfs_costs_round_up_and_give_up_on_bad_ratios() -> TestResult {
        let m = BtrfsModel { level: 9 };
        // Compressible: rounded up to a sector.
        check_eq(
            m.disk_cost(131_072, 5000),
            8192,
            "a compressible block rounds up to a sector",
        )?;
        // Barely smaller: btrfs stores it uncompressed.
        check_eq(
            m.disk_cost(131_072, 130_000),
            131_072,
            "a barely smaller block is stored as-is",
        )?;
        // Exactly one sector.
        check_eq(
            m.disk_cost(131_072, 4096),
            4096,
            "a block that fits one sector costs one",
        )
    }

    fn write_file(path: &Path, bytes: Vec<u8>) -> Result<u64, String> {
        std::fs::write(path, &bytes).ctx("write a test file")?;
        Ok(bytes.len() as u64)
    }

    /// Pseudo-random bytes, standing in for already-compressed game data.
    ///
    /// splitmix64, whole words at a time: taking one byte per step would leave
    /// a short repeating cycle that zstd compresses away.
    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            out.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn estimates_compressible_and_random_files() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let model = BtrfsModel { level: 9 };
        let opts = EstimateOpts {
            level: 9,
            mount_level: None,
        };

        let zeros = tmp.path().join("zeros.dat");
        let size = write_file(&zeros, vec![0u8; 4 * 1024 * 1024])?;
        let est = estimate_file(&zeros, size, &model, &opts).ctx("estimate zeros.dat")?;
        check(est.disk_after < est.disk_now / 10, format!("{est:?}"))?;
        check(est.worthwhile(), "a file of zeros is worth compressing")?;

        let random = tmp.path().join("noise.dat");
        let size = write_file(&random, noise(4 * 1024 * 1024))?;
        let est = estimate_file(&random, size, &model, &opts).ctx("estimate noise.dat")?;
        check(!est.worthwhile(), format!("{est:?}"))
    }

    #[test]
    fn the_slower_level_is_used_only_where_it_pays() -> TestResult {
        let block = u64::from(BtrfsModel::BLOCK);
        check(
            !worth_the_level(block, block, block),
            "no gain does not earn it",
        )?;
        check(
            !worth_the_level(0, 0, 0),
            "nothing sampled does not earn it",
        )?;
        check(
            !worth_the_level(1000, 990, 1000),
            "subsector gains do not earn a rewrite",
        )?;
        check(
            worth_the_level(1_048_576, 1_044_480, 1_048_576),
            "maximum keeps a whole-sector gain below one percent",
        )?;
        check(
            !worth_the_level(8192, 12288, 131072),
            "a higher level that costs more is rejected",
        )
    }

    #[test]
    fn a_file_with_nothing_to_gain_keeps_the_cheaper_level() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let path = tmp.path().join("noise.dat");
        let size = write_file(&path, noise(2 * 1024 * 1024))?;
        let file = std::fs::File::open(&path).ctx("open noise.dat")?;
        let chosen = choose_level(&file, size, &BtrfsModel { level: 9 }, 9, 15)
            .ctx("choose a level for noise.dat")?;
        check_eq(
            chosen,
            9,
            "incompressible data does not earn the slower level",
        )?;
        // A single candidate is answered without reading anything.
        let same = choose_level(&file, size, &BtrfsModel { level: 9 }, 15, 15)
            .ctx("choose between one level")?;
        check_eq(same, 15, "one candidate is the answer")
    }

    #[test]
    fn a_container_header_is_measured_rather_than_assumed() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let model = BtrfsModel { level: 9 };
        let opts = EstimateOpts {
            level: 9,
            mount_level: None,
        };

        // A zstd header over bytes that really are incompressible. This is the
        // case the magic number is a useful hint for, and it still reports
        // nothing to gain.
        let packed = tmp.path().join("packed.pak");
        let mut bytes = vec![0x28, 0xB5, 0x2F, 0xFD];
        bytes.extend_from_slice(&noise(1024 * 1024));
        let size = write_file(&packed, bytes)?;
        let est = estimate_file(&packed, size, &model, &opts).ctx("estimate packed.pak")?;
        check_eq(
            est.inspection.family,
            crate::classify::Family::Zstandard,
            "the estimate retains its format evidence",
        )?;
        check(
            !est.worthwhile(),
            format!("a packed archive has nothing to give: {est:?}"),
        )?;

        // The same header over bytes that compress. An archive can hold
        // entries it never compressed, and reading one block and stopping
        // reported no saving for every one of them.
        let loose = tmp.path().join("loose.pak");
        let mut bytes = vec![0x28, 0xB5, 0x2F, 0xFD];
        bytes.resize(1024 * 1024, 0);
        let size = write_file(&loose, bytes)?;
        let est = estimate_file(&loose, size, &model, &opts).ctx("estimate loose.pak")?;
        check(
            est.worthwhile(),
            format!("a loose archive is worth rewriting: {est:?}"),
        )?;
        check(est.disk_after < est.disk_now / 2, format!("{est:?}"))
    }

    #[test]
    fn a_compressing_mount_shrinks_the_promised_saving() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let path = tmp.path().join("text.dat");
        let size = write_file(&path, "the quick brown fox ".repeat(200_000).into_bytes())?;
        let model = BtrfsModel { level: 15 };

        let raw = estimate_file(
            &path,
            size,
            &model,
            &EstimateOpts {
                level: 15,
                mount_level: None,
            },
        )
        .ctx("estimate on a plain mount")?;
        let on_zstd1 = estimate_file(
            &path,
            size,
            &model,
            &EstimateOpts {
                level: 15,
                mount_level: Some(1),
            },
        )
        .ctx("estimate on a zstd:1 mount")?;
        check_eq(
            raw.disk_after,
            on_zstd1.disk_after,
            "the target level is the same either way",
        )?;
        // Level 1 already banked most of it, so the remaining gain is smaller.
        check(
            on_zstd1.saving() < raw.saving(),
            format!("raw {raw:?} vs {on_zstd1:?}"),
        )
    }

    #[test]
    fn game_estimate_sums_only_worthwhile_files() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let dir = tmp.path();
        std::fs::write(dir.join("good.dat"), vec![b'a'; 2 * 1024 * 1024]).ctx("write good.dat")?;
        std::fs::write(dir.join("tiny.cfg"), b"x").ctx("write tiny.cfg")?;
        let inv = inventory::walk(dir, &inventory::WalkOpts::default()).ctx("walk the game dir")?;
        let est = estimate_game(
            dir,
            &inv,
            &BtrfsModel { level: 9 },
            &EstimateOpts {
                level: 9,
                mount_level: None,
            },
        );
        check_eq(est.files, 1, "one worthwhile file")?;
        check_eq(est.skipped_files, 1, "the tiny file is skipped")?;
        check(est.saving_ratio() > 0.9, format!("{est:?}"))
    }
}
