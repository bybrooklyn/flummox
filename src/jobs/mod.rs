//! Persistent jobs shared by the window and command line.
//!
//! A coordinator owns workers and durable outcomes. Closing a client never
//! transfers ownership or terminates a worker.

mod autostart;
mod client;
mod packs;
mod service;
mod worker;

use crate::{
    backend::{CompressOpts, Event},
    estimate::Estimate,
    model::Game,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use client::{configured_libraries, request, state_dir};

/// Protocol version. A mismatched installed worker is rejected before work.
pub const VERSION: u32 = 8;

/// Which application palette the desktop shell follows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemePreference {
    #[default]
    System,
    Dark,
    Light,
}

impl ThemePreference {
    /// Name shown to the user; `Display` prints the same text.
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }
}

impl std::fmt::Display for ThemePreference {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// How much interface motion the desktop shell uses.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MotionPreference {
    #[default]
    Expressive,
    Subtle,
    Reduced,
}

impl MotionPreference {
    /// Name shown to the user; `Display` prints the same text.
    pub fn label(self) -> &'static str {
        match self {
            Self::Expressive => "Expressive",
            Self::Subtle => "Subtle",
            Self::Reduced => "Reduced",
        }
    }
}

impl std::fmt::Display for MotionPreference {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Operation requested for a game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    /// Sample files and report an estimate. Rewrites nothing.
    Analyze,
    /// Rewrite files through the native filesystem backend.
    Compress,
    /// Undo native compression.
    Decompress,
    /// Run a Maximum Space task. The job carries it in [`Job::pack`].
    Pack,
}

/// Durable lifecycle, including outcomes that need another attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Waiting for the coordinator to start it.
    Queued,
    /// An analysis worker is running.
    Analyzing,
    /// A worker process or storage thread is running.
    Running,
    /// A pause was sent to the worker, which has not confirmed it yet.
    Pausing,
    /// Stopped at a checkpoint, or a queued job the user paused before it started.
    Paused,
    /// A cancel was requested and the worker has not finished yet.
    Cancelling,
    /// Stopped on request before finishing.
    Cancelled,
    /// Finished with no errors.
    Completed,
    /// Finished, but some files reported errors.
    Partial,
    /// The coordinator restarted or lost contact with the worker mid-job.
    Interrupted,
    /// The worker or storage task returned an error.
    Failed,
}

impl Phase {
    /// Whether a job can still change without a retry request.
    pub fn active(self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::Analyzing
                | Self::Running
                | Self::Pausing
                | Self::Paused
                | Self::Cancelling
        )
    }
    /// Short text used in rows and queue entries.
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Analyzing => "Analyzing",
            Self::Running => "Working",
            Self::Pausing => "Pausing",
            Self::Paused => "Paused",
            Self::Cancelling => "Stopping",
            Self::Cancelled => "Stopped",
            Self::Completed => "Completed",
            Self::Partial => "Needs attention",
            Self::Interrupted => "Interrupted",
            Self::Failed => "Needs attention",
        }
    }
}

/// One operation. Estimates and drive-wide deltas have distinct fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Queue row id. Later requests get larger ids.
    pub id: i64,
    /// The game as last discovered. `install_dir` is canonical.
    pub game: Game,
    pub operation: Operation,
    pub options: CompressOpts,
    pub phase: Phase,
    /// Progress from the worker's latest event.
    pub files_done: u64,
    pub bytes_done: u64,
    /// Totals from the worker's latest `Started` event. Zero until one arrives.
    pub files_total: u64,
    pub bytes_total: u64,
    /// Latest sampled estimate. It is replaced as sampling proceeds.
    pub estimate: Option<Estimate>,
    /// Status line for the user: the current file, a pause reason, or the outcome.
    pub message: String,
    /// Warnings and per-file failures, capped at 20.
    pub errors: Vec<String>,
    /// Unix seconds when the job was queued.
    pub created: u64,
    /// Seconds since the worker or storage thread started, pauses included.
    pub elapsed: u64,
    /// Free bytes on the drive after the job minus before. Other writers on
    /// the same filesystem are included.
    pub drive_change: Option<i64>,
    /// The user asked for the pause. A running game pauses work without setting this.
    pub user_paused: bool,
    /// The storage task when `operation` is [`Operation::Pack`].
    #[serde(default)]
    pub pack: Option<PackTask>,
    /// False once a storage task enters a step that must finish. Pause and
    /// cancel are refused from then on.
    #[serde(default)]
    pub pack_interruptible: bool,
    /// Free-space requirements the client reviewed or the worker computed.
    #[serde(default)]
    pub space_plan: Option<crate::storage::SpacePlan>,
}

