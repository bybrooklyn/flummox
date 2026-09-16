//! Compression backends.
//!
//! A backend knows how to make a directory's files take less space and how to
//! put them back. Everything above this layer works in terms of the trait, so
//! the CLI, daemon and GUI never learn which filesystem is involved.

pub mod btrfs;

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::estimate::{BtrfsModel, UnitModel};
use crate::fsprobe::BackendKind;
use crate::inventory::{Inventory, WalkOpts};

/// How hard to compress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Quick pass, small gain.
    Fast,
    /// The default.
    Balanced,
    /// Slowest, best ratio.
    Max,
}

impl Preset {
    /// The zstd level this preset uses on a native filesystem.
    ///
    /// btrfs caps its window at the 128 KiB block it compresses, so levels
    /// above 15 cost time without buying ratio.
    pub fn btrfs_level(self) -> i32 {
        match self {
            Self::Fast => 3,
            Self::Balanced => 9,
            Self::Max => 15,
        }
    }

    /// The name used on the command line and in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Max => "max",
        }
    }
}

/// Settings for one compression job.
#[derive(Debug, Clone, Copy)]
pub struct CompressOpts {
    /// The preset, unless `level` overrides it.
    pub preset: Preset,
    /// An explicit zstd level.
    pub level: Option<i32>,
    /// How many files to work on at once.
    pub threads: usize,
}

impl Default for CompressOpts {
    fn default() -> Self {
        Self { preset: Preset::Balanced, level: None, threads: 2 }
    }
}

impl CompressOpts {
    /// The level this job will use on btrfs.
    pub fn btrfs_level(&self) -> i32 {
        self.level.unwrap_or_else(|| self.preset.btrfs_level())
    }
}

/// Progress reported while a job runs.
#[derive(Debug, Clone)]
pub enum Event {
    /// The job started, with the number of files and bytes it will touch.
    Started {
        /// Files to process.
        files: u64,
        /// Bytes to process.
        bytes: u64,
    },
    /// A file finished.
    Progress {
        /// Files done so far.
        files_done: u64,
        /// Bytes done so far.
        bytes_done: u64,
        /// The file just finished, relative to the install directory.
        current: String,
    },
    /// Something went wrong that did not stop the job.
    Warning(String),
    /// The job ended.
    Finished(Box<Outcome>),
}

/// Where progress events go.
pub trait EventSink: Sync {
    /// Handles one event.
    fn event(&self, event: Event);
}

/// An event sink that discards everything, for tests and one-shot calls.
pub struct NullSink;

impl EventSink for NullSink {
    fn event(&self, _event: Event) {}
}

/// Per-job state a backend needs: where to report, and when to stop.
pub struct JobCtx<'a> {
    /// Progress destination.
    pub events: &'a dyn EventSink,
    /// Set to stop the job at the next file boundary.
    pub cancel: &'a AtomicBool,
}

impl JobCtx<'_> {
    /// Whether the job has been asked to stop.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// What a finished job did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Outcome {
    /// Files rewritten.
    pub files: u64,
    /// Their total size.
    pub bytes: u64,
    /// Files skipped, including ones that were already compressed.
    pub skipped: u64,
    /// Free space on the filesystem before the job, if it could be read.
    ///
    /// `None` when `statvfs` failed, which happens when a drive is removed
    /// mid-job. Treating that as zero made the tool report freeing the entire
    /// disk.
    pub free_before: Option<u64>,
    /// Free space after, on the same terms.
    pub free_after: Option<u64>,
    /// The compression level the kernel actually applied.
    ///
    /// Lower than the level asked for when the kernel is too old to accept
    /// one. Recording the requested level instead tells the estimator a file
    /// is finished at 15 when it is sitting at the mount default, and it then
    /// refuses to offer the saving that is still available.
    pub effective_level: Option<i32>,
    /// Whether the job stopped early because it was cancelled.
    pub cancelled: bool,
    /// Per-file failures, as messages.
    pub errors: Vec<String>,
}

impl Outcome {
    /// Bytes freed, measured from the filesystem's free space.
    ///
    /// Anything else writing to the same filesystem during the job shows up
    /// here too, which is why the UI labels this figure approximate.
    pub fn freed(&self) -> Option<i64> {
        let before = i64::try_from(self.free_before?).ok()?;
        let after = i64::try_from(self.free_after?).ok()?;
        Some(after - before)
    }
}

