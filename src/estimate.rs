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

use crate::db::NOT_ATTEMPTED;
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

/// Window used by the desktop preview shared by native and pack estimates.
///
/// One native block, smaller than the pack's real frame. That keeps the
/// projection conservative and lets a budget of a few windows per file cover
/// the whole file instead of its head and tail.
const PREVIEW_WINDOW: u64 = 128 * 1024;

/// Smallest per-file budget, in bytes, that still yields a usable sample.
///
/// Below this, a file should be treated as unsampled: one window shorter than
/// a few sectors cannot show the whole-sector saving both models require.
pub const MIN_SAMPLE_BUDGET: u64 = 128 * 1024;

/// Models how a backend turns compressed bytes into disk usage.
pub trait UnitModel: Sync {
    /// The unit the backend compresses independently.
    fn block_size(&self) -> u32;

    /// Disk bytes used by one block, given its compressed size.
    fn disk_cost(&self, uncompressed: u32, compressed: u32) -> u64;

    /// Whether the backend leaves files marked no-copy-on-write uncompressed.
    /// Estimates for such a backend predict no saving for those files.
    fn skips_nocow_files(&self) -> bool {
        false
    }
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

    fn skips_nocow_files(&self) -> bool {
        true
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
    #[serde(default)]
    pub maximum_qualified: bool,
    #[serde(default)]
    pub maximum_qualification: Option<[u8; 32]>,
    #[serde(default)]
    pub small_files: crate::pack::SmallFileSample,
    /// Files a native compress pass would rewrite, including eligible files
    /// left unsampled when the analysis budget ran out.
    pub rewrite_files: u64,
    /// Sampled files expected to shrink with native compression or Maximum Space.
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

    /// The bytes a saving is judged against: the whole install, or the
    /// current usage of the files that were sampled if that is larger.
    ///
    /// `disk_now` covers only files expected to shrink, so a relative
    /// threshold taken against it flatters a game that is mostly video.
    pub fn current_bytes(&self) -> u64 {
        self.install_bytes.max(self.disk_now)
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
    /// The lowest level the plan applies, when it spans several.
    ///
    /// A file recorded at or above this has already had its chance. `None`
    /// means `level`.
    pub floor: Option<i32>,
}

impl EstimateOpts {
    /// Builds options for a target level, reading the mount's own setting from
    /// the probed filesystem.
    pub fn new(level: i32, fs: &FsInfo) -> Self {
        let mount_level = fs.mount_compression().and_then(|(algo, level)| {
            // Only zstd is modelled; lzo and zlib would need their own curves.
            (algo == "zstd").then_some(level.unwrap_or(3))
        });
        Self {
            level,
            mount_level,
            floor: None,
        }
    }

    /// Sets the lowest level of the plan being estimated.
    #[must_use]
    pub fn with_floor(self, floor: i32) -> Self {
        Self {
            floor: Some(floor),
            ..self
        }
    }
}

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

/// The `FS_IOC_GETFLAGS` read behind the no-copy-on-write check. It repeats the
/// one in `backend/btrfs.rs`, which is private to that module.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod attributes {
    use std::{fs::File, os::fd::AsRawFd};

    nix::ioctl_read!(fs_ioc_getflags, b'f', 1, libc::c_long);

    /// `FS_NOCOW_FL`: btrfs never compresses the file.
    const FS_NOCOW_FL: libc::c_long = 0x0080_0000;

    /// Whether the file is marked no-copy-on-write. A filesystem without the
    /// ioctl answers false.
    pub(super) fn is_nocow(file: &File) -> bool {
        let mut flags: libc::c_long = 0;
        // SAFETY: `file` is open and `flags` is writable for the whole call. A
        // filesystem without the ioctl makes the kernel return ENOTTY.
        let read = unsafe { fs_ioc_getflags(file.as_raw_fd(), &mut flags) };
        read.is_ok() && flags & FS_NOCOW_FL != 0
    }
}

/// How an open file sits on disk, as opposed to what its bytes would compress to.
#[derive(Debug, Clone, Copy, Default)]
struct Residency {
    /// Bytes the filesystem has allocated, capped at the logical size. `None`
    /// where the figure is not trusted: only Linux reports it reliably.
    allocated: Option<u64>,
    /// The file is marked no-copy-on-write.
    nocow: bool,
}

impl Residency {
    #[cfg(target_os = "linux")]
    fn of(file: &std::fs::File, size: u64) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            allocated: file
                .metadata()
                .ok()
                .map(|meta| meta.blocks().saturating_mul(512).min(size)),
            nocow: attributes::is_nocow(file),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn of(_file: &std::fs::File, _size: u64) -> Self {
        Self::default()
    }