/// Durable work for Maximum Space. Paths survive client disconnection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PackTask {
    /// Build a verified store at `store`. The game folder is left as it is.
    Create {
        #[serde(with = "crate::path_serde")]
        store: PathBuf,
    },
    /// Mount `store` at the game's path, keeping the original for rollback.
    Activate {
        #[serde(with = "crate::path_serde")]
        store: PathBuf,
        /// Build the store first when it does not exist yet.
        create: bool,
        /// A report the installed files must still match. `None` skips the check.
        qualification: Option<Box<crate::compatibility::Report>>,
    },
    /// Build a new store from the mounted install and switch to it. The
    /// previous store stays until `Prune`.
    Compact,
    /// Put ordinary files back at the game's path and drop the install record.
    Restore,
    /// Check restored files, then drop the install record. Nothing is deleted.
    VerifyRestored,
    /// Verify the store, then delete the original kept at activation.
    Reclaim,
    /// Delete the previous store and update layer kept by `Compact`.
    Prune,
}

impl PackTask {
    /// Title of the queue entry for this task.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Create { .. } => "Create verified store",
            Self::Activate { create: true, .. } => "Create Maximum Space",
            Self::Activate { .. } => "Activate Maximum Space",
            Self::Compact => "Compact updates",
            Self::Restore => "Restore ordinary files",
            Self::VerifyRestored => "Verify restored files",
            Self::Reclaim => "Reclaim original",
            Self::Prune => "Reclaim previous version",
        }
    }
}

/// How a manually added location maps to games.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FolderKind {
    /// The folder is one game.
    #[default]
    Game,
    /// Each visible subfolder is a game.
    Collection,
}

impl std::fmt::Display for FolderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Game => "Single game",
            Self::Collection => "Games library",
        })
    }
}

/// Library policy. Enabling maintenance starts observing from that moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    #[serde(with = "crate::path_serde")]
    pub path: PathBuf,
    /// Queue compression for new installs and settled updates under `path`.
    pub automatic: bool,
    /// Added by the user. Only custom locations can be removed.
    pub custom: bool,
    /// How a custom location maps to games.
    #[serde(default)]
    pub folder_kind: FolderKind,
}

/// Snapshot returned to clients; no database handle crosses the boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// Coordinator start time in Unix nanoseconds. A new value means a new coordinator.
    #[serde(default)]
    pub worker_epoch: u64,
    /// Requests this coordinator has answered, counting the one that returned this.
    #[serde(default)]
    pub revision: u64,
    /// Oldest first. Finished jobs beyond the newest 300 are dropped.
    pub jobs: Vec<Job>,
    pub libraries: Vec<Library>,
    /// Game ids in `launcher:key` form that no job may touch.
    pub excluded: Vec<String>,
    /// Title of a game with a running process, or the reason processes cannot
    /// be read. No job starts and active jobs pause while this is set.
    pub gaming: Option<String>,
    /// Mirror of `motion == Reduced`, kept for clients that predate `motion`.
    #[serde(default)]
    pub reduced_motion: bool,
    #[serde(default)]
    pub theme: ThemePreference,
    #[serde(default)]
    pub motion: MotionPreference,
    /// Activated Maximum Space installs.
    #[serde(default)]
    pub packs: Vec<crate::pack::Install>,
    /// Games from discovery. A running scan updates this batch by batch.
    #[serde(default)]
    pub discovered: Vec<Game>,
    /// What the running scan is reading. `None` when no scan is running.
    #[serde(default)]
    pub scan_source: Option<String>,
    /// Scans started by this coordinator.
    #[serde(default)]
    pub scan_generation: u64,
    /// Problems reported by the latest scan.
    #[serde(default)]
    pub scan_warnings: Vec<String>,
}