/// How compressed a directory currently is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct CompressionStatus {
    /// Bytes held in compressed extents.
    pub compressed_bytes: u64,
    /// Bytes examined in total.
    pub total_bytes: u64,
    /// Files examined.
    pub files: u64,
}

impl CompressionStatus {
    /// The fraction of data held compressed, 0.0 to 1.0.
    pub fn ratio(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        self.compressed_bytes as f64 / self.total_bytes as f64
    }
}

/// A way of compressing a directory in place.
pub trait Backend: Sync {
    /// Which backend this is.
    fn kind(&self) -> BackendKind;

    /// The arithmetic the estimator must use to match this backend.
    fn model(&self, opts: &CompressOpts) -> Box<dyn UnitModel>;

    /// How this backend measures what a file currently costs on disk.
    ///
    /// Defaults to measuring nothing, which makes estimates fall back to
    /// reasoning from the mount options.
    fn disk_probe(&self) -> Box<dyn crate::estimate::DiskProbe> {
        Box::new(crate::estimate::NoProbe)
    }

    /// How this backend wants the install directory walked.
    ///
    /// The size floor differs sharply: a pack store spends an object per
    /// file, so small files are not worth it, while a filesystem that
    /// compresses in place charges nothing per file and should take almost
    /// everything.
    fn walk_opts(&self) -> WalkOpts;

    /// Compresses every file the inventory selected.
    fn compress(
        &self,
        install_dir: &Path,
        inv: &Inventory,
        opts: &CompressOpts,
        ctx: &JobCtx<'_>,
    ) -> io::Result<Outcome>;

    /// Puts the directory back to uncompressed storage.
    fn decompress(
        &self,
        install_dir: &Path,
        inv: &Inventory,
        ctx: &JobCtx<'_>,
    ) -> io::Result<Outcome>;

    /// Measures how compressed the directory is now.
    fn status(&self, install_dir: &Path, inv: &Inventory) -> io::Result<CompressionStatus>;
}

/// Builds the backend for a filesystem kind.
pub fn for_kind(kind: BackendKind) -> Option<Box<dyn Backend>> {
    match kind {
        BackendKind::Btrfs => Some(Box::new(btrfs::BtrfsBackend)),
        // Not implemented yet; the pack tier lands with the FUSE layer.
        BackendKind::Bcachefs | BackendKind::Pack => None,
    }
}

/// Free bytes on the filesystem holding `path`.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path)
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    Ok(stat.blocks_available() as u64 * stat.fragment_size() as u64)
}

/// The estimator model matching a backend and its options.
pub fn model_for(kind: BackendKind, opts: &CompressOpts) -> Box<dyn UnitModel> {
    match kind {
        BackendKind::Btrfs | BackendKind::Bcachefs => {
            Box::new(BtrfsModel { level: opts.btrfs_level() })
        }
        BackendKind::Pack => Box::new(BtrfsModel { level: opts.btrfs_level() }),
    }
}

#[cfg(test)]
mod tests {
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    use super::*;

    #[test]
    fn presets_map_to_levels() -> TestResult {
        check_eq(Preset::Fast.btrfs_level(), 3, "the fast preset's level")?;
        check_eq(Preset::Max.btrfs_level(), 15, "the max preset's level")?;
        let opts = CompressOpts { preset: Preset::Fast, level: Some(12), ..CompressOpts::default() };
        check_eq(opts.btrfs_level(), 12, "an explicit level overrides the preset")
    }

    #[test]
    fn free_space_is_readable_here() -> TestResult {
        check(free_bytes(Path::new(".")).ctx("read free space")? > 0, "some space is free here")
    }

    #[test]
    fn status_ratio_handles_empty_directories() -> TestResult {
        check_eq(CompressionStatus::default().ratio(), 0.0, "an empty status has no ratio")?;
        let s = CompressionStatus { compressed_bytes: 50, total_bytes: 200, files: 1 };
        check((s.ratio() - 0.25).abs() < f64::EPSILON, "50 of 200 bytes is a quarter")
    }
}