    /// Prices what is on disk. A sparse file costs its allocated bytes, and the
    /// saving shrinks in proportion, since sampling a hole reads zeros that
    /// compress to nothing but were never stored.
    fn price(&self, est: &mut FileEstimate) {
        let Some(allocated) = self.allocated.filter(|a| *a < est.size && est.size > 0) else {
            return;
        };
        let share = allocated as f64 / est.size as f64;
        let saving = est.saving();
        est.disk_now = est.disk_now.min(allocated).min((est.disk_now as f64 * share) as u64);
        let kept = ((saving as f64 * share) as u64).min(est.disk_now);
        est.disk_after = est.disk_now - kept;
    }

    /// Gives a file the filesystem will not compress no saving.
    fn skip_nocow(&self, est: &mut FileEstimate) {
        if self.nocow {
            est.disk_after = est.disk_now;
        }
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
    let residency = Residency::of(file, size);
    residency.price(&mut est);
    if model.skips_nocow_files() {
        residency.skip_nocow(&mut est);
    }
    Ok(est)
}

/// Models used to score one shared desktop preview sample.
pub(crate) struct PreviewEstimate<'a> {
    /// Native filesystem model.
    pub native: &'a dyn UnitModel,
    /// Native target and current-mount settings.
    pub native_opts: &'a EstimateOpts,
    /// Fraction of the file already stored compressed.
    pub measured: Option<f64>,
    /// Writable pack model.
    pub maximum: &'a dyn UnitModel,
    /// Writable pack compression settings.
    pub maximum_opts: &'a EstimateOpts,
    /// Most source data this file may consume from the analysis budget.
    pub byte_cap: u64,
}

/// Samples one anchored file once and scores the bytes for both native and
/// writable-pack compression.
///
/// The two estimates report the same `sampled` byte count because they share
/// the reads. Callers accounting for I/O should add it only once.
pub(crate) fn estimate_open_file_pair(
    mut file: &std::fs::File,
    size: u64,
    preview: PreviewEstimate<'_>,
) -> io::Result<(FileEstimate, FileEstimate)> {
    let fits_in_budget = size <= preview.byte_cap;
    let window = if fits_in_budget {
        size.max(1)
    } else {
        PREVIEW_WINDOW.min(preview.byte_cap.max(1))
    };
    let samples = if fits_in_budget {
        1
    } else {
        (preview.byte_cap / window).max(1)
    };
    let buffer_len = usize::try_from(window).unwrap_or(usize::MAX);
    let mut buf = vec![0u8; buffer_len];
    let head = read_full(&mut file, buf.get_mut(..MAGIC_PEEK).unwrap_or(&mut []))?;
    let inspection = crate::classify::inspect(buf.get(..head).unwrap_or(&[]));
    let mut sampled_in = 0u64;
    let mut native_now = 0u64;
    let mut native_after = 0u64;
    let mut maximum_after = 0u64;

    for index in 0..samples {
        let offset = sample_offset(index, size, window, samples);
        if offset >= size {
            break;
        }
        file.seek(SeekFrom::Start(offset))?;
        let want = window.min(size - offset) as usize;
        let read = read_full(&mut file, buf.get_mut(..want).unwrap_or(&mut []))?;
        let Some(sample) = buf.get(..read) else { break };
        if sample.is_empty() {
            break;
        }

        let native_block = preview.native.block_size() as usize;
        for chunk in sample.chunks(native_block) {
            let raw = chunk.len() as u32;
            let compressed = zstd::bulk::compress(chunk, preview.native_opts.level)?.len() as u32;
            let raw_cost = preview.native.disk_cost(raw, raw) as f64;
            let mount_cost = |level: i32| -> io::Result<f64> {
                let bytes = if level == preview.native_opts.level {
                    compressed
                } else {
                    zstd::bulk::compress(chunk, level)?.len() as u32
                };
                Ok(preview.native.disk_cost(raw, bytes) as f64)
            };
            let cost_now = match preview.measured {
                Some(fraction) => {
                    let fraction = fraction.clamp(0.0, 1.0);
                    if fraction <= 0.0 {
                        raw_cost
                    } else {
                        mount_cost(preview.native_opts.mount_level.unwrap_or(3))? * fraction
                            + raw_cost * (1.0 - fraction)
                    }
                }
                None => match preview.native_opts.mount_level {
                    Some(level) => mount_cost(level)?,
                    None => raw_cost,
                },
            };
            native_now = native_now.saturating_add(cost_now as u64);
            native_after = native_after.saturating_add(preview.native.disk_cost(raw, compressed));
        }

        let raw = sample.len() as u32;
        let compressed = zstd::bulk::compress(sample, preview.maximum_opts.level)?.len() as u32;
        maximum_after = maximum_after.saturating_add(preview.maximum.disk_cost(raw, compressed));
        sampled_in = sampled_in.saturating_add(read as u64);
    }

    let mut native_estimate = FileEstimate {
        size,
        sampled: sampled_in,
        inspection,
        ..FileEstimate::default()
    };
    let mut maximum_estimate = native_estimate;
    if sampled_in == 0 {
        native_estimate.disk_now = size;
        native_estimate.disk_after = size;
        maximum_estimate.disk_now = size;
        maximum_estimate.disk_after = size;
        return Ok((native_estimate, maximum_estimate));
    }

    let scale = size as f64 / sampled_in as f64;
    native_estimate.disk_now = (native_now as f64 * scale) as u64;
    native_estimate.disk_after = (native_after as f64 * scale) as u64;
    maximum_estimate.disk_now = native_estimate.disk_now;
    maximum_estimate.disk_after = (maximum_after as f64 * scale) as u64;
    let residency = Residency::of(file, size);
    residency.price(&mut native_estimate);
    residency.price(&mut maximum_estimate);
    if preview.native.skips_nocow_files() {
        residency.skip_nocow(&mut native_estimate);
    }
    Ok((native_estimate, maximum_estimate))
}

/// The options for one file, pricing its compressed share at the level an
/// earlier pass recorded for it.
///
/// A recorded level of [`NOT_ATTEMPTED`] or below leaves `opts` as given.
fn opts_for_recorded(opts: &EstimateOpts, recorded: Option<i32>) -> EstimateOpts {
    match recorded.filter(|level| *level > NOT_ATTEMPTED) {
        Some(level) => EstimateOpts {
            mount_level: Some(level),
            ..*opts
        },
        None => *opts,
    }
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
    estimate_game_budgeted(
        install_dir,
        inv,
        model,
        opts,
        probe,
        cancel,
        MAX_SAMPLE_BYTES,
    )
}

/// What the estimate does with one candidate file.
enum Plan {
    /// Sample it, pricing its compressed share at the recorded level if any.
    Sample(Option<i32>),
    /// An earlier pass at or above the plan's floor already tried it.
    AlreadyTried,
    /// The sampling budget was spent before this file.
    OutOfBudget,
}

/// [`estimate_game_cancellable`] with the sampling budget as a parameter.
fn estimate_game_budgeted(
    install_dir: &Path,
    inv: &Inventory,
    model: &dyn UnitModel,
    opts: &EstimateOpts,
    probe: &dyn DiskProbe,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    sample_bytes: u64,
) -> Estimate {
    let cancelled = || cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed));
    let mut candidates: Vec<_> = inv.to_compress().collect();
    // Largest first, so the budget is spent where most of the bytes are and
    // whatever goes unsampled is the small remainder.
    candidates.sort_by_key(|entry| std::cmp::Reverse(entry.size));
    // The sampled set is chosen here, in order, before any thread starts. A
    // shared budget spent by whichever worker got there first made the set
    // depend on scheduling. Each file is charged the most it can read.
    let mut remaining = sample_bytes;
    let per_file_cap = u64::from(BtrfsModel::BLOCK) * MAX_BLOCKS_PER_FILE;
    let plans: Vec<Plan> = candidates
        .iter()
        .map(|entry| {
            // A pass at this level or higher has already had its chance at
            // this file; whatever it left uncompressed, it left uncompressed
            // for a reason. Claiming a further saving here is how the
            // estimator used to promise space that a rerun could not deliver.
            let recorded = probe.attempted_level(&entry.path(install_dir));
            if recorded.is_some_and(|applied| applied >= opts.floor.unwrap_or(opts.level)) {
                Plan::AlreadyTried
            } else if remaining == 0 {
                Plan::OutOfBudget
            } else {
                remaining = remaining.saturating_sub(entry.size.min(per_file_cap));
                Plan::Sample(recorded)
            }
        })
        .collect();
    let results: Vec<(FileEstimate, bool)> = candidates
        .par_iter()
        .zip(plans.par_iter())
        .map(|(entry, plan)| {
            let unsampled = || {
                (
                    FileEstimate {
                        size: entry.size,
                        ..FileEstimate::default()
                    },
                    false,
                )
            };
            let recorded = match plan {
                _ if cancelled() => return unsampled(),
                Plan::OutOfBudget => return unsampled(),
                Plan::AlreadyTried => {
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
                Plan::Sample(recorded) => *recorded,
            };
            let path = entry.path(install_dir);
            let measured = probe.measure(&path).and_then(|(compressed, mapped)| {
                (mapped > 0).then(|| compressed as f64 / mapped as f64)
            });
            let per_file = opts_for_recorded(opts, recorded);
            match estimate_file_with(&path, entry.size, model, &per_file, measured) {
                Ok(est) => {
                    let worth = est.worthwhile();
                    (est, worth)
                }
                // An unreadable file is simply not a candidate.
                Err(_) => unsampled(),
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
    // Sizes of the files that were sampled and of those the budget did not
    // reach, for scaling the result up afterwards.
    let mut sampled_size = 0u64;
    let mut unsampled_size = 0u64;
    for (est, worthwhile) in results {
        out.sampled = out.sampled.saturating_add(est.sampled);
        out.inspected_files += u64::from(est.sampled > 0);
        if est.sampled > 0 {
            out.format_evidence.observe(est.inspection);
            sampled_size = sampled_size.saturating_add(est.size);
        }
        if est.sampled == 0 && est.disk_now == 0 && est.disk_after == 0 {
            out.unsampled_files += 1;
            unsampled_size = unsampled_size.saturating_add(est.size);
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
    // The totals so far cover only the sampled files. Leaving the rest out
    // reported about a quarter of what a pass freed on a 6,700-file game, so
    // the unsampled bytes are assumed to behave like the sampled ones. A
    // cancelled estimate stays as measured.
    if unsampled_size > 0 && sampled_size > 0 && !cancelled() {
        let scale = unsampled_size as f64 / sampled_size as f64;
        out.bytes = out.bytes.saturating_add((out.bytes as f64 * scale) as u64);
        out.disk_now = out
            .disk_now
            .saturating_add((out.disk_now as f64 * scale) as u64);
        out.disk_after = out
            .disk_after
            .saturating_add((out.disk_after as f64 * scale) as u64);
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
        blocks.saturating_sub(1) / 2
    } else {
        index.saturating_mul(blocks.saturating_sub(1)) / (samples - 1)
    }
}

/// The share of `saving` that `done` of `planned` bytes account for.
pub fn scaled_saving(saving: u64, done: u64, planned: u64) -> u64 {
    if planned == 0 || done >= planned {
        return saving;
    }
    let scaled = u128::from(saving) * u128::from(done) / u128::from(planned);
    u64::try_from(scaled).unwrap_or(saving)
}

/// Start of sample window `index` of `samples`, each `window` bytes, in a file
/// of `size` bytes.
///
/// The file is cut into `samples` equal strata and each window is centred in
/// its stratum, so head and tail carry no more weight than any other part.
/// The result keeps the window inside the file.
pub fn sample_offset(index: u64, size: u64, window: u64, samples: u64) -> u64 {
    let last_start = size.saturating_sub(window);
    let samples = samples.max(1);
    let stratum = size / samples;
    let start = index
        .saturating_mul(stratum)
        .saturating_add(stratum.saturating_sub(window) / 2);
    start.min(last_start)
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
        check_eq(sample_block(0, 1, 1), 0, "single-block file")?;
        check_eq(sample_block(0, 9, 1), 4, "one sample uses the middle")
    }

    /// Saving fraction of a file under the pair estimator with a 1 MiB budget.
    fn paired_saving_ratio(bytes: Vec<u8>) -> Result<f64, String> {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let path = tmp.path().join("body.pak");
        let size = write_file(&path, bytes)?;
        let file = std::fs::File::open(&path).ctx("open body.pak")?;
        let opts = EstimateOpts {
            level: 3,
            mount_level: None,
            floor: None,
        };
        let (native, _) = estimate_open_file_pair(
            &file,
            size,
            PreviewEstimate {
                native: &BtrfsModel { level: 3 },
                native_opts: &opts,
                measured: None,
                maximum: &PackModel { level: 3 },
                maximum_opts: &opts,
                byte_cap: 1024 * 1024,
            },
        )
        .ctx("estimate body.pak")?;
        check(native.sampled > 0, "something was sampled")?;
        Ok(native.saving() as f64 / native.disk_now.max(1) as f64)
    }

    fn shaped(head: &[u8], body: &[u8], tail: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(head);
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(tail);
        bytes
    }

    #[test]
    fn sampling_sees_the_body_not_only_the_head_and_tail() -> TestResult {
        let edge = vec![0u8; 512 * 1024];
        let mib = 1024 * 1024;
        // Controls: a file that is all zeros must save, all noise must not.
        check(
            paired_saving_ratio(vec![0u8; 8 * mib])? > 0.9,
            "control: zeros compress",
        )?;
        check(
            paired_saving_ratio(noise(8 * mib))? < 0.05,
            "control: noise does not",
        )?;
        let packed = shaped(&edge, &noise(7 * mib), &edge);
        let ratio = paired_saving_ratio(packed)?;
        check(
            ratio < 0.3,
            format!("compressible edges around noise saved {ratio:.3} of the file"),
        )?;
        let loose = shaped(&noise(512 * 1024), &vec![0u8; 7 * mib], &noise(512 * 1024));
        let ratio = paired_saving_ratio(loose)?;
        check(
            ratio > 0.7,
            format!("noise edges around a compressible body saved {ratio:.3}"),
        )
    }

    #[test]
    fn sample_windows_stay_inside_the_file_and_do_not_overlap() -> TestResult {
        let size = 9 * 1024 * 1024;
        let window = 128 * 1024;
        let mut last_end = 0;
        for index in 0..8 {
            let start = sample_offset(index, size, window, 8);
            check(start >= last_end, "windows do not overlap")?;
            check(start + window <= size, "a window ends inside the file")?;
            last_end = start + window;
        }
        check(
            sample_offset(7, size, window, 8) > size / 2,
            "the last window is in the back half",
        )
    }

    #[test]
    fn paired_estimate_reads_once_and_scores_both_backends() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let path = tmp.path().join("mixed.dat");
        let size = write_file(&path, "the quick brown fox ".repeat(400_000).into_bytes())?;
        let file = std::fs::File::open(&path).ctx("open mixed.dat")?;
        let native_opts = EstimateOpts {
            level: 9,
            mount_level: None,
            floor: None,
        };
        let maximum_opts = EstimateOpts {
            level: 19,
            mount_level: None,
            floor: None,
        };
        let (native, maximum) = estimate_open_file_pair(
            &file,
            size,
            PreviewEstimate {
                native: &BtrfsModel { level: 9 },
                native_opts: &native_opts,
                measured: None,
                maximum: &PackModel { level: 19 },
                maximum_opts: &maximum_opts,
                byte_cap: 1024 * 1024,
            },
        )
        .ctx("estimate both backends")?;
        check_eq(
            native.sampled,
            maximum.sampled,
            "the projections share their reads",
        )?;
        check_eq(native.sampled, 1024 * 1024, "the file cap is exact")?;
        check(native.worthwhile(), format!("native estimate: {native:?}"))?;
        check(
            maximum.disk_after <= native.disk_after,
            format!("native {native:?}, maximum {maximum:?}"),
        )?;

        let small_path = tmp.path().join("small.dat");
        let small_size = write_file(&small_path, vec![b'a'; 700 * 1024])?;
        let small_file = std::fs::File::open(&small_path).ctx("open small.dat")?;
        let (small, _) = estimate_open_file_pair(
            &small_file,
            small_size,
            PreviewEstimate {
                native: &BtrfsModel { level: 9 },
                native_opts: &native_opts,
                measured: None,
                maximum: &PackModel { level: 19 },
                maximum_opts: &maximum_opts,
                byte_cap: 1024 * 1024,
            },
        )
        .ctx("estimate a file smaller than the cap")?;
        check_eq(
            small.sampled,
            small_size,
            "a small file is not sampled twice",
        )
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
            floor: None,
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

    /// A scratch folder beside the sources, which sits on the machine's real
    /// disk rather than a temporary filesystem.
    fn local_dir() -> Result<tempfile::TempDir, String> {
        tempfile::tempdir_in(std::env::current_dir().ctx("working directory")?)
            .ctx("scratch folder")
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_sparse_file_saves_no_more_than_it_has_allocated() -> TestResult {
        use std::os::unix::fs::MetadataExt;
        let tmp = local_dir()?;
        let model = BtrfsModel { level: 3 };
        let opts = EstimateOpts {
            level: 3,
            mount_level: None,
            floor: None,
        };
        let logical = 1024u64 * 1024 * 1024;
        let sparse = tmp.path().join("sparse.bin");
        let mut file = std::fs::File::create(&sparse).ctx("create the sparse file")?;
        file.set_len(logical).ctx("extend the file")?;
        io::Write::write_all(&mut file, &noise(1024 * 1024)).ctx("write the data")?;
        file.sync_all().ctx("flush the data")?;
        drop(file);
        let allocated = std::fs::metadata(&sparse).ctx("stat")?.blocks() * 512;
        check(
            allocated < logical / 100,
            format!("control: the file must be sparse here, but {allocated} bytes are allocated"),
        )?;
        let est = estimate_file(&sparse, logical, &model, &opts).ctx("estimate the sparse file")?;
        check(
            est.saving() <= allocated,
            format!(
                "saving {} exceeds the {allocated} bytes allocated: {est:?}",
                est.saving()
            ),
        )?;
        check(
            est.disk_now <= allocated,
            format!("disk_now {} exceeds the {allocated} allocated", est.disk_now),
        )?;
        let handle = std::fs::File::open(&sparse).ctx("reopen")?;
        let preview = PreviewEstimate {
            native: &model,
            native_opts: &opts,
            measured: None,
            maximum: &PackModel { level: 3 },
            maximum_opts: &opts,
            byte_cap: 4 * 1024 * 1024,
        };
        let (native, maximum) =
            estimate_open_file_pair(&handle, logical, preview).ctx("paired estimate")?;
        check(
            native.saving() <= allocated && maximum.saving() <= allocated,
            format!("paired savings exceed the allocation: {native:?} {maximum:?}"),
        )?;
        let dense = tmp.path().join("dense.bin");
        let size = write_file(&dense, vec![0u8; 4 * 1024 * 1024])?;
        let est = estimate_file(&dense, size, &model, &opts).ctx("estimate the dense file")?;
        check(
            est.saving() > 3 * 1024 * 1024,
            format!("control: a dense file of zeros still saves a lot: {est:?}"),
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_nocow_file_is_given_no_saving_on_btrfs() -> TestResult {
        let tmp = local_dir()?;
        let fs = crate::fsprobe::probe(tmp.path()).ctx("probe the scratch folder")?;
        if fs.fstype != "btrfs" {
            check(
                std::env::var_os("FLUMMOX_REQUIRE_BTRFS").is_none(),
                format!(
                    "the scratch folder is {}, not btrfs, and FLUMMOX_REQUIRE_BTRFS is set",
                    fs.fstype
                ),
            )?;
            eprintln!("skipped: the scratch folder is not btrfs");
            return Ok(());
        }
        let model = BtrfsModel { level: 3 };
        let opts = EstimateOpts {
            level: 3,
            mount_level: None,
            floor: None,
        };
        let plain = tmp.path().join("plain.bin");
        let cow = tmp.path().join("nocow.bin");
        std::fs::File::create(&cow).ctx("create the empty file")?;
        let status = std::process::Command::new("chattr")
            .arg("+C")
            .arg(&cow)
            .status()
            .ctx("run chattr")?;
        check(status.success(), format!("chattr +C failed: {status}"))?;
        for path in [&plain, &cow] {
            std::fs::write(path, vec![0u8; 4 * 1024 * 1024]).ctx("write data")?;
        }
        let size = 4 * 1024 * 1024;
        let normal = estimate_file(&plain, size, &model, &opts).ctx("estimate plain")?;
        check(
            normal.saving() > 1024 * 1024,
            format!("control: the ordinary file saves space: {normal:?}"),
        )?;
        let marked = estimate_file(&cow, size, &model, &opts).ctx("estimate nocow")?;
        check_eq(marked.saving(), 0, "a no-copy-on-write file saves nothing")?;
        let handle = std::fs::File::open(&cow).ctx("reopen")?;
        let preview = PreviewEstimate {
            native: &model,
            native_opts: &opts,
            measured: None,
            maximum: &PackModel { level: 3 },
            maximum_opts: &opts,
            byte_cap: 8 * 1024 * 1024,
        };
        let (native, maximum) =
            estimate_open_file_pair(&handle, size, preview).ctx("paired estimate")?;
        check_eq(native.saving(), 0, "the paired Standard estimate is zero too")?;
        check(
            maximum.saving() > 0,
            "the pack tier is not affected by the attribute",
        )
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
            floor: None,
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
                floor: None,
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
                floor: None,
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
    fn files_beyond_the_sampling_budget_are_scaled_in() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let dir = tmp.path();
        for index in 0..6 {
            std::fs::write(
                dir.join(format!("part-{index}.dat")),
                vec![b'a'; 1024 * 1024],
            )
            .ctx("write a part")?;
        }
        let inv = inventory::walk(dir, &inventory::WalkOpts::default()).ctx("walk the game dir")?;
        let model = BtrfsModel { level: 9 };
        let opts = EstimateOpts {
            level: 9,
            mount_level: None,
            floor: None,
        };
        let whole = estimate_game_budgeted(dir, &inv, &model, &opts, &NoProbe, None, u64::MAX);
        check_eq(whole.unsampled_files, 0, "control: everything sampled")?;
        // A single thread makes the number of files the budget reaches exact.
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .ctx("pool")?;
        let short = pool.install(|| {
            estimate_game_budgeted(dir, &inv, &model, &opts, &NoProbe, None, 2 * 1024 * 1024)
        });
        check_eq(short.unsampled_files, 4, "the budget reached two files")?;
        check_eq(
            short.saving(),
            whole.saving(),
            "six equal files save the same whether two or six were sampled",
        )
    }

    /// A probe that reports one recorded level, and optionally that every
    /// file is already stored compressed.
    struct Recorded {
        level: Option<i32>,
        all_compressed: bool,
    }

    impl DiskProbe for Recorded {
        fn measure(&self, _path: &Path) -> Option<(u64, u64)> {
            self.all_compressed.then_some((1000, 1000))
        }

        fn attempted_level(&self, _path: &Path) -> Option<i32> {
            self.level
        }
    }

    /// Words drawn from a 2048-word vocabulary: compressible, and by an
    /// amount that moves with the level.
    fn wordy(len: usize) -> Vec<u8> {
        let letters = noise(2048 * 8);
        let vocabulary: Vec<&[u8]> = letters.chunks(8).collect();
        let picks = noise(len);
        let mut out = Vec::with_capacity(len + 16);
        for pick in picks.chunks(2) {
            let index = pick
                .iter()
                .fold(0usize, |acc, byte| (acc << 8) | usize::from(*byte))
                % 2048;
            let word = vocabulary.get(index).copied().unwrap_or_default();
            out.extend(word.iter().map(|byte| b'a' + byte % 26));
            out.push(b' ');
            if out.len() >= len {
                break;
            }
        }
        out
    }

    fn one_game(dir: &Path, bytes: Vec<u8>) -> Result<Inventory, String> {
        std::fs::write(dir.join("data.dat"), bytes).ctx("write data.dat")?;
        inventory::walk(dir, &inventory::WalkOpts::default()).ctx("walk the game dir")
    }

    #[test]
    fn a_file_recorded_at_the_plan_floor_is_not_offered_again() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let inv = one_game(tmp.path(), wordy(2 * 1024 * 1024))?;
        let model = BtrfsModel { level: 15 };
        let max = EstimateOpts {
            level: 15,
            mount_level: None,
            floor: Some(9),
        };
        let probe = |level| Recorded {
            level,
            all_compressed: false,
        };
        let fresh = estimate_game_with(tmp.path(), &inv, &model, &max, &probe(None));
        check(
            fresh.files == 1 && fresh.saving() > 0,
            format!("control: an unrecorded file is offered: {fresh:?}"),
        )?;
        let after_max = estimate_game_with(tmp.path(), &inv, &model, &max, &probe(Some(9)));
        check_eq(
            after_max.files,
            0,
            "a level-9 file has had its chance under a 9 to 15 plan",
        )?;
        let below = estimate_game_with(tmp.path(), &inv, &model, &max, &probe(Some(3)));
        check_eq(below.files, 1, "a level-3 file is still offered")
    }

    #[test]
    fn already_compressed_data_is_priced_at_its_recorded_level() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let path = tmp.path().join("data.dat");
        let size = write_file(&path, wordy(2 * 1024 * 1024))?;
        let model = BtrfsModel { level: 19 };
        let opts = EstimateOpts {
            level: 19,
            mount_level: None,
            floor: None,
        };
        let now_at = |recorded: Option<i32>| -> Result<u64, String> {
            let per_file = opts_for_recorded(&opts, recorded);
            estimate_file_with(&path, size, &model, &per_file, Some(1.0))
                .map(|est| est.disk_now)
                .ctx("estimate with a recorded level")
        };
        let (low, high, unknown) = (now_at(Some(1))?, now_at(Some(12))?, now_at(None)?);
        check(low > 0 && high > 0, "control: both were sampled")?;
        check(
            low > high,
            format!("level 1 holds more bytes than level 12: {low} vs {high}"),
        )?;
        check(
            unknown != low || unknown != high,
            "control: an unrecorded file is priced at the default, not at both",
        )?;
        check_eq(
            now_at(Some(NOT_ATTEMPTED))?,
            unknown,
            "a skipped file's zero is not a level",
        )
    }

    #[test]
    fn the_sampled_set_does_not_depend_on_thread_count() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let dir = tmp.path();
        for index in 0..12 {
            std::fs::write(
                dir.join(format!("part-{index:02}.dat")),
                "the quick brown fox ".repeat(50_000 + index * 1000),
            )
            .ctx("write a part")?;
        }
        let inv = inventory::walk(dir, &inventory::WalkOpts::default()).ctx("walk the game dir")?;
        let model = BtrfsModel { level: 9 };
        let opts = EstimateOpts {
            level: 9,
            mount_level: None,
            floor: None,
        };
        let budget = 2 * 1024 * 1024;
        let mut seen = Vec::new();
        for threads in [1, 2, 8, 8, 8] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .ctx("pool")?;
            seen.push(pool.install(|| {
                estimate_game_budgeted(dir, &inv, &model, &opts, &NoProbe, None, budget)
            }));
        }
        let first = seen.first().ctx("one result")?;
        check(first.inspected_files > 0, "control: something was sampled")?;
        check(first.unsampled_files > 0, "control: the budget bound")?;
        for other in &seen {
            check_eq(*other, *first, "every thread count gives the same estimate")?;
        }
        Ok(())
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
                floor: None,
            },
        );
        check_eq(est.files, 1, "one worthwhile file")?;
        check_eq(est.skipped_files, 1, "the tiny file is skipped")?;
        check(est.saving_ratio() > 0.9, format!("{est:?}"))
    }
}
