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
use crate::inventory::{self, Inventory};

/// How many blocks are sampled from one file at most.
const MAX_BLOCKS_PER_FILE: u64 = 32;

/// Sampling stops once this many bytes have been read for one game.
const MAX_SAMPLE_BYTES: u64 = 512 * 1024 * 1024;

/// A file must save at least this fraction to be worth rewriting.
const MIN_SAVING_RATIO: f64 = 0.03;

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
}

impl FileEstimate {
    /// Bytes this file would save.
    pub fn saving(&self) -> u64 {
        self.disk_now.saturating_sub(self.disk_after)
    }

    /// Whether the saving clears the "worth rewriting" bar.
    pub fn worthwhile(&self) -> bool {
        self.disk_now > 0
            && (self.saving() as f64 / self.disk_now as f64) >= MIN_SAVING_RATIO
            && self.saving() >= u64::from(BtrfsModel::BLOCK)
    }
}

/// The result for a whole game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
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
    /// tried — so without a record, an estimate keeps advertising a saving
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
    let block = u64::from(model.block_size());
    let blocks = size.div_ceil(block).max(1);
    let sample_count = blocks.min(MAX_BLOCKS_PER_FILE);
    // Spread the samples evenly, so a file with a compressible header and
    // incompressible body is not judged by its header alone.
    let stride = blocks / sample_count;

    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; model.block_size() as usize];
    let mut est = FileEstimate { size, ..FileEstimate::default() };
    let mut sampled_now = 0u64;
    let mut sampled_after = 0u64;
    let mut sampled_in = 0u64;

    for i in 0..sample_count {
        let offset = i.saturating_mul(stride).saturating_mul(block);
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
        // A compressed container anywhere in the file means the rest is very
        // likely compressed too.
        if i == 0 && inventory::is_precompressed_magic(chunk) {
            est.disk_now = size;
            est.disk_after = size;
            est.sampled = read as u64;
            return Ok(est);
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
    let candidates: Vec<_> = inv.to_compress().collect();
    let budget = std::sync::atomic::AtomicU64::new(MAX_SAMPLE_BYTES);
    let results: Vec<(FileEstimate, bool)> = candidates
        .par_iter()
        .map(|entry| {
            let remaining = budget.load(std::sync::atomic::Ordering::Relaxed);
            if remaining == 0 {
                // Out of sampling budget: assume the file behaves like the
                // ones already measured by leaving it out of both totals.
                return (FileEstimate { size: entry.size, ..FileEstimate::default() }, false);
            }
            let path = entry.path(install_dir);
            // A pass at this level or higher has already had its chance at
            // this file; whatever it left uncompressed, it left uncompressed
            // for a reason. Claiming a further saving here is how the
            // estimator used to promise space that a rerun could not deliver.
            if probe.attempted_level(&path).is_some_and(|applied| applied >= opts.level) {
                return (
                    FileEstimate {
                        size: entry.size,
                        disk_now: entry.size,
                        disk_after: entry.size,
                        sampled: 0,
                    },
                    false,
                );
            }
            let measured = probe.measure(&path).and_then(|(compressed, mapped)| {
                (mapped > 0).then(|| compressed as f64 / mapped as f64)
            });
            match estimate_file_with(&path, entry.size, model, opts, measured) {
                Ok(est) => {
                    budget.fetch_sub(
                        est.sampled.min(remaining),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    let worth = est.worthwhile();
                    (est, worth)
                }
                // An unreadable file is simply not a candidate.
                Err(_) => (FileEstimate { size: entry.size, ..FileEstimate::default() }, false),
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

/// Reads until the buffer is full or the file ends.
fn read_full(file: &mut std::fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let Some(rest) = buf.get_mut(total..) else { break };
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

    #[test]
    fn btrfs_costs_round_up_and_give_up_on_bad_ratios() -> TestResult {
        let m = BtrfsModel { level: 9 };
        // Compressible: rounded up to a sector.
        check_eq(m.disk_cost(131_072, 5000), 8192, "a compressible block rounds up to a sector")?;
        // Barely smaller: btrfs stores it uncompressed.
        check_eq(
            m.disk_cost(131_072, 130_000),
            131_072,
            "a barely smaller block is stored as-is",
        )?;
        // Exactly one sector.
        check_eq(m.disk_cost(131_072, 4096), 4096, "a block that fits one sector costs one")
    }

    fn write_file(path: &Path, bytes: Vec<u8>) -> Result<u64, String> {
        std::fs::write(path, &bytes).ctx("write a test file")?;
        Ok(bytes.len() as u64)
    }

    #[test]
    fn estimates_compressible_and_random_files() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let model = BtrfsModel { level: 9 };
        let opts = EstimateOpts { level: 9, mount_level: None };

        let zeros = tmp.path().join("zeros.dat");
        let size = write_file(&zeros, vec![0u8; 4 * 1024 * 1024])?;
        let est = estimate_file(&zeros, size, &model, &opts).ctx("estimate zeros.dat")?;
        check(est.disk_after < est.disk_now / 10, format!("{est:?}"))?;
        check(est.worthwhile(), "a file of zeros is worth compressing")?;

        // Pseudo-random bytes stand in for already-compressed game data.
        // splitmix64, whole words at a time: taking one byte per step would
        // leave a short repeating cycle that zstd compresses away.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut noise = Vec::with_capacity(4 * 1024 * 1024);
        while noise.len() < 4 * 1024 * 1024 {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            noise.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
        }
        let random = tmp.path().join("noise.dat");
        let size = write_file(&random, noise)?;
        let est = estimate_file(&random, size, &model, &opts).ctx("estimate noise.dat")?;
        check(!est.worthwhile(), format!("{est:?}"))
    }

    #[test]
    fn a_compressed_header_short_circuits_the_file() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let path = tmp.path().join("archive.pak");
        let mut bytes = vec![0x28, 0xB5, 0x2F, 0xFD];
        bytes.resize(1024 * 1024, 0);
        let size = write_file(&path, bytes)?;
        let est =
            estimate_file(&path, size, &BtrfsModel { level: 9 }, &EstimateOpts {
                level: 9,
                mount_level: None,
            })
            .ctx("estimate archive.pak")?;
        check_eq(est.disk_now, est.disk_after, "a compressed header means nothing to gain")?;
        check(!est.worthwhile(), "an already-compressed file is not worthwhile")
    }

    #[test]
    fn a_compressing_mount_shrinks_the_promised_saving() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let path = tmp.path().join("text.dat");
        let size = write_file(&path, "the quick brown fox ".repeat(200_000).into_bytes())?;
        let model = BtrfsModel { level: 15 };

        let raw = estimate_file(&path, size, &model, &EstimateOpts {
            level: 15,
            mount_level: None,
        })
        .ctx("estimate on a plain mount")?;
        let on_zstd1 = estimate_file(&path, size, &model, &EstimateOpts {
            level: 15,
            mount_level: Some(1),
        })
        .ctx("estimate on a zstd:1 mount")?;
        check_eq(raw.disk_after, on_zstd1.disk_after, "the target level is the same either way")?;
        // Level 1 already banked most of it, so the remaining gain is smaller.
        check(on_zstd1.saving() < raw.saving(), format!("raw {raw:?} vs {on_zstd1:?}"))
    }

    #[test]
    fn game_estimate_sums_only_worthwhile_files() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let dir = tmp.path();
        std::fs::write(dir.join("good.dat"), vec![b'a'; 2 * 1024 * 1024]).ctx("write good.dat")?;
        std::fs::write(dir.join("tiny.cfg"), b"x").ctx("write tiny.cfg")?;
        let inv =
            inventory::walk(dir, &inventory::WalkOpts::default()).ctx("walk the game dir")?;
        let est = estimate_game(dir, &inv, &BtrfsModel { level: 9 }, &EstimateOpts {
            level: 9,
            mount_level: None,
        });
        check_eq(est.files, 1, "one worthwhile file")?;
        check_eq(est.skipped_files, 1, "the tiny file is skipped")?;
        check(est.saving_ratio() > 0.9, format!("{est:?}"))
    }
}
