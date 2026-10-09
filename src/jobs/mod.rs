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

pub use client::{configured_libraries, request, request_many, state_dir};

/// Protocol version. A mismatched installed worker is rejected before work.
pub const VERSION: u32 = 9;

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
            Self::Expressive => "Smooth",
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
            Self::Queued => "Waiting",
            Self::Analyzing => "Analyzing",
            Self::Running => "Running",
            Self::Pausing => "Pausing",
            Self::Paused => "Paused",
            Self::Cancelling => "Stopping",
            Self::Cancelled => "Stopped",
            Self::Completed => "Completed",
            Self::Partial => "Partly done",
            Self::Interrupted => "Interrupted",
            Self::Failed => "Failed",
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
            Self::Create { .. } => "Create the store",
            Self::Activate { create: true, .. } => "Create the store and switch to Maximum",
            Self::Activate { .. } => "Switch to Maximum",
            Self::Compact => "Fold in updates",
            Self::Restore => "Decompress to ordinary files",
            Self::VerifyRestored => "Check decompressed files",
            Self::Reclaim => "Delete the original",
            Self::Prune => "Delete the previous version",
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
    /// Queue several jobs in one request. Each item gets the checks of
    /// `Enqueue`; a refused item does not stop the rest. The reply lists the
    /// refusals beside the snapshot.
    EnqueueMany {
        items: Vec<(Game, Operation, CompressOpts)>,
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
/// One item of an `EnqueueMany` request that was not queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// Title of the game as sent in the request.
    pub title: String,
    /// Why the coordinator refused it, as it would reply to `Enqueue`.
    pub reason: String,
}

/// The coordinator's reply. Exactly one of `snapshot` and `error` is set.
#[derive(Serialize, Deserialize)]
pub(crate) struct Response {
    pub version: u32,
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
    /// Items of an `EnqueueMany` that were refused. Empty for other commands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<Refusal>,
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

/// Reply to a client that connects while the coordinator is still remounting
/// stores. The client retries until its start-up wait ends.
pub(crate) const STARTING: &str = "The background worker is starting…";

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
    // `args_os`: `args` panics on an argument that is not valid UTF-8, and
    // this runs before the command line parser sees the arguments.
    let role = std::env::args_os().nth(1);
    if role.as_deref() == Some(std::ffi::OsStr::new("__coordinator")) {
        service::run()?;
        Ok(true)
    } else if role.as_deref() == Some(std::ffi::OsStr::new("__worker")) {
        worker::run()?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Unix time in seconds, or zero when the clock reads before the epoch.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Refuses roots, the home folder and the folders above it, shared
/// top-level folders such as `/mnt` and `/opt`, and application and system
/// configuration directories.
pub fn validate_folder(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let home = crate::launchers::Env::current().map(|env| env.home);
    validate_folder_for(path, home.as_deref())
}

/// [`validate_folder`] for a given home folder.
fn validate_folder_for(
    path: &std::path::Path,
    home: Option<&std::path::Path>,
) -> anyhow::Result<PathBuf> {
    use std::path::Path;
    let path = path.canonicalize()?;
    anyhow::ensure!(
        path.is_dir() && path.parent().is_some(),
        "Choose a game folder, not an entire drive."
    );
    // The folder is canonical, so the home it is compared with must be too.
    let home = home.map(|home| home.canonicalize().unwrap_or_else(|_| home.to_path_buf()));
    if let Some(home) = &home {
        anyhow::ensure!(
            !home.starts_with(&path),
            "Choose a game folder, not your home folder or one that contains it."
        );
        for private in [".ssh", ".gnupg", ".config", ".cache"] {
            anyhow::ensure!(
                !path.starts_with(home.join(private)),
                "Choose an installed game folder, not application settings."
            );
        }
        for shared in [".local", ".local/share", ".var", ".steam", "Documents"] {
            anyhow::ensure!(
                path != home.join(shared),
                "Choose a game folder, not a general folder in your home."
            );
        }
    }
    let media_root = path.starts_with("/run/media") && path.components().count() <= 4;
    anyhow::ensure!(
        !media_root
            && [
                "/home",
                "/var",
                "/var/home",
                "/mnt",
                "/media",
                "/run",
                "/opt",
                "/srv",
                "/root",
                "/tmp"
            ]
            .iter()
            .all(|shared| path != Path::new(shared)),
        "Choose a game folder, not a system or shared folder."
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
                        "New store; the original stays on its drive",
                    )?;
                    if matches!(task, PackTask::Activate { .. }) {
                        plan.add(
                            storage::volume(&game.install_dir)?,
                            0,
                            "The original is kept; a little space for the switch",
                        )?;
                    }
                }
                PackTask::Activate { store, .. } => {
                    anyhow::ensure!(store.exists(), "The store is missing. Create it first.");
                    plan.retained_original = true;
                    plan.add(
                        storage::volume(store)?,
                        0,
                        "The existing store and room for updates",
                    )?;
                    plan.add(
                        storage::volume(&game.install_dir)?,
                        0,
                        "The original is kept; a little space for the switch",
                    )?;
                }
                PackTask::Compact | PackTask::Restore | PackTask::VerifyRestored => {
                    let install = snapshot
                        .packs
                        .iter()
                        .find(|install| install.game_path == game.install_dir)
                        .context("This game is not using Maximum.")?;
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
                            "A new store; the previous version is kept until you delete it",
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
                            "Ordinary files and room for updates",
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
    fn folders_that_hold_the_home_or_other_users_data_are_refused() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let real = temp.path().join("real-home");
        let link = temp.path().join("home-link");
        let steam = real.join(".local/share/Steam/steamapps/common/Game");
        std::fs::create_dir_all(&steam).ctx("game folder")?;
        std::os::unix::fs::symlink(&real, &link).ctx("home symlink")?;
        let refuses = |path: &std::path::Path, home: &std::path::Path| {
            validate_folder_for(path, Some(home)).is_err()
        };
        check(refuses(&real, &link), "a symlinked home is still the home")?;
        check(
            refuses(temp.path(), &real),
            "a folder that contains the home is refused",
        )?;
        check(refuses(&real.join(".local"), &real), "~/.local")?;
        check(refuses(&real.join(".local/share"), &real), "~/.local/share")?;
        check(
            validate_folder_for(&steam, Some(&link)).is_ok(),
            "control: a game folder deep in the home is accepted",
        )?;
        for shared in ["/tmp", "/mnt", "/opt", "/"] {
            let shared = std::path::Path::new(shared);
            check(
                !shared.exists() || validate_folder_for(shared, None).is_err(),
                format!("{} is refused", shared.display()),
            )?;
        }
        check(
            validate_folder_for(&steam, None).is_ok(),
            "control: the same folder is accepted without a home",
        )
    }

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
        release
            .join()
            .map_err(|_| "release thread panicked".to_string())
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