/// Client requests operate on ids, never shell command strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    /// Return the current state and change nothing.
    Snapshot,
    /// Start a discovery scan without waiting for the periodic one. A scan
    /// already running finishes first.
    RefreshDiscovery,
    /// Stop the running scan and discard what it has not yet reported.
    CancelDiscovery,
    /// Restart an idle coordinator after replacing the executable.
    Restart,
    /// Queue an analysis, compression or decompression job. A request that
    /// duplicates an active job for the same folder and operation is ignored.
    Enqueue {
        game: Game,
        operation: Operation,
        options: CompressOpts,
    },
    /// Queue a Maximum Space task. Needs a build with pack mounting.
    EnqueuePack {
        game: Game,
        task: PackTask,
    },
    /// Queue `command` with the space plan the user reviewed. Refused when the
    /// plan no longer fits the drives it names.
    EnqueuePlanned {
        command: Box<Command>,
        plan: crate::storage::SpacePlan,
    },
    /// Pause or resume one active job.
    Pause {
        id: i64,
        paused: bool,
    },
    /// Stop one job by id.
    Cancel(i64),
    /// Queue a new job with the parameters of a finished one.
    Retry(i64),
    /// Add a library policy, replacing any policy for the same path.
    Library(Library),
    /// Remove a custom location that has no active jobs or activated stores.
    RemoveLibrary(#[serde(with = "crate::path_serde")] PathBuf),
    /// Exclude a game id or restore it. Excluding cancels its active jobs.
    Exclude {
        id: String,
        excluded: bool,
    },
    /// Older form of `Motion`: true selects Reduced, false Expressive.
    ReducedMotion(bool),
    Theme(ThemePreference),
    Motion(MotionPreference),
    /// The five `Pack*` commands run a storage transaction inside the request
    /// and are refused while any job is running. `EnqueuePack` queues the same
    /// work as a job.
    PackActivate {
        #[serde(with = "crate::path_serde")]
        game_path: PathBuf,
        #[serde(with = "crate::path_serde")]
        store_path: PathBuf,
        #[serde(with = "crate::path_serde")]
        writes_path: PathBuf,
    },
    PackRollback {
        #[serde(with = "crate::path_serde")]
        game_path: PathBuf,
    },
    PackReclaim {
        #[serde(with = "crate::path_serde")]
        game_path: PathBuf,
    },
    PackCompact {
        #[serde(with = "crate::path_serde")]
        game_path: PathBuf,
    },
    PackPrune {
        #[serde(with = "crate::path_serde")]
        game_path: PathBuf,
    },
}

/// One client message: a single line of JSON on the control socket.
#[derive(Serialize, Deserialize)]
pub(crate) struct Request {
    pub version: u32,
    pub command: Command,
}
/// The coordinator's reply. Exactly one of `snapshot` and `error` is set.
#[derive(Serialize, Deserialize)]
pub(crate) struct Response {
    pub version: u32,
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
}

/// Lines a worker writes to stdout for the coordinator.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum WorkerEvent {
    Progress(Event),
    /// The running estimate. Each one replaces the last.
    Estimate(Estimate),
    SpacePlan(crate::storage::SpacePlan),
    /// The job ended in an orderly way. Nothing follows it.
    Done {
        cancelled: bool,
        errors: Vec<String>,
        drive_change: Option<i64>,
    },
    /// The job could not run or stopped on an error. Nothing follows it.
    Failed(String),
}

/// The first line the coordinator writes to a worker's stdin.
#[derive(Serialize, Deserialize)]
pub(crate) struct Work {
    pub version: u32,
    pub job: Job,
}

/// Error text of [`operation_lock`] when another process kept the lock for
/// the whole wait. The coordinator requeues a job that fails with it.
pub(crate) const LOCK_BUSY: &str = "Another Flummox process is working. Retry when it finishes.";

/// How long [`operation_lock`] waits for the lock before giving up.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Serializes filesystem operations across coordinator workers and legacy CLI jobs.
/// Waits up to ten seconds for a previous holder to finish and exit.
/// Keep the returned handle alive until the operation and recording finish.
pub fn operation_lock() -> anyhow::Result<std::fs::File> {
    lock_in(&state_dir()?, LOCK_WAIT)
}

