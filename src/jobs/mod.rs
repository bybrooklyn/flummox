//! Persistent jobs shared by the window and command line.
//!
//! A coordinator owns workers and durable outcomes. Closing a client never
//! transfers ownership or terminates a worker.

mod autostart;
mod service;
mod worker;

use crate::{
    backend::{CompressOpts, Event},
    estimate::Estimate,
    model::Game,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use service::{configured_libraries, request, state_dir};

/// Protocol version. A mismatched installed worker is rejected before work.
pub const VERSION: u32 = 7;

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
    Analyze,
    Compress,
    Decompress,
    Pack,
}

/// Durable lifecycle, including outcomes that need another attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Queued,
    Analyzing,
    Running,
    Pausing,
    Paused,
    Cancelling,
    Cancelled,
    Completed,
    Partial,
    Interrupted,
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
    pub id: i64,
    pub game: Game,
    pub operation: Operation,
    pub options: CompressOpts,
    pub phase: Phase,
    pub files_done: u64,
    pub bytes_done: u64,
    pub files_total: u64,
    pub bytes_total: u64,
    pub estimate: Option<Estimate>,
    pub message: String,
    pub errors: Vec<String>,
    pub created: u64,
    pub elapsed: u64,
    pub drive_change: Option<i64>,
    pub user_paused: bool,
    #[serde(default)]
    pub pack: Option<PackTask>,
    #[serde(default)]
    pub pack_interruptible: bool,
    #[serde(default)]
    pub space_plan: Option<crate::storage::SpacePlan>,
}

/// Durable work for Maximum Space. Paths survive client disconnection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PackTask {
    Create {
        #[serde(with = "crate::path_serde")]
        store: PathBuf,
    },
    Activate {
        #[serde(with = "crate::path_serde")]
        store: PathBuf,
        create: bool,
        qualification: Option<Box<crate::compatibility::Report>>,
    },
    Compact,
    Restore,
    Reclaim,
    Prune,
}

impl PackTask {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Create { .. } => "Create verified store",
            Self::Activate { create: true, .. } => "Create Maximum Space",
            Self::Activate { .. } => "Activate Maximum Space",
            Self::Compact => "Compact updates",
            Self::Restore => "Restore ordinary files",
            Self::Reclaim => "Reclaim original",
            Self::Prune => "Reclaim previous version",
        }
    }
}

/// How a manually added location maps to games.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FolderKind {
    #[default]
    Game,
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
    pub automatic: bool,
    pub custom: bool,
    #[serde(default)]
    pub folder_kind: FolderKind,
}

/// Snapshot returned to clients; no database handle crosses the boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub jobs: Vec<Job>,
    pub libraries: Vec<Library>,
    pub excluded: Vec<String>,
    pub gaming: Option<String>,
    #[serde(default)]
    pub reduced_motion: bool,
    #[serde(default)]
    pub theme: ThemePreference,
    #[serde(default)]
    pub motion: MotionPreference,
    #[serde(default)]
    pub packs: Vec<crate::pack::Install>,
    #[serde(default)]
    pub discovered: Vec<Game>,
}

/// Client requests operate on ids, never shell command strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Snapshot,
    /// Restart an idle coordinator after replacing the executable.
    Restart,
    Enqueue {
        game: Game,
        operation: Operation,
        options: CompressOpts,
    },
    EnqueuePack {
        game: Game,
        task: PackTask,
    },
    EnqueuePlanned {
        command: Box<Command>,
        plan: crate::storage::SpacePlan,
    },
    Pause {
        id: i64,
        paused: bool,
    },
    Cancel(i64),
    Retry(i64),
    Library(Library),
    RemoveLibrary(#[serde(with = "crate::path_serde")] PathBuf),
    Exclude {
        id: String,
        excluded: bool,
    },
    ReducedMotion(bool),
    Theme(ThemePreference),
    Motion(MotionPreference),
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

#[derive(Serialize, Deserialize)]
pub(crate) struct Request {
    pub version: u32,
    pub command: Command,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Response {
    pub version: u32,
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum WorkerEvent {
    Progress(Event),
    Estimate(Estimate),
    SpacePlan(crate::storage::SpacePlan),
    Done {
        cancelled: bool,
        errors: Vec<String>,
        drive_change: Option<i64>,
    },
    Failed(String),
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Work {
    pub version: u32,
    pub job: Job,
}

/// Serializes filesystem operations across coordinator workers and legacy CLI jobs.
/// Keep the returned handle alive until the operation and recording finish.
pub fn operation_lock() -> anyhow::Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state_dir()?.join("operation.lock"))?;
    anyhow::ensure!(
        lock.try_lock().is_ok(),
        "Another Flummox process is working. Retry when it finishes."
    );
    Ok(lock)
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
                }
                PackTask::Compact | PackTask::Restore => {
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
                        let bytes = if install.backup_path.is_some() {
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
    use crate::testutil::{Ctx, TestResult, check_eq};

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
