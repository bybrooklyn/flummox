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
    /// Waiting because the game was launched.
    Paused {
        /// The process holding the game open.
        by: String,
    },
    /// The game closed, so work continues.
    Resumed,
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
    /// Asked between files: is something using the game right now?
    ///
    /// `None` means never pause. The backend knows nothing about Steam or
    /// `/proc`; it only knows to wait while this says someone is playing.
    pub busy: Option<&'a dyn BusyCheck>,
}

/// Answers whether the game is in use, so a job can wait instead of competing
/// with it.
///
/// Compression is heavy on a disk. Running it while someone is loading a level
/// is the wrong time, and refusing to start at all is the wrong answer for a
/// job that may run for minutes.
pub trait BusyCheck: Sync {
    /// Who is using the game, or `None` when nothing is.
    fn in_use_by(&self) -> Option<String>;
}

impl JobCtx<'_> {
    /// Whether the job has been asked to stop.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Blocks while the game is in use, returning when it is free.
    ///
    /// Returns `false` if the job was cancelled while waiting, so the caller
    /// stops rather than resuming. Checking costs a `/proc` scan, so it is
    /// rate limited: a scan per file would cost more than the compression on
    /// a library with hundreds of thousands of files.
    pub fn wait_while_busy(&self, last_check: &std::sync::Mutex<std::time::Instant>) -> bool {
        const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
        const POLL: std::time::Duration = std::time::Duration::from_millis(500);

        let Some(busy) = self.busy else { return !self.cancelled() };
        match last_check.lock() {
            Ok(mut at) if at.elapsed() >= CHECK_EVERY => *at = std::time::Instant::now(),
            Ok(_) => return !self.cancelled(),
            Err(_) => return !self.cancelled(),
        }

        let mut announced = false;
        while let Some(who) = busy.in_use_by() {
            if self.cancelled() {
                return false;
            }
            if !announced {
                self.events.event(Event::Paused { by: who });
                announced = true;
            }
            std::thread::sleep(POLL);
        }
        if announced {
            self.events.event(Event::Resumed);
        }
        !self.cancelled()
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
mod pause_tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::{Duration, Instant};

    use crate::testutil::{TestResult, check, check_eq};

    use super::*;

    /// Reports busy for the first `busy_for` calls, then free.
    struct FakeGame {
        calls: AtomicUsize,
        busy_for: usize,
    }

    impl BusyCheck for FakeGame {
        fn in_use_by(&self) -> Option<String> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            (n < self.busy_for).then(|| "Portal 2 (pid 1234)".to_owned())
        }
    }

    /// Records the events a job emitted, so a test can assert on them.
    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<String>>,
    }

    impl EventSink for Recorder {
        fn event(&self, event: Event) {
            let name = match event {
                Event::Paused { by } => format!("paused by {by}"),
                Event::Resumed => "resumed".to_owned(),
                Event::Started { .. } => "started".to_owned(),
                Event::Progress { .. } => "progress".to_owned(),
                Event::Warning(m) => format!("warning {m}"),
                Event::Finished(_) => "finished".to_owned(),
            };
            if let Ok(mut events) = self.events.lock() {
                events.push(name);
            }
        }
    }

    /// Far enough in the past that the rate limit never suppresses a check.
    fn due() -> Mutex<Instant> {
        Mutex::new(Instant::now() - Duration::from_secs(60))
    }

    #[test]
    fn waits_until_the_game_closes_then_says_so() -> TestResult {
        let cancel = AtomicBool::new(false);
        let events = Recorder::default();
        // Busy for two polls, which at 500ms each means it blocks about a
        // second before the game "closes".
        let game = FakeGame { calls: AtomicUsize::new(0), busy_for: 2 };
        let ctx = JobCtx { events: &events, cancel: &cancel, busy: Some(&game) };

        let started = Instant::now();
        let carry_on = ctx.wait_while_busy(&due());
        let waited = started.elapsed();

        check(carry_on, "the job should continue once the game closes")?;
        check(
            waited >= Duration::from_millis(400),
            format!("expected it to block while busy, waited {waited:?}"),
        )?;
        let seen = events.events.lock().map_err(|e| e.to_string())?.clone();
        check_eq(
            seen,
            vec!["paused by Portal 2 (pid 1234)".to_owned(), "resumed".to_owned()],
            "it announces the pause once and the resume once",
        )
    }

    #[test]
    fn a_free_game_does_not_pause_or_announce() -> TestResult {
        let cancel = AtomicBool::new(false);
        let events = Recorder::default();
        let game = FakeGame { calls: AtomicUsize::new(0), busy_for: 0 };
        let ctx = JobCtx { events: &events, cancel: &cancel, busy: Some(&game) };

        check(ctx.wait_while_busy(&due()), "a free game continues immediately")?;
        let seen = events.events.lock().map_err(|e| e.to_string())?.len();
        check_eq(seen, 0, "nothing to announce when nothing was waiting")
    }

    #[test]
    fn cancelling_during_a_pause_stops_the_job() -> TestResult {
        let cancel = AtomicBool::new(true);
        let events = Recorder::default();
        // Busy forever: only the cancel can end this wait.
        let game = FakeGame { calls: AtomicUsize::new(0), busy_for: usize::MAX };
        let ctx = JobCtx { events: &events, cancel: &cancel, busy: Some(&game) };

        check(
            !ctx.wait_while_busy(&due()),
            "Ctrl-C during a pause must stop the job, not resume it",
        )
    }

    #[test]
    fn without_a_check_it_never_waits() -> TestResult {
        let cancel = AtomicBool::new(false);
        let events = Recorder::default();
        let ctx = JobCtx { events: &events, cancel: &cancel, busy: None };

        check(ctx.wait_while_busy(&due()), "no check means no pausing")?;
        let seen = events.events.lock().map_err(|e| e.to_string())?.len();
        check_eq(seen, 0, "and nothing announced")
    }

    #[test]
    fn checks_are_rate_limited() -> TestResult {
        let cancel = AtomicBool::new(false);
        let events = Recorder::default();
        let game = FakeGame { calls: AtomicUsize::new(0), busy_for: usize::MAX };
        let ctx = JobCtx { events: &events, cancel: &cancel, busy: Some(&game) };

        // Checked a moment ago, so this call must skip the scan entirely. A
        // scan per file would cost more than the compression on a library
        // with hundreds of thousands of files.
        let recent = Mutex::new(Instant::now());
        check(ctx.wait_while_busy(&recent), "a recent check means carry on")?;
        check_eq(
            game.calls.load(Ordering::Relaxed),
            0,
            "the busy check must not have been consulted at all",
        )
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