/// Takes `operation.lock` in `dir`, polling for up to `wait`.
fn lock_in(dir: &std::path::Path, wait: std::time::Duration) -> anyhow::Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("operation.lock"))?;
    let until = std::time::Instant::now() + wait;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) => {
                anyhow::ensure!(std::time::Instant::now() < until, LOCK_BUSY);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

/// Invalidates desktop receipts before a CLI override rewrites a game.
/// The caller must hold [`operation_lock`] until its operation finishes.
pub(crate) fn invalidate_for_cli(path: &std::path::Path) -> anyhow::Result<()> {
    service::invalidate_receipts(
        &state_dir()?.join("queue.sqlite"),
        &path.canonicalize()?,
        None,
    )
}

/// Later lines on a worker's stdin. A closed stdin is treated as `Cancel`.
#[derive(Serialize, Deserialize)]
pub(crate) enum Control {
    Pause(bool),
    Cancel,
}

/// Dispatches private process roles before the public CLI parser runs.
pub fn entrypoint() -> anyhow::Result<bool> {
    match std::env::args().nth(1).as_deref() {
        Some("__coordinator") => {
            service::run()?;
            Ok(true)
        }
        Some("__worker") => {
            worker::run()?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Unix time in seconds, or zero when the clock reads before the epoch.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Refuses roots and application/system configuration directories.
pub fn validate_folder(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let path = path.canonicalize()?;
    anyhow::ensure!(
        path.is_dir() && path.parent().is_some(),
        "Choose a game folder, not an entire drive."
    );
    let home = crate::launchers::Env::current().map(|env| env.home);
    anyhow::ensure!(
        home.as_ref() != Some(&path),
        "Choose a game folder, not your home folder."
    );
    if let Some(home) = &home {
        for private in [".ssh", ".gnupg", ".config", ".cache"] {
            anyhow::ensure!(
                !path.starts_with(home.join(private)),
                "Choose an installed game folder, not application settings."
            );
        }
    }
    anyhow::ensure!(
        path != std::path::Path::new("/home") && path != std::path::Path::new("/var"),
        "Choose a game folder, not a system folder."
    );
    if let Some(state) =
        crate::db::Db::default_path().and_then(|p| p.parent().map(|p| p.to_path_buf()))
    {
        anyhow::ensure!(
            !path.starts_with(&state) && !state.starts_with(&path),
            "The selected folder includes Flummox's own state."
        );
    }
    for system in [
        "/usr", "/etc", "/proc", "/sys", "/dev", "/bin", "/lib", "/lib64", "/boot",
    ] {
        anyhow::ensure!(
            !path.starts_with(system),
            "Choose a game folder outside system directories."
        );
    }
    Ok(path)
}

/// Resolves a typed location without invoking a shell or expanding variables.
pub fn folder_path(input: &str, home: &std::path::Path) -> PathBuf {
    let text = input.trim();
    let text = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| text.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(text);
    let expand = |text: &str| {
        if text == "~" {
            home.to_path_buf()
        } else if let Some(relative) = text.strip_prefix("~/") {
            home.join(relative)
        } else {
            PathBuf::from(text)
        }
    };
    let raw = expand(text);
    if raw.exists() {
        raw
    } else {
        expand(&text.replace("\\ ", " "))
    }
}

/// Builds a read-only preflight for an explicit client command.
pub fn space_plan(
    command: &Command,
    snapshot: &Snapshot,
) -> anyhow::Result<crate::storage::SpacePlan> {
    use crate::storage::{self, SpacePlan};
    use anyhow::Context;
    match command {
        Command::Enqueue {
            game, operation, ..
        } if *operation != Operation::Analyze => {
            storage::native_plan(&game.install_dir, *operation == Operation::Decompress)
        }
        Command::EnqueuePack { game, task } => {
            let mut plan = SpacePlan::default();
            match task {
                // A store that does not exist yet: budget its upper bound on
                // the store's drive.
                PackTask::Create { store }
                | PackTask::Activate {
                    store,
                    create: true,
                    ..
                } if !store.exists() => {
                    let footprint = storage::inventory(&game.install_dir)?;
                    plan.retained_original = true;
                    plan.add(
                        storage::volume(store)?,
                        storage::pack_bound(&footprint)?,
                        "Verified store; original remains on the source drive",
                    )?;
                    if matches!(task, PackTask::Activate { .. }) {
                        plan.add(
                            storage::volume(&game.install_dir)?,
                            0,
                            "Original retained; activation metadata",
                        )?;
                    }
                }
                PackTask::Activate { store, .. } => {
                    anyhow::ensure!(store.exists(), "Verified store is unavailable");
                    plan.retained_original = true;
                    plan.add(
                        storage::volume(store)?,
                        0,
                        "Existing verified store and writable updates",
                    )?;
                    plan.add(
                        storage::volume(&game.install_dir)?,
                        0,
                        "Original retained; activation metadata",
                    )?;
                }
                PackTask::Compact | PackTask::Restore | PackTask::VerifyRestored => {
                    let install = snapshot
                        .packs
                        .iter()
                        .find(|install| install.game_path == game.install_dir)
                        .context("This game has no activated store")?;
                    let updates = storage::inventory(&install.writes_path)?;
                    let summary = install.summary.clone().map(Ok).unwrap_or_else(|| {
                        Ok::<_, anyhow::Error>(
                            crate::pack::Reader::open(&install.store_path)?
                                .summary()
                                .clone(),
                        )
                    })?;
                    // Compaction rebuilds the whole store beside the old one.
                    if matches!(task, PackTask::Compact) {
                        let footprint = storage::Footprint {
                            files: summary.files.saturating_add(updates.files),
                            bytes: summary.logical_bytes.saturating_add(updates.bytes),
                            largest: 0,
                            metadata_bytes: updates
                                .metadata_bytes
                                .saturating_add(summary.metadata_bytes.saturating_mul(8)),
                        };
                        plan.add(
                            storage::volume(&install.store_path)?,
                            storage::pack_bound(&footprint)?,
                            "New compacted store; previous version retained",
                        )?;
                    } else {
                        // With the original still on disk, restoring only adds
                        // the update layer. Otherwise every file is rebuilt.
                        let bytes = if install.backup_path.is_some()
                            && !matches!(task, PackTask::VerifyRestored)
                        {
                            updates.bytes
                        } else {
                            summary.logical_bytes.saturating_add(updates.bytes)
                        };
                        // The FUSE launcher mount is not the destination filesystem.
                        let destination = install
                            .game_path
                            .parent()
                            .context("Game has no parent folder")?;
                        plan.add(
                            storage::volume(destination)?,
                            bytes,
                            "Ordinary files and writable updates",
                        )?;
                    }
                    plan.retained_original = install.backup_path.is_some();
                }
                _ => {}
            }
            Ok(plan)
        }
        _ => Ok(SpacePlan::default()),
    }
}

#[cfg(test)]
mod folder_tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    #[test]
    fn the_operation_lock_waits_for_a_holder_that_is_about_to_finish() -> TestResult {
        let dir = tempfile::tempdir().ctx("state")?;
        let held = lock_in(dir.path(), std::time::Duration::ZERO).ctx("first holder")?;
        let busy = lock_in(dir.path(), std::time::Duration::from_millis(150));
        check(
            busy.is_err_and(|error| error.to_string() == LOCK_BUSY),
            "control: a holder that stays makes the wait run out",
        )?;
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(held);
        });
        let started = std::time::Instant::now();
        lock_in(dir.path(), std::time::Duration::from_secs(5)).ctx("second holder")?;
        check(
            started.elapsed() >= std::time::Duration::from_millis(200),
            "the second holder waited for the first",
        )?;
        release.join().map_err(|_| "release thread panicked".to_string())
    }

    #[test]
    fn typed_locations_expand_home_spaces_and_preserve_literal_backslashes() -> TestResult {
        let home = tempfile::tempdir().ctx("home")?;
        let games = home.path().join("My Games");
        std::fs::create_dir(&games).ctx("games")?;
        for input in [
            "~/My Games",
            "~/My\\ Games",
            "\"~/My Games\"",
            "'~/My Games'",
        ] {
            check_eq(folder_path(input, home.path()), games.clone(), input)?;
        }
        let literal = home.path().join("My\\ Games");
        std::fs::create_dir(&literal).ctx("literal backslash")?;
        check_eq(
            folder_path("~/My\\ Games", home.path()),
            literal,
            "existing literal path wins",
        )?;
        let legacy: Library = serde_json::from_value(serde_json::json!({
            "path": games, "automatic": false, "custom": true
        }))
        .ctx("legacy location")?;
        check_eq(
            legacy.folder_kind,
            FolderKind::Game,
            "existing locations retain single-game behavior",
        )
    }
}
