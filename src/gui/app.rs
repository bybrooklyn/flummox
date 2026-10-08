//! Window state. Blocking work returns snapshots through tasks and subscriptions.

use crate::{
    db::{Activity, Db, GameRecord},
    fsprobe,
    jobs::{
        self, Command, FolderKind, Job, Library, MotionPreference, Operation, PackTask, Phase,
        Snapshot, ThemePreference,
    },
    launchers::Env,
    model::Game,
};
use iced::{Animation, Task, animation::Easing};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A navigation destination.
///
/// Drives and Recovery are sections inside Settings. Going to one opens
/// Settings scrolled to that section. Queue is the Jobs page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Overview,
    Games,
    Queue,
    Drives,
    Recovery,
    Settings,
}
/// The pages listed at the top of the sidebar. Settings is drawn apart from
/// them, at the bottom.
pub const PAGES: [Page; 3] = [Page::Overview, Page::Games, Page::Queue];
impl Page {
    /// The page that is shown for this destination.
    pub fn main(self) -> Self {
        match self {
            Self::Overview | Self::Games | Self::Queue => self,
            _ => Self::Settings,
        }
    }
    /// Sidebar order, which decides the direction a page change slides in.
    pub fn rank(self) -> u8 {
        match self.main() {
            Self::Overview => 0,
            Self::Games => 1,
            Self::Queue => 2,
            _ => 3,
        }
    }
    /// The id of the Settings container to scroll to, for a destination that
    /// is a section. `view::settings_page` must give a container this id.
    pub fn section(self) -> Option<&'static str> {
        match self {
            Self::Drives => Some("settings-locations"),
            Self::Recovery => Some("settings-recovery"),
            _ => None,
        }
    }
}
impl Page {
    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Games => "Games",
            Self::Queue => "Jobs",
            Self::Drives => "Drives",
            Self::Recovery => "Recovery",
            Self::Settings => "Settings",
        }
    }
}

/// A discovered game plus what the scan learned about the drive it is on.
#[derive(Debug, Clone)]
pub struct GameRow {
    pub game: Game,
    /// Filesystem type, or `Offline` or `Unavailable` when it was not probed.
    pub filesystem: String,
    pub mountpoint: Option<PathBuf>,
    /// Either of the two flags below.
    pub supported: bool,
    /// The filesystem compresses in place and a backend exists for it.
    pub native_supported: bool,
    /// A Maximum Space store can be mounted over this game.
    pub pack_supported: bool,
    /// Why the game cannot be compressed. Replaces the row's status line.
    pub note: Option<String>,
    /// Image for the row icon.
    pub artwork: Option<super::artwork::Source>,
    /// Larger image for the detail pane.
    pub cover: Option<super::artwork::Source>,
}
impl GameRow {
    /// Probes the filesystem under the game's install directory. Blocks, so
    /// it runs inside `scan`.
    fn probe(
        game: Game,
        artwork: Option<super::artwork::Source>,
        cover: Option<super::artwork::Source>,
    ) -> Self {
        // `libraries` marks a game it could not rediscover with this detail
        // prefix. Its directory may be gone, so it is not probed.
        if matches!(&game.state, crate::model::InstallState::Broken { detail } if detail.starts_with("Library unavailable:"))
        {
            return Self {
                game,
                filesystem: "Offline".into(),
                mountpoint: None,
                supported: false,
                native_supported: false,
                pack_supported: false,
                note: Some("Reconnect the original drive and refresh.".into()),
                artwork,
                cover,
            };
        }
        match fsprobe::probe(&game.install_dir) {
            Ok(fs) => {
                let tier = fsprobe::tier_for(&fs);
                let native_supported = match &tier {
                    fsprobe::Tier::Native(kind) => crate::backend::for_kind(*kind).is_some(),
                    fsprobe::Tier::Pack | fsprobe::Tier::Unsupported(_) => false,
                };
                // Mounting a store needs the `pack-mount` feature and FUSE.
                let pack_supported = cfg!(feature = "pack-mount")
                    && std::path::Path::new("/dev/fuse").exists()
                    && matches!(tier, fsprobe::Tier::Native(_) | fsprobe::Tier::Pack);
                let supported = native_supported || pack_supported;
                Self {
                    game,
                    filesystem: fs.fstype,
                    mountpoint: Some(fs.mountpoint),
                    supported,
                    native_supported,
                    pack_supported,
                    note: (!supported)
                        .then(|| "Compression isn't supported on this drive yet.".into()),
                    artwork,
                    cover,
                }
            }
            Err(_) => Self {
                game,
                filesystem: "Unavailable".into(),
                mountpoint: None,
                supported: false,
                native_supported: false,
                pack_supported: false,
                note: Some("Reconnect this drive to continue.".into()),
                artwork,
                cover,
            },
        }
    }
}
/// A mountpoint that holds at least one game.
#[derive(Debug, Clone)]
pub struct Drive {
    pub path: PathBuf,
    /// Free bytes, or `None` when the drive could not be read.
    pub free: Option<u64>,
    pub games: usize,
}
/// Everything one background scan read. `update` swaps it into `State` whole.
#[derive(Debug, Clone)]
pub struct ScanResult {
    /// The worker epoch and scan generation of the snapshot the scan started
    /// from. `update` discards the result when the worker has moved on.
    pub worker_epoch: u64,
    pub generation: u64,
    /// The worker's game list as scanned, tools included.
    pub discovered: Vec<Game>,
    pub reports: Vec<crate::compatibility::Report>,
    pub games: Vec<GameRow>,
    pub drives: Vec<Drive>,
    pub records: Vec<GameRecord>,
    pub activity: Vec<Activity>,
    pub warnings: Vec<String>,
}
/// The text of the toast. An error stays until dismissed; anything else
/// leaves after four seconds.
#[derive(Debug, Clone)]
pub struct Status {
    pub is_error: bool,
    pub text: String,
}
impl Status {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            is_error: false,
            text: text.into(),
        }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            text: text.into(),
        }
    }
}
/// Order of the Games list. Size and Saving put the largest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Games that would save the most first, then the rest in groups.
    Worth,
    Name,
    Size,
}
impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Worth => "Most space to save",
            Self::Name => "Name",
            Self::Size => "Size",
        }
    }
}
/// How a game is compressed when its Compress button is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageChoice {
    /// Native compression in place. Quick, and the files stay where they are.
    Standard,
    /// A Maximum Space store mounted at the game's path. Saves more, takes
    /// minutes, and keeps the original until the user confirms the game runs.
    Maximum,
}

/// What compressing a game gained, for its row and its History entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A native pass. Both sizes are sampled predictions, since the
    /// filesystem does not report what compression saved.
    Estimated { installed: u64, saved: u64 },
    /// Maximum Space with the original deleted. Both sizes are the store's.
    Measured { before: u64, after: u64 },
    /// Maximum Space with the original still kept, so nothing is saved yet.
    AwaitingConfirm { expected: u64 },
}

/// The estimated result of one finished compression job.
pub fn job_outcome(job: &Job) -> Option<Outcome> {
    (job.operation == Operation::Compress && job.phase == Phase::Completed)
        .then_some(job.estimate.as_ref())
        .flatten()
        .map(|estimate| Outcome::Estimated {
            installed: estimate.install_bytes,
            saved: estimate.saving(),
        })
}

/// Where a game sits in the Worth order: games that would save space, games
/// not analyzed yet, compressed games, then games with little to gain or on
/// a drive that cannot compress.
pub const WORTH_GROUPS: [&str; 4] = [
    "Worth compressing",
    "Not analyzed yet",
    "Compressed",
    "Little to gain",
];

/// Which games the Games list shows. `State::filtered` holds the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    /// Supported, idle and not yet compressed.
    Ready,
    Compressed,
    /// Unsupported, or the latest job failed, was partial or was interrupted.
    Attention,
    /// The recorded build differs from the installed one.
    Updated,
}
impl Filter {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All games",
            Self::Ready => "Ready",
            Self::Compressed => "Compressed",
            Self::Attention => "Needs attention",
            Self::Updated => "Updated games",
        }
    }
}

/// Everything the window draws from.
///
/// `update` is the only writer. Games are keyed by `GameId::to_string()` in
/// every map and set below.
pub struct State {
    pub env: Env,
    /// Compatibility reports read by the last scan, plus any imported since.
    pub reports: Vec<crate::compatibility::Report>,
    pub page: Page,
    pub games: Vec<GameRow>,
    pub artwork_cache: super::artwork::Cache,
    pub drives: Vec<Drive>,
    pub records: Vec<GameRecord>,
    pub activity: Vec<Activity>,
    pub warnings: Vec<String>,
    // The toast.
    pub status: Option<Status>,
    pub status_reveal: Animation<bool>,
    /// When the toast leaves by itself. `None` for an error.
    pub status_deadline: Option<Instant>,
    /// The newest state received from the worker.
    pub snapshot: Snapshot,
    /// A command waiting for the user to accept its space plan.
    pub planned: Option<(Command, crate::storage::SpacePlan)>,
    pub qualification: Option<crate::qualification::Wizard>,
    // Navigation and scrolling.
    /// The highlight animation of each sidebar entry.
    pub nav: Vec<(Page, Animation<bool>)>,
    pub page_reveal: Animation<bool>,
    /// Frames keep coming until this instant, after a scroll set from code.
    pub scroll_redraw_until: Option<Instant>,
    /// 1.0 when the last page change went to a later page, -1.0 to an earlier.
    pub page_direction: f32,
    /// The last scroll offset of each main page, by page label.
    pub scroll_positions: super::surface::Positions,
    /// True once any snapshot has arrived.
    pub snapshot_loaded: bool,
    /// The error from the last failed snapshot request.
    pub connection_error: Option<String>,
    // The Games list.
    pub query: String,
    pub selected: std::collections::HashSet<String>,
    /// The game whose detail pane is open or closing.
    pub expanded: Option<String>,
    pub sort: Sort,
    pub filter: Filter,
    /// A mountpoint to show games from.
    pub drive_filter: Option<PathBuf>,
    /// A launcher label to show games from.
    pub launcher_filter: Option<String>,
    /// Presets picked in this session. `preset_for` has the fallbacks.
    pub presets: std::collections::HashMap<String, crate::backend::Preset>,
    pub reduced_motion: bool,
    pub motion: MotionPreference,
    pub theme: ThemePreference,
    pub system_theme: iced::theme::Mode,
    /// A scan is running in the background.
    pub scanning: bool,
    // The "Add a location" form.
    pub folder: String,
    pub folder_error: Option<String>,
    pub folder_kind: FolderKind,
    /// A native dialog is open. Buttons that open one are disabled meanwhile.
    pub picker_busy: bool,
    /// How many rows of the Games list are built.
    pub shown: usize,
    /// Opening and closing of the detail pane.
    pub detail: Animation<bool>,
    /// Whether the once-a-second snapshot subscription runs.
    pub polling: bool,
    /// The animated progress fraction of each job, by job id.
    pub progress: std::collections::HashMap<i64, Animation<f32>>,
    /// Store paths the user typed or picked.
    pub pack_paths: std::collections::HashMap<String, String>,
    /// The game whose "delete the original" button is showing its confirm
    /// step. At most one at a time.
    pub confirm_reclaim: std::collections::HashSet<String>,
    /// Games with the advanced storage section open.
    pub advanced: std::collections::HashSet<String>,
    /// Games with an active pack job. Their actions are disabled.
    pub pending: std::collections::HashSet<String>,
    /// A batch of automatic analyses is being sent to the worker.
    analysis_queuing: bool,
    /// Group and size key per game for the Worth sort. Captured at set
    /// moments so rows do not move while an analysis is filling in estimates.
    worth_order: std::collections::HashMap<String, (u8, u64)>,
    /// Estimates arrived while a row was open or selected, so the list was
    /// left as it was and the page offers to sort again.
    pub order_stale: bool,
    /// The "Little to gain" group is expanded.
    pub show_low: bool,
    /// Modes the user picked, by game. A game without an entry uses
    /// `State::choice_for`'s default.
    pub choices: std::collections::HashMap<String, StorageChoice>,
    /// A snapshot has been applied at least once.
    snapshot_loaded_before: bool,
}
impl State {
    /// An empty window state on Overview. Nothing is loaded until the first
    /// `Message::Refresh`.
    pub fn new(env: Env) -> Self {
        Self {
            env,
            reports: vec![],
            page: Page::Overview,
            games: vec![],
            artwork_cache: Default::default(),
            drives: vec![],
            records: vec![],
            activity: vec![],
            warnings: vec![],
            status: None,
            status_reveal: Animation::new(false)
                .duration(Duration::from_millis(180))
                .easing(Easing::EaseOutCubic),
            status_deadline: None,
            snapshot: Snapshot::default(),
            planned: None,
            qualification: None,
            nav: PAGES
                .into_iter()
                .chain(std::iter::once(Page::Settings))
                .map(|page| {
                    (
                        page,
                        Animation::new(page == Page::Overview)
                            .duration(Duration::from_millis(200))
                            .easing(Easing::EaseOutCubic),
                    )
                })
                .collect(),
            scroll_redraw_until: None,
            page_reveal: Animation::new(true)
                .duration(Duration::from_millis(240))
                .easing(Easing::EaseOutCubic),
            page_direction: 1.0,
            scroll_positions: Default::default(),
            snapshot_loaded: false,
            connection_error: None,
            query: String::new(),
            selected: Default::default(),
            expanded: None,
            sort: Sort::Worth,
            filter: Filter::All,
            drive_filter: None,
            launcher_filter: None,
            presets: Default::default(),
            reduced_motion: false,
            motion: MotionPreference::Expressive,
            theme: ThemePreference::System,
            system_theme: iced::theme::Mode::Dark,
            scanning: false,
            folder: String::new(),
            folder_error: None,
            folder_kind: FolderKind::Collection,
            picker_busy: false,
            shown: 40,
            detail: Animation::new(false).duration(Duration::from_millis(200)),
            polling: true,
            progress: Default::default(),
            pack_paths: Default::default(),
            confirm_reclaim: Default::default(),
            advanced: Default::default(),
            pending: Default::default(),
            analysis_queuing: false,
            worth_order: Default::default(),
            order_stale: false,
            show_low: false,
            choices: Default::default(),
            snapshot_loaded_before: false,
        }
    }
    /// Replaces the toast and restarts its reveal animation.
    pub fn show_status(&mut self, status: Status) {
        self.status_deadline = (!status.is_error).then(|| Instant::now() + Duration::from_secs(4));
        self.status = Some(status);
        self.status_reveal = Animation::new(false)
            .duration(self.motion_duration(240, 150))
            .easing(self.motion_easing())
            .go(true, Instant::now());
    }

    /// An animation length in milliseconds for the current motion setting.
    /// Reduced motion gives zero.
    fn motion_duration(&self, expressive: u64, subtle: u64) -> Duration {
        Duration::from_millis(match self.motion {
            MotionPreference::Expressive => expressive,
            MotionPreference::Subtle => subtle,
            MotionPreference::Reduced => 0,
        })
    }

    fn motion_easing(&self) -> Easing {
        match self.motion {
            MotionPreference::Expressive => Easing::EaseOutCubic,
            MotionPreference::Subtle | MotionPreference::Reduced => Easing::EaseOutCubic,
        }
    }

    /// Starts hiding the toast. With motion it stays in `status` until `Tick`
    /// sees the animation end.
    fn dismiss_status(&mut self) {
        self.status_deadline = None;
        if self.reduced_motion {
            self.status = None;
        } else {
            self.status_reveal.go_mut(false, Instant::now());
        }
    }
    /// The preset for a game: the one picked in this session, else the one
    /// its newest compression job used, else Balanced.
    pub fn preset_for(&self, id: &str) -> crate::backend::Preset {
        self.presets
            .get(id)
            .copied()
            .or_else(|| {
                self.snapshot
                    .jobs
                    .iter()
                    .rev()
                    .find(|job| {
                        job.game.id.to_string() == id && job.operation == Operation::Compress
                    })
                    .map(|job| job.options.preset)
            })
            .unwrap_or(crate::backend::Preset::Balanced)
    }
    /// Sum of the launchers' size hints. Games without one count as zero.
    pub fn total_bytes(&self) -> u64 {
        self.games.iter().filter_map(|g| g.game.size_hint).sum()
    }
    /// The newest job of any kind for this install directory.
    pub fn latest(&self, game: &Game) -> Option<&Job> {
        self.snapshot
            .jobs
            .iter()
            .rev()
            .find(|j| j.game.install_dir == game.install_dir)
    }
    /// What compressing this game gained, or `None` when it is not compressed.
    ///
    /// A Maximum Space install reports its store's own sizes. A native pass
    /// reports the estimate its job carried, else the one in its record.
    pub fn result(&self, game: &Game) -> Option<Outcome> {
        if let Some(install) = self
            .snapshot
            .packs
            .iter()
            .find(|install| install.game_path == game.install_dir)
        {
            let summary = install.summary.as_ref()?;
            // Bytes shared with another game's store are not charged here.
            let after = summary.archive_bytes.saturating_sub(summary.shared_bytes);
            return Some(if install.backup_path.is_some() {
                Outcome::AwaitingConfirm {
                    expected: summary.logical_bytes.saturating_sub(after),
                }
            } else {
                Outcome::Measured {
                    before: summary.logical_bytes,
                    after,
                }
            });
        }
        if !self.compressed(game) {
            return None;
        }
        self.snapshot
            .jobs
            .iter()
            .rev()
            .filter(|job| job.game.install_dir == game.install_dir && job.game.build == game.build)
            .find_map(job_outcome)
            .or_else(|| {
                self.records
                    .iter()
                    .find(|record| record.id == game.id && record.build == game.build)
                    .map(|record| Outcome::Estimated {
                        installed: record.install_bytes,
                        saved: u64::try_from(record.est_saving).unwrap_or(0),
                    })
            })
    }
    /// The mode this game's Compress button uses. Standard unless the user
    /// chose otherwise, the game already runs from a store, or its drive has
    /// no native compression, in which case Maximum is the only mode.
    pub fn choice_for(&self, game: &Game) -> StorageChoice {
        if let Some(choice) = self.choices.get(&game.id.to_string()) {
            return *choice;
        }
        let stored = self
            .snapshot
            .packs
            .iter()
            .any(|install| install.game_path == game.install_dir);
        let only_mode = self
            .games
            .iter()
            .any(|row| row.game.id == game.id && !row.native_supported && row.pack_supported);
        if stored || only_mode {
            StorageChoice::Maximum
        } else {
            StorageChoice::Standard
        }
    }
    /// What `choice` is predicted to save for this game. `None` until it has
    /// been analyzed, and zero when the saving is too small to bother with
    /// or the drive does not support that mode.
    pub fn prospect(&self, game: &Game, choice: StorageChoice) -> Option<u64> {
        let estimate = self.estimate(game)?;
        let row = self.games.iter().find(|row| row.game.id == game.id)?;
        let saving = match choice {
            StorageChoice::Standard if row.native_supported => estimate.saving(),
            StorageChoice::Maximum if row.pack_supported => estimate.maximum_saving().unwrap_or(0),
            _ => 0,
        };
        let worthwhile = crate::recommendation::clears_threshold(
            saving,
            estimate.disk_now,
            crate::recommendation::Policy::default(),
        );
        Some(if worthwhile { saving } else { 0 })
    }
    /// A game's place in the Worth order as captured, with games seen since
    /// the capture counted as not analyzed.
    pub fn worth(&self, game: &Game) -> (u8, u64) {
        self.worth_order
            .get(&game.id.to_string())
            .copied()
            .unwrap_or((1, 0))
    }
    /// Recomputes every game's place in the Worth order from what is known now.
    pub fn capture_order(&mut self) {
        self.worth_order = self
            .games
            .iter()
            .map(|row| {
                let place = if !row.supported {
                    (3, 0)
                } else if self.compressed(&row.game) {
                    let saved = match self.result(&row.game) {
                        Some(Outcome::Estimated { saved, .. }) => saved,
                        Some(Outcome::Measured { before, after }) => before.saturating_sub(after),
                        // Nothing is saved until the original is deleted.
                        Some(Outcome::AwaitingConfirm { .. }) => 0,
                        None => 0,
                    };
                    (2, saved)
                } else {
                    match self.prospect(&row.game, self.choice_for(&row.game)) {
                        Some(0) => (3, 0),
                        Some(saving) => (0, saving),
                        None => (1, row.game.size_hint.unwrap_or(0)),
                    }
                };
                (row.game.id.to_string(), place)
            })
            .collect();
        self.order_stale = false;
    }
    /// Whether an analysis is queued or running.
    fn analysis_active(&self) -> bool {
        self.snapshot
            .jobs
            .iter()
            .any(|job| job.operation == Operation::Analyze && job.phase.active())
    }
    /// The estimate that still applies to this game at its installed build.
    ///
    /// Jobs are read newest first, and the search ends at the first job that
    /// is neither an analysis nor still queued. So once a compression or
    /// decompression has started, earlier estimates no longer count.
    pub fn estimate(&self, game: &Game) -> Option<&crate::estimate::Estimate> {
        self.snapshot
            .jobs
            .iter()
            .rev()
            .filter(|j| j.game.install_dir == game.install_dir && j.game.build == game.build)
            .take_while(|j| j.operation == Operation::Analyze || j.phase == Phase::Queued)
            .filter(|j| {
                !matches!(
                    j.phase,
                    Phase::Cancelled | Phase::Failed | Phase::Interrupted
                )
            })
            .find_map(|j| j.estimate.as_ref())
    }
    /// What to do with this game, from its estimate and what its drive
    /// supports. `None` until an estimate exists. Maximum Space is offered
    /// only when the estimate says a qualification matched.
    pub fn recommendation(&self, game: &Game) -> Option<crate::recommendation::Recommendation> {
        let row = self.games.iter().find(|row| row.game.id == game.id)?;
        self.estimate(game).map(|estimate| {
            crate::recommendation::choose(
                estimate,
                row.native_supported,
                row.pack_supported && estimate.maximum_qualified,
                crate::recommendation::Policy::default(),
            )
        })
    }
    /// Where this game's Maximum Space store goes: the path the user entered,
    /// else `.flummox/<hash>.store` beside the install directory. The hash is
    /// the first 16 hex digits of the BLAKE3 of the install path.
    pub fn store_path(&self, game: &Game) -> PathBuf {
        if let Some(path) = self
            .pack_paths
            .get(&game.id.to_string())
            .filter(|path| !path.trim().is_empty())
        {
            return PathBuf::from(path);
        }
        use std::os::unix::ffi::OsStrExt;
        let identity: String = blake3::hash(game.install_dir.as_os_str().as_bytes())
            .to_hex()
            .chars()
            .take(16)
            .collect();
        game.install_dir
            .parent()
            .unwrap_or(&game.install_dir)
            .join(".flummox")
            .join(format!("{identity}.store"))
    }

    /// Builds the command that compresses `game` with `choice`.
    ///
    /// Standard is a native compression at the game's preset. Maximum builds
    /// a store and mounts it, keeping the original until the user confirms.
    /// A saved compatibility report for this build is attached when there is
    /// one, and the job proceeds without one when the user chose this mode.
    fn optimize_command(&self, game: Game, choice: StorageChoice) -> Result<Command, String> {
        let row = self
            .games
            .iter()
            .find(|r| r.game.id == game.id)
            .ok_or_else(|| "Game no longer exists".to_owned())?;
        match choice {
            StorageChoice::Standard => {
                if !row.native_supported {
                    return Err(format!(
                        "{}'s drive has no native compression. Open the game and choose Maximum.",
                        game.title
                    ));
                }
                let options = crate::backend::CompressOpts {
                    preset: self.preset_for(&game.id.to_string()),
                    ..Default::default()
                };
                Ok(Command::Enqueue {
                    game,
                    operation: Operation::Compress,
                    options,
                })
            }
            StorageChoice::Maximum => {
                if !row.pack_supported {
                    return Err(format!(
                        "Maximum is not available for {} on this drive.",
                        game.title
                    ));
                }
                let identity = self
                    .estimate(&game)
                    .and_then(|estimate| estimate.maximum_qualification);
                let report = self
                    .reports
                    .iter()
                    .find(|r| {
                        identity.is_some()
                            && r.identity().ok() == identity
                            && r.qualifies(
                                &game,
                                &r.corpus.sha256,
                                crate::compatibility::Policy::default(),
                            )
                    })
                    .cloned();
                Ok(Command::EnqueuePack {
                    task: PackTask::Activate {
                        store: self.store_path(&game),
                        create: true,
                        qualification: report.map(Box::new),
                    },
                    game,
                })
            }
        }
    }

    /// Predicted bytes the library could still save: the sum over supported,
    /// uncompressed games whose recommendation is to do something.
    pub fn potential_saving(&self) -> u64 {
        self.games
            .iter()
            .filter(|row| row.supported && !self.compressed(&row.game))
            // The library-wide action only ever uses Standard.
            .filter_map(|row| self.prospect(&row.game, StorageChoice::Standard))
            .sum()
    }

    /// Bytes saved so far. An estimate for natively compressed games.
    pub fn current_saving(&self) -> u64 {
        // One figure per installed game, from the same source its row shows.
        // A Maximum Space game whose original is still kept has saved nothing.
        self.games
            .iter()
            .filter_map(|row| self.result(&row.game))
            .map(|outcome| match outcome {
                Outcome::Estimated { saved, .. } => saved,
                Outcome::Measured { before, after } => before.saturating_sub(after),
                Outcome::AwaitingConfirm { .. } => 0,
            })
            .sum()
    }
    pub fn analysis_queuing(&self) -> bool {
        self.analysis_queuing
    }
    /// Whether the game counts as compressed at its installed build.
    ///
    /// True with a pack install. Otherwise the newest started job that is
    /// not an analysis decides: it must be a completed compression of this
    /// build. With no such job, a database record of this build decides.
    pub fn compressed(&self, game: &Game) -> bool {
        if self
            .snapshot
            .packs
            .iter()
            .any(|install| install.game_path == game.install_dir)
        {
            return true;
        }
        let action = self.snapshot.jobs.iter().rev().find(|j| {
            j.game.install_dir == game.install_dir
                && j.operation != Operation::Analyze
                && j.phase != Phase::Queued
        });
        match action {
            Some(j) => {
                j.operation == Operation::Compress
                    && j.phase == Phase::Completed
                    && j.game.build == game.build
            }
            None => self
                .records
                .iter()
                .any(|r| r.id == game.id && r.build == game.build),
        }
    }
    /// The Games list: rows that are not excluded and pass the search, the
    /// drive, launcher and status filters, in the chosen order. Ties sort by
    /// title.
    pub fn filtered(&self) -> Vec<&GameRow> {
        let query = self.query.to_lowercase();
        let mut games: Vec<_> = self
            .games
            .iter()
            .filter(|row| {
                !self
                    .snapshot
                    .excluded
                    .iter()
                    .any(|id| row.game.ids().any(|g| g.to_string() == *id))
                    && row.game.title.to_lowercase().contains(&query)
                    && self
                        .drive_filter
                        .as_ref()
                        .is_none_or(|p| row.mountpoint.as_ref() == Some(p))
                    && self
                        .launcher_filter
                        .as_ref()
                        .is_none_or(|l| row.game.id.launcher.label() == l)
                    && match self.filter {
                        Filter::All => true,
                        Filter::Updated => self.records.iter().any(|record| {
                            record.id == row.game.id && record.build != row.game.build
                        }),
                        Filter::Ready => {
                            row.supported && row.game.state.is_idle() && !self.compressed(&row.game)
                        }
                        Filter::Compressed => self.compressed(&row.game),
                        Filter::Attention => {
                            !row.supported
                                || self.latest(&row.game).is_some_and(|j| {
                                    matches!(
                                        j.phase,
                                        Phase::Failed | Phase::Partial | Phase::Interrupted
                                    )
                                })
                        }
                    }
            })
            .collect();
        games.sort_by(|a, b| {
            match self.sort {
                Sort::Name => a
                    .game
                    .title
                    .to_lowercase()
                    .cmp(&b.game.title.to_lowercase()),
                Sort::Size => b.game.size_hint.cmp(&a.game.size_hint),
                Sort::Worth => {
                    let (group_a, key_a) = self.worth(&a.game);
                    let (group_b, key_b) = self.worth(&b.game);
                    group_a.cmp(&group_b).then(key_b.cmp(&key_a))
                }
            }
            .then(a.game.title.cmp(&b.game.title))
        });
        games
    }
    /// The job for the bar under the page: the first one in progress, else
    /// the first one queued.
    pub fn active(&self) -> Option<&Job> {
        self.snapshot
            .jobs
            .iter()
            .find(|j| j.phase.active() && j.phase != Phase::Queued)
            .or_else(|| self.snapshot.jobs.iter().find(|j| j.phase == Phase::Queued))
    }

    /// Selected games a bulk action may queue: supported, idle and without
    /// an active pack job.
    pub fn actionable_selection(&self) -> Vec<Game> {
        self.games
            .iter()
            .filter(|row| {
                self.selected.contains(&row.game.id.to_string())
                    && row.supported
                    && row.game.state.is_idle()
                    && !self.pending.contains(&row.game.id.to_string())
            })
            .map(|row| row.game.clone())
            .collect()
    }
}

/// Everything `update` reacts to: user input, and the results of background
/// work. Variants carrying a `Result` are results. A `String` naming a game
/// is its `GameId::to_string()`.
#[derive(Debug, Clone)]
pub enum Message {
    GoTo(Page),
    /// Open Settings and scroll to the container with this id.
    Jump(&'static str),
    /// The measured position of a section, ready to scroll to.
    JumpOffset(f32),
    /// An artwork tile came into view and wants its image decoded.
    ArtworkVisible(super::artwork::Source),
    ArtworkLoaded(super::artwork::Source, Option<iced::widget::image::Handle>),
    ArtworkSaved(Result<(), String>),
    /// Rebuild the window's own data from the worker's current game list.
    Refresh,
    /// Ask the worker to discover games again.
    Rescan,
    Scanned(Result<ScanResult, String>),
    Snapshot(Result<Snapshot, String>),
    /// The worker's reply to a command the user gave.
    Commanded(Result<Snapshot, String>),
    /// A space plan is ready for review, with the command it belongs to.
    Planned(Result<(Command, crate::storage::SpacePlan), String>),
    /// The user accepted the plan in `State::planned`.
    StartPlanned,
    CancelPlanned,
    ExportDiagnostics,
    Qualify(String),
    QualificationReady(Result<crate::qualification::Wizard, String>),
    QualificationField(crate::qualification::Field, String),
    QualificationCheck(crate::qualification::Check, bool),
    QualificationMode(crate::compatibility::StorageMode),
    MeasureQualification,
    QualificationMeasured(Result<crate::allocation::Allocation, String>),
    SaveQualification,
    OpenChangelog,
    CloseQualification,
    QualificationSaved(Result<PathBuf, String>),
    DiagnosticsExported(Result<PathBuf, String>),
    AnalysisQueued(Result<Snapshot, String>),
    /// Hide the toast.
    Dismiss,
    /// A frame passed, or a scroll set from code finished. Drives the toast's
    /// deadline and removal.
    Tick,
    Query(String),
    Select(String, bool),
    /// Open this game's detail pane, or toggle it when it is the open one.
    Expand(String),
    /// Go to Games and open this game's detail pane.
    ReviewGame(String),
    /// Set how this game's Compress button compresses it.
    Choice(String, StorageChoice),
    Sort(Sort),
    /// Expand or collapse the "Little to gain" group.
    ToggleLow,
    Filter(Filter),
    /// Build 40 more rows of the Games list.
    ShowMore,
    /// Run this operation on every selected game that can take it.
    Queue(Operation),
    /// Compress every game whose recommendation says it is worthwhile.
    OptimizeLibrary,
    /// Run this operation on one game.
    One(String, Operation),
    /// Send a command to the worker. Pack maintenance commands are queued as
    /// pack jobs instead.
    Send(Command),
    Folder(String),
    AddFolder,
    FolderKind(FolderKind),
    /// Open a native picker.
    Browse(super::dialog::Target),
    /// The picker closed. `Ok(None)` is a cancel.
    Chosen(super::dialog::Target, Result<Option<PathBuf>, String>),
    ReportImported(Result<crate::compatibility::Report, String>),
    Preset(String, crate::backend::Preset),
    Keyboard(iced::keyboard::Event),
    Motion(MotionPreference),
    Theme(ThemePreference),
    SystemTheme(iced::theme::Mode),
    DriveFilter(Option<PathBuf>),
    LauncherFilter(Option<String>),
    /// The store path field of a game was edited.
    PackPath(String, String),
    /// Mount a store over a game. The flag says whether to create it first.
    PackActivate(String, bool),
    /// Create a store for a game without mounting it.
    PackCreate(String),
    /// Show the confirm step for reclaiming the retained original.
    PackReclaimPrompt(String),
    ToggleAdvanced(String),
}

/// Runs blocking work without occupying iced's executor or window thread.
///
/// Each call starts its own thread. The error is returned when that thread
/// ends without sending a result.
pub async fn background<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (send, receive) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _sent = send.send(f());
    });
    receive
        .await
        .map_err(|_| "The background task stopped unexpectedly.".into())
}
/// Sends a command to the worker, with a review step for the ones that need
/// disk space.
///
/// Compression, decompression and pack jobs are not sent here. A space plan
/// is computed and returned as `Message::Planned`, and the job is queued when
/// the user accepts it. Every other command goes straight to the worker.
fn send(command: Command) -> Task<Message> {
    if matches!(
        &command,
        Command::Enqueue {
            operation: Operation::Compress | Operation::Decompress,
            ..
        } | Command::EnqueuePack { .. }
    ) {
        return Task::perform(
            background(move || {
                let snapshot =
                    jobs::request(Command::Snapshot).map_err(|error| error.to_string())?;
                let plan =
                    jobs::space_plan(&command, &snapshot).map_err(|error| error.to_string())?;
                Ok((command, plan))
            }),
            |result| Message::Planned(result.and_then(|result| result)),
        );
    }
    send_unchecked(command)
}

/// Sends a command with no space plan and delivers the worker's reply as
/// `Message::Snapshot`.
fn send_unchecked(command: Command) -> Task<Message> {
    Task::perform(
        background(move || jobs::request(command).map_err(|e| e.to_string())),
        |result| Message::Commanded(result.and_then(|snapshot| snapshot)),
    )
}

/// Sends commands in order on one thread, with no space plan review, and
/// delivers the last reply. The first failure stops the batch; commands
/// already sent stay queued.
fn send_many(commands: Vec<Command>) -> Task<Message> {
    Task::perform(
        background(move || {
            let mut snapshot = jobs::request(Command::Snapshot).map_err(|e| e.to_string())?;
            for command in commands {
                snapshot = jobs::request(command).map_err(|e| e.to_string())?;
            }
            Ok(snapshot)
        }),
        |result| Message::Commanded(result.and_then(|snapshot| snapshot)),
    )
}

/// Queues one job for each game and shows the Jobs page. Several games at
/// once are always compressed with Standard: Maximum is chosen game by game.
/// A game that cannot take the job is left out and counted in a notice, so
/// one of them does not stop the rest.
fn queue_standard(state: &mut State, games: Vec<Game>, operation: Operation) -> Task<Message> {
    let wanted = games.len();
    let commands: Vec<Command> = games
        .into_iter()
        .filter_map(|game| {
            if operation == Operation::Compress {
                state.optimize_command(game, StorageChoice::Standard).ok()
            } else {
                Some(Command::Enqueue {
                    options: crate::backend::CompressOpts {
                        preset: state.preset_for(&game.id.to_string()),
                        ..Default::default()
                    },
                    game,
                    operation,
                })
            }
        })
        .collect();
    let skipped = wanted - commands.len();
    if skipped > 0 {
        state.show_status(Status::info(format!(
            "{skipped} game{} left out: their drive needs Maximum, which is chosen per game.",
            if skipped == 1 { " was" } else { "s were" }
        )));
    }
    if commands.is_empty() {
        return Task::none();
    }
    let navigation = update(state, Message::GoTo(Page::Queue));
    Task::batch([navigation, send_many(commands)])
}

/// Plans a job that mounts a store over the game, creating the store first
/// when `create` is set, and shows the queue. Does nothing unless the game is
/// idle and its drive supports stores.
fn pack_activate(state: &mut State, id: &str, create: bool) -> Task<Message> {
    let Some(game) = state
        .games
        .iter()
        .find(|row| row.game.id.to_string() == id && row.pack_supported && row.game.state.is_idle())
        .map(|row| row.game.clone())
    else {
        return Task::none();
    };
    let store = state.store_path(&game);
    let command = Command::EnqueuePack {
        game,
        task: PackTask::Activate {
            store,
            create,
            qualification: None,
        },
    };
    let navigation = update(state, Message::GoTo(Page::Queue));
    Task::batch([navigation, send(command)])
}

/// Gathers everything the pages show that the worker does not send: drive
/// probes, artwork, history and compatibility reports.
///
/// Blocks, so it runs through `background`. Only a failed worker request is
/// an error. Unreadable history, reports or artwork become warnings.
fn scan(env: Env) -> Result<ScanResult, String> {
    let snapshot = jobs::request(Command::Snapshot).map_err(|error| error.to_string())?;
    let worker_epoch = snapshot.worker_epoch;
    let generation = snapshot.scan_generation;
    let discovered = snapshot.discovered;
    let scanned_discovery = discovered.clone();
    let mut warnings = snapshot.scan_warnings;
    let artwork = super::artwork::Index::new(crate::launchers::steam::roots(&env));
    if let Err(error) = &artwork {
        warnings.push(format!("Artwork preferences unavailable: {error}"));
    }
    let games: Vec<_> = discovered
        .into_iter()
        .filter(|g| !g.is_tool)
        .map(|g| {
            let source = artwork.as_ref().ok().and_then(|index| index.source(&g));
            let cover = artwork.as_ref().ok().and_then(|index| index.cover(&g));
            GameRow::probe(g, source, cover)
        })
        .collect();
    // One entry per distinct mountpoint, in the order games first use it.
    let mut drives: Vec<Drive> = vec![];
    for row in &games {
        if let Some(path) = &row.mountpoint {
            if let Some(drive) = drives.iter_mut().find(|d| d.path == *path) {
                drive.games += 1;
            } else {
                drives.push(Drive {
                    path: path.clone(),
                    free: crate::backend::free_bytes(path).ok(),
                    games: 1,
                });
            }
        }
    }
    let mut records = vec![];
    let mut activity = vec![];
    if let Some(path) = Db::default_path() {
        match Db::open(&path).and_then(|db| Ok((db.games()?, db.recent_activity(50)?))) {
            Ok((r, a)) => {
                records = r;
                activity = a;
            }
            Err(e) => warnings.push(format!("History is unavailable: {e}")),
        }
    }
    let reports = match crate::compatibility::Store::local().and_then(|store| store.load()) {
        Ok(reports) => reports,
        Err(error) => {
            warnings.push(format!(
                "Compatibility reports could not be loaded: {error}"
            ));
            vec![]
        }
    };
    Ok(ScanResult {
        worker_epoch,
        generation,
        discovered: scanned_discovery,
        reports,
        games,
        drives,
        records,
        activity,
        warnings,
    })
}

/// Queues only newly visible games. Existing results and failures are kept
/// until a user requests another analysis or the launcher's build changes.
fn analyze_visible(state: &mut State) -> Task<Message> {
    if state.analysis_queuing || state.scanning {
        return Task::none();
    }
    // A game with any job for its installed build, whatever the outcome, is
    // left alone. At most 40 are queued per call.
    let games: Vec<_> = state
        .filtered()
        .into_iter()
        .take(state.shown)
        .filter(|row| {
            row.supported
                && row.game.state.is_idle()
                && !state.snapshot.jobs.iter().any(|job| {
                    job.game.install_dir == row.game.install_dir && job.game.build == row.game.build
                })
        })
        .take(40)
        .map(|row| row.game.clone())
        .collect();
    if games.is_empty() {
        return Task::none();
    }
    state.analysis_queuing = true;
    Task::perform(
        background(move || {
            let mut snapshot = jobs::request(Command::Snapshot).map_err(|e| e.to_string())?;
            for game in games {
                snapshot = jobs::request(Command::Enqueue {
                    game,
                    operation: Operation::Analyze,
                    options: Default::default(),
                })
                .map_err(|e| e.to_string())?;
            }
            Ok(snapshot)
        }),
        |result| Message::AnalysisQueued(result.and_then(|result| result)),
    )
}

/// Applies one message to the state and returns the work it starts.
///
/// Runs on the window thread, so anything that blocks goes through
/// `background` and comes back as another message. Arms that do not return
/// early fall through to `artwork_tasks`.
pub fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        // Compatibility qualification wizard.
        Message::Qualify(id) => {
            if let Some(game) = state
                .games
                .iter()
                .find(|row| row.game.id.to_string() == id && row.game.state.is_idle())
                .map(|row| row.game.clone())
            {
                return Task::perform(
                    background(move || {
                        crate::qualification::Wizard::start(game).map_err(|error| error.to_string())
                    }),
                    |result| Message::QualificationReady(result.and_then(|result| result)),
                );
            }
        }
        Message::QualificationReady(result) => match result {
            Ok(wizard) => state.qualification = Some(wizard),
            Err(error) => state.show_status(Status::error(error)),
        },
        Message::QualificationField(field, text) => {
            if let Some(wizard) = &mut state.qualification {
                wizard.field(field, text);
            }
        }
        Message::QualificationCheck(check, value) => {
            if let Some(wizard) = &mut state.qualification {
                wizard.check(check, value);
            }
        }
        Message::QualificationMode(mode) => {
            if let Some(wizard) = &mut state.qualification {
                wizard.mode = mode;
            }
        }
        Message::MeasureQualification => {
            if let Some(wizard) = &mut state.qualification {
                let game = &wizard.game.install_dir;
                let pack = state.snapshot.packs.iter().find(|p| p.game_path == *game);
                // A packed game is measured by its store and its updates
                // directory. A native one is measured by its install directory.
                let roots = match (pack, wizard.mode) {
                    (Some(pack), _) => vec![pack.store_path.clone(), pack.writes_path.clone()],
                    (None, crate::compatibility::StorageMode::MaximumSpace) => {
                        wizard.measured(Err("Activate Maximum Space for this game first".into()));
                        return Task::none();
                    }
                    (None, crate::compatibility::StorageMode::Native) => vec![game.clone()],
                };
                return Task::perform(
                    background(move || {
                        crate::allocation::measure(&roots).map_err(|error| format!("{error:#}"))
                    }),
                    |result| Message::QualificationMeasured(result.and_then(|result| result)),
                );
            }
        }
        Message::QualificationMeasured(result) => {
            if let Some(wizard) = &mut state.qualification {
                wizard.measured(result);
            }
        }
        Message::SaveQualification => {
            if let Some(wizard) = &state.qualification {
                match wizard.report() {
                    Ok(report) => {
                        return Task::perform(
                            background(move || {
                                crate::compatibility::Store::local()
                                    .and_then(|store| store.save(&report))
                                    .map_err(|error| error.to_string())
                            }),
                            |result| Message::QualificationSaved(result.and_then(|result| result)),
                        );
                    }
                    Err(error) => state.show_status(Status::error(error.to_string())),
                }
            }
        }
        Message::CloseQualification => state.qualification = None,
        Message::OpenChangelog => {
            if let Err(error) = super::open_changelog() {
                state.show_status(Status::error(format!(
                    "Could not open a browser ({error}). The changelog is at {}",
                    super::CHANGELOG_URL
                )));
            }
        }
        Message::QualificationSaved(result) => match result {
            Ok(path) => {
                state.qualification = None;
                state.show_status(Status::info(format!(
                    "Compatibility report saved to {}",
                    path.display()
                )));
                return update(state, Message::Refresh);
            }
            Err(error) => state.show_status(Status::error(error)),
        },
        // Space plan review.
        Message::Planned(result) => match result {
            // A plan with room to spare starts at once. The review appears
            // only when the plan fails its own check, to say what is short.
            Ok((command, plan)) if plan.check().is_ok() => {
                return send_unchecked(Command::EnqueuePlanned {
                    command: Box::new(command),
                    plan,
                });
            }
            Ok(plan) => state.planned = Some(plan),
            Err(error) => state.show_status(Status::error(error)),
        },
        Message::StartPlanned => {
            if let Some((command, plan)) = state.planned.take() {
                // The plan is checked again here. A plan that fails is dropped
                // along with its command.
                match plan.check() {
                    Ok(()) => {
                        return send_unchecked(Command::EnqueuePlanned {
                            command: Box::new(command),
                            plan,
                        });
                    }
                    Err(error) => state.show_status(Status::error(error.to_string())),
                }
            }
        }
        Message::CancelPlanned => state.planned = None,
        Message::ExportDiagnostics => {
            let snapshot = state.snapshot.clone();
            // Writes the current snapshot as `diagnostics.json` in the data
            // directory, through a temporary file renamed into place.
            return Task::perform(
                background(move || {
                    let root = crate::libraries::data_dir().map_err(|error| error.to_string())?;
                    crate::libraries::private_dir(&root).map_err(|error| error.to_string())?;
                    let mut file = tempfile::NamedTempFile::new_in(&root)
                        .map_err(|error| error.to_string())?;
                    serde_json::to_writer_pretty(&mut file, &snapshot)
                        .map_err(|error| error.to_string())?;
                    file.as_file()
                        .sync_all()
                        .map_err(|error| error.to_string())?;
                    let path = root.join("diagnostics.json");
                    file.persist(&path).map_err(|error| error.to_string())?;
                    Ok(path)
                }),
                |result| Message::DiagnosticsExported(result.and_then(|result| result)),
            );
        }
        Message::DiagnosticsExported(result) => match result {
            Ok(path) => state.show_status(Status::info(format!(
                "Diagnostics saved to {}. They include local folder paths.",
                path.display()
            ))),
            Err(error) => state.show_status(Status::error(error)),
        },
        // Artwork. Decodes are started by `artwork_tasks` after the match.
        Message::ArtworkVisible(source) => state.artwork_cache.request(source),
        Message::ArtworkLoaded(source, image) => state.artwork_cache.loaded(source, image),
        Message::ArtworkSaved(result) => match result {
            Ok(()) => return update(state, Message::Refresh),
            Err(error) => state.show_status(Status::error(error)),
        },
        // Navigation and scrolling.
        Message::Jump(section) => {
            // `GoTo` can return artwork decodes it has already marked as
            // pending. Dropping that task left them pending for good, and
            // with two decodes allowed at once artwork loading could stall.
            let navigation = update(state, Message::GoTo(Page::Settings));
            return navigation.chain(super::surface::jump(section, Message::JumpOffset));
        }
        Message::JumpOffset(offset) => {
            state.scroll_redraw_until = Some(Instant::now() + Duration::from_millis(150));
            return iced::widget::operation::scroll_to(
                "Settings",
                iced::widget::operation::AbsoluteOffset {
                    x: None,
                    y: Some(offset),
                },
            )
            .chain(Task::done(Message::Tick));
        }
        Message::GoTo(destination) => {
            let page = destination.main();
            let changed = state.page.main() != page;
            // A real page change restarts the reveal animation, sliding in the
            // direction of the move through the sidebar.
            if changed {
                state.page_direction = if page.rank() < state.page.rank() {
                    -1.0
                } else {
                    1.0
                };
                state.page = page;
                if page == Page::Games {
                    state.capture_order();
                }
                state.page_reveal = Animation::new(false)
                    .duration(state.motion_duration(180, 120))
                    .easing(state.motion_easing())
                    .go(true, Instant::now());
            }
            state.confirm_reclaim.clear();
            for (target, animation) in &mut state.nav {
                animation.go_mut(*target == page, Instant::now());
            }
            // A section destination scrolls to its section. Any other page
            // change returns to where that page was last scrolled.
            if let Some(section) = destination.section() {
                return super::surface::jump(section, Message::JumpOffset);
            }
            if changed {
                let offset = super::surface::recorded(&state.scroll_positions, page.label());
                return iced::widget::operation::scroll_to(
                    page.label(),
                    iced::widget::operation::AbsoluteOffset {
                        x: None,
                        y: Some(offset),
                    },
                )
                .chain(Task::done(Message::Tick));
            }
        }
        // Scanning and worker snapshots.
        Message::Rescan => return send(Command::RefreshDiscovery),
        Message::Refresh => {
            if state.scanning {
                return Task::none();
            }
            state.scanning = true;
            let env = state.env.clone();
            return Task::perform(background(move || scan(env)), |result| {
                Message::Scanned(result.and_then(|result| result))
            });
        }
        Message::Scanned(result) => {
            state.scanning = false;
            match result {
                Ok(scan) => {
                    // The worker restarted, rescanned or changed its game list
                    // while this scan ran, so the result describes an older
                    // list. Scan again.
                    if scan.worker_epoch > 0
                        && state.snapshot_loaded
                        && (scan.worker_epoch != state.snapshot.worker_epoch
                            || scan.generation != state.snapshot.scan_generation
                            || scan.discovered != state.snapshot.discovered)
                    {
                        return update(state, Message::Refresh);
                    }
                    state.games = scan.games;
                    state.reports = scan.reports;
                    state.drives = scan.drives;
                    state.records = scan.records;
                    state.capture_order();
                    state.activity = scan.activity;
                    state.warnings = scan.warnings;
                    // Drop per-game interface state for games that are gone.
                    // A selected game must also still be supported.
                    let valid: std::collections::HashSet<_> = state
                        .games
                        .iter()
                        .map(|row| row.game.id.to_string())
                        .collect();
                    state.selected.retain(|id| {
                        valid.contains(id)
                            && state
                                .games
                                .iter()
                                .any(|row| row.game.id.to_string() == *id && row.supported)
                    });
                    state.advanced.retain(|id| valid.contains(id));
                    state.pack_paths.retain(|id, _| valid.contains(id));
                    state.confirm_reclaim.retain(|id| valid.contains(id));
                    if state
                        .expanded
                        .as_ref()
                        .is_some_and(|id| !valid.contains(id))
                    {
                        state.expanded = None;
                    }
                }
                Err(e) => state.show_status(Status::error(e)),
            }
            return analyze_visible(state);
        }
        Message::AnalysisQueued(result) => {
            state.analysis_queuing = false;
            return update(state, Message::Snapshot(result));
        }

        // The reply to something the user asked for. A refusal is about that
        // request, so it is shown for a few seconds and the connection is
        // not marked as lost.
        Message::Commanded(result) => match result {
            Ok(snapshot) => return update(state, Message::Snapshot(Ok(snapshot))),
            Err(refusal) => {
                state.show_status(Status::error(refusal));
                state.status_deadline = Some(Instant::now() + Duration::from_secs(8));
            }
        },
        Message::Snapshot(result) => match result {
            Ok(snapshot) => {
                // Replies can arrive out of order. One from an earlier worker,
                // or from this worker at a revision no newer than the one
                // shown, is ignored.
                if snapshot.worker_epoch > 0
                    && state.snapshot.worker_epoch > 0
                    && (snapshot.worker_epoch < state.snapshot.worker_epoch
                        || (snapshot.worker_epoch == state.snapshot.worker_epoch
                            && snapshot.revision <= state.snapshot.revision))
                {
                    return Task::none();
                }
                state.snapshot_loaded = true;
                state.connection_error = None;
                let libraries_changed = state.snapshot.libraries != snapshot.libraries;
                let discovery_changed = state.snapshot.discovered != snapshot.discovered;
                // A job other than an analysis that was active in the old
                // snapshot and has stopped in the new one.
                let completed_work = snapshot.jobs.iter().any(|job| {
                    job.operation != Operation::Analyze
                        && !job.phase.active()
                        && state
                            .snapshot
                            .jobs
                            .iter()
                            .any(|old| old.id == job.id && old.phase.active())
                });
                // Progress bars ease towards each new fraction. A fraction
                // lower than the one drawn is shown at once, with no animation.
                for job in &snapshot.jobs {
                    let fraction = if job.bytes_total > 0 {
                        (job.bytes_done as f32 / job.bytes_total as f32).clamp(0., 1.)
                    } else if job.files_total > 0 {
                        (job.files_done as f32 / job.files_total as f32).clamp(0., 1.)
                    } else {
                        0.
                    };
                    let animation = state.progress.entry(job.id).or_insert_with(|| {
                        Animation::new(fraction)
                            .duration(Duration::from_millis(250))
                            .easing(Easing::EaseOutCubic)
                    });
                    if fraction < animation.value() {
                        *animation = Animation::new(fraction).duration(Duration::from_millis(250));
                    } else {
                        animation.go_mut(fraction, Instant::now());
                    }
                }
                state
                    .progress
                    .retain(|id, _| snapshot.jobs.iter().any(|j| j.id == *id));
                state.reduced_motion = snapshot.reduced_motion;
                state.motion = snapshot.motion;
                state.theme = snapshot.theme;
                state.pending = snapshot
                    .jobs
                    .iter()
                    .filter(|job| job.operation == Operation::Pack && job.phase.active())
                    .map(|job| job.game.id.to_string())
                    .collect();
                let analyzing = state.analysis_active();
                let first = !state.snapshot_loaded_before;
                state.snapshot_loaded_before = true;
                state.snapshot = snapshot;
                // Estimates settle when the last analysis ends. The list is
                // sorted then, unless that would move a row the user has open
                // or ticked.
                if first || (analyzing && !state.analysis_active()) {
                    if state.expanded.is_some() || !state.selected.is_empty() {
                        state.order_stale = true;
                    } else {
                        state.capture_order();
                    }
                }
                state.polling = true;
                // A changed library list, a changed game list or finished work
                // all mean the scanned data is out of date.
                if libraries_changed {
                    state.show_status(Status::info("Library settings saved."));
                    return update(state, Message::Refresh);
                }
                if completed_work || discovery_changed {
                    return update(state, Message::Refresh);
                }
                return analyze_visible(state);
            }
            Err(e) => {
                state.connection_error = Some(e.clone());
                state.polling = true;
                state.show_status(Status::error(e));
            }
        },
        // The Games list.
        Message::Query(query) => {
            state.query = query;
            state.shown = 40;
            state.capture_order();
        }
        Message::Select(id, selected) => {
            let actionable = state.games.iter().any(|row| {
                row.game.id.to_string() == id
                    && row.supported
                    && row.game.state.is_idle()
                    && !state.pending.contains(&id)
            });
            if selected && actionable {
                state.selected.insert(id);
            } else {
                state.selected.remove(&id);
            }
        }
        Message::ReviewGame(id) => {
            let navigation = update(state, Message::GoTo(Page::Games));
            return Task::batch([navigation, update(state, Message::Expand(id))]);
        }
        Message::Expand(id) => {
            state.confirm_reclaim.clear();
            if state.expanded.as_ref() == Some(&id) {
                state.detail.go_mut(!state.detail.value(), Instant::now());
            } else {
                state.expanded = Some(id);
                state.detail = Animation::new(false)
                    .duration(state.motion_duration(260, 150))
                    .easing(state.motion_easing())
                    .go(true, Instant::now());
            }
        }
        Message::Sort(sort) => {
            state.sort = sort;
            state.capture_order();
        }
        Message::ToggleLow => state.show_low = !state.show_low,
        Message::Choice(id, choice) => {
            state.choices.insert(id, choice);
            // The predicted saving differs by mode, so the game may belong
            // in another group now.
            if state.expanded.is_some() {
                state.order_stale = true;
            } else {
                state.capture_order();
            }
        }
        Message::Filter(filter) => {
            state.filter = filter;
            state.capture_order();
        }
        Message::ShowMore => state.shown += 40,
        // Queueing work.
        Message::One(id, operation) => {
            if state.pending.contains(&id) {
                return Task::none();
            }
            if let Some(game) = state
                .games
                .iter()
                .find(|g| g.game.id.to_string() == id && g.supported)
                .map(|row| row.game.clone())
            {
                // Compress follows the recommendation, which may choose a pack
                // job. Analyze and Decompress are queued as asked.
                if operation == Operation::Compress {
                    let choice = state.choice_for(&game);
                    match state.optimize_command(game, choice) {
                        // The row and the work bar both show the job, so the
                        // window stays where the button was pressed.
                        Ok(command) => return send(command),
                        Err(error) => {
                            state.show_status(Status::error(error));
                            return Task::none();
                        }
                    }
                }
                let options = crate::backend::CompressOpts {
                    preset: state.preset_for(&id),
                    ..Default::default()
                };
                return send(Command::Enqueue {
                    game,
                    operation,
                    options,
                });
            }
        }
        Message::Queue(operation) => {
            let games = state.actionable_selection();
            return queue_standard(state, games, operation);
        }
        Message::OptimizeLibrary => {
            let games: Vec<Game> = state
                .games
                .iter()
                .filter(|row| {
                    row.supported
                        && row.game.state.is_idle()
                        && !state.compressed(&row.game)
                        && state
                            .prospect(&row.game, StorageChoice::Standard)
                            .is_some_and(|saving| saving > 0)
                })
                .map(|row| row.game.clone())
                .collect();
            if games.is_empty() {
                state.show_status(Status::info(
                    "Analysis has not found a game worth compressing yet.",
                ));
            } else {
                return queue_standard(state, games, Operation::Compress);
            }
        }
        Message::Send(command) => {
            // Compact, prune, restore and reclaim are turned into pack jobs
            // for the game at that path, so they pass through `send` and its
            // space plan.
            let pack = match &command {
                Command::PackCompact { game_path } => Some((game_path, PackTask::Compact)),
                Command::PackPrune { game_path } => Some((game_path, PackTask::Prune)),
                Command::PackRollback { game_path } => Some((game_path, PackTask::Restore)),
                Command::PackReclaim { game_path } => Some((game_path, PackTask::Reclaim)),
                _ => None,
            };
            if let Some((path, task)) = pack {
                if let Some(game) = state
                    .games
                    .iter()
                    .find(|row| row.game.install_dir == *path)
                    .map(|row| row.game.clone())
                {
                    let navigation = update(state, Message::GoTo(Page::Queue));
                    return Task::batch([navigation, send(Command::EnqueuePack { game, task })]);
                }
                state.show_status(Status::error("The selected game is no longer installed."));
                return Task::none();
            }
            return send(command);
        }
        // Native pickers and what is done with the chosen path.
        Message::Browse(target) => {
            if state.picker_busy {
                return Task::none();
            }
            state.picker_busy = true;
            let dialog_target = target.clone();
            return Task::perform(
                background(move || super::dialog::choose(&dialog_target)),
                move |result| Message::Chosen(target.clone(), result.and_then(|path| path)),
            );
        }
        Message::Chosen(target, result) => {
            state.picker_busy = false;
            match result {
                Ok(Some(path)) => match target {
                    super::dialog::Target::Game => {
                        if let Some(path) = path.to_str() {
                            state.folder = path.to_owned();
                            state.folder_error = None;
                        } else {
                            state.show_status(Status::error("This path cannot be shown in the folder field. Add it through the command line."));
                        }
                    }
                    super::dialog::Target::Storage(id) => {
                        let Some(game) = state
                            .games
                            .iter()
                            .find(|row| row.game.id.to_string() == id)
                            .map(|row| row.game.clone())
                        else {
                            return Task::none();
                        };
                        // Keep the store's file name and move it to the chosen
                        // folder.
                        let name = state
                            .store_path(&game)
                            .file_name()
                            .map(|n| n.to_os_string())
                            .unwrap_or_else(|| "game.store".into());
                        let destination = path.join(name);
                        if let Some(path) = destination.to_str() {
                            state.pack_paths.insert(id, path.to_owned());
                        } else {
                            state.show_status(Status::error("This path cannot be shown in the storage field. Choose another folder."));
                        }
                    }
                    super::dialog::Target::Artwork(game) => {
                        return Task::perform(
                            background(move || {
                                super::artwork::save_override(game, path)
                                    .map_err(|error| error.to_string())
                            }),
                            |result| Message::ArtworkSaved(result.and_then(|result| result)),
                        );
                    }
                    super::dialog::Target::Report => {
                        // Read at most 1 MiB, parse it, and save it into the
                        // local report store.
                        return Task::perform(
                            background(move || {
                                let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
                                use std::io::Read;
                                let mut bytes = Vec::new();
                                file.take(1024 * 1024 + 1)
                                    .read_to_end(&mut bytes)
                                    .map_err(|e| e.to_string())?;
                                if bytes.len() > 1024 * 1024 {
                                    return Err("Compatibility report exceeds 1 MiB.".into());
                                }
                                let report: crate::compatibility::Report =
                                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                                crate::compatibility::Store::local()
                                    .and_then(|store| store.save(&report))
                                    .map_err(|e| e.to_string())?;
                                Ok(report)
                            }),
                            |result| Message::ReportImported(result.and_then(|report| report)),
                        );
                    }
                },
                Ok(None) => {}
                Err(error) => state.show_status(Status::error(error)),
            }
        }
        Message::ReportImported(result) => match result {
            Ok(report) => {
                let game = state
                    .games
                    .iter()
                    .find(|row| report.game.matches(&row.game))
                    .map(|row| row.game.clone());
                state.reports.push(report);
                state.show_status(Status::info("Report imported. Analysis will verify the installed files before enabling Maximum Space."));
                if let Some(game) = game {
                    return send(Command::Enqueue {
                        game,
                        operation: Operation::Analyze,
                        options: Default::default(),
                    });
                }
            }
            Err(error) => state.show_status(Status::error(error)),
        },
        // Locations and preferences.
        Message::Folder(folder) => {
            state.folder = folder;
            state.folder_error = None;
        }
        Message::FolderKind(kind) => state.folder_kind = kind,
        Message::AddFolder => {
            let Some(env) = Env::current() else {
                state.folder_error = Some("Cannot locate your home folder.".into());
                return Task::none();
            };
            let folder = jobs::folder_path(&state.folder, &env.home);
            if !folder.is_dir() || folder.parent().is_none() {
                state.folder_error = Some("Choose an existing game folder.".into());
                return Task::none();
            }
            state.folder_error = None;
            return send(Command::Library(Library {
                path: folder,
                automatic: false,
                custom: true,
                folder_kind: state.folder_kind,
            }));
        }
        Message::Preset(id, preset) => {
            state.presets.insert(id, preset);
        }
        Message::Motion(motion) => {
            state.motion = motion;
            state.reduced_motion = motion == MotionPreference::Reduced;
            // The sidebar animations are rebuilt at their current value with
            // the new timing. The setting is also sent to the worker.
            let duration = state.motion_duration(220, 140);
            let easing = state.motion_easing();
            for (_, animation) in &mut state.nav {
                *animation = Animation::new(animation.value())
                    .duration(duration)
                    .easing(easing);
            }
            return send(Command::Motion(motion));
        }
        Message::Theme(theme) => {
            state.theme = theme;
            return send(Command::Theme(theme));
        }
        Message::SystemTheme(theme) => state.system_theme = theme,
        Message::DriveFilter(path) => {
            state.drive_filter = path;
            state.capture_order();
        }
        Message::LauncherFilter(launcher) => {
            state.launcher_filter = launcher;
            state.capture_order();
        }
        // Maximum Space storage.
        Message::PackPath(id, path) => {
            state.pack_paths.insert(id, path);
        }
        Message::PackActivate(id, create) => return pack_activate(state, &id, create),
        Message::PackCreate(id) => {
            if let Some(game) = state
                .games
                .iter()
                .find(|row| {
                    row.game.id.to_string() == id && row.pack_supported && row.game.state.is_idle()
                })
                .map(|row| row.game.clone())
            {
                let store = state.store_path(&game);
                let navigation = update(state, Message::GoTo(Page::Queue));
                return Task::batch([
                    navigation,
                    send(Command::EnqueuePack {
                        game,
                        task: PackTask::Create { store },
                    }),
                ]);
            }
        }
        // Only one confirm step is open at a time.
        Message::PackReclaimPrompt(id) => {
            state.confirm_reclaim.clear();
            state.confirm_reclaim.insert(id);
        }
        Message::ToggleAdvanced(id) => {
            if !state.advanced.remove(&id) {
                state.advanced.insert(id);
            }
        }
        // The toast and the keyboard.
        Message::Dismiss => state.dismiss_status(),
        Message::Tick => {
            // Start hiding at the deadline, then remove the toast once the
            // hide animation has finished.
            if state
                .status_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                state.dismiss_status();
            }
            if state.status.is_some()
                && !state.status_reveal.value()
                && !state.status_reveal.is_animating(Instant::now())
            {
                state.status = None;
            }
        }
        Message::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            use iced::keyboard::{Key, key::Named};
            // Tab moves focus, Escape closes the detail pane and clears the
            // selection, and the command key with F or R searches or rescans.
            match key {
                Key::Named(Named::Tab) => {
                    return if modifiers.shift() {
                        iced::widget::operation::focus_previous()
                    } else {
                        iced::widget::operation::focus_next()
                    };
                }
                Key::Named(Named::Escape) => {
                    state.detail.go_mut(false, Instant::now());
                    state.selected.clear();
                }
                Key::Character(key) if modifiers.command() && key.as_str() == "f" => {
                    let navigation = update(state, Message::GoTo(Page::Games));
                    return Task::batch([
                        navigation,
                        iced::widget::operation::focus(iced::widget::Id::new("game-search")),
                    ]);
                }
                Key::Character(key) if modifiers.command() && key.as_str() == "r" => {
                    return update(state, Message::Rescan);
                }
                _ => {}
            }
        }
        Message::Keyboard(_) => {}
    }
    artwork_tasks(state)
}

/// Starts a background decode for each source the cache hands out. A failed
/// decode is reported as `None`, which the cache records.
fn artwork_tasks(state: &mut State) -> Task<Message> {
    let mut tasks = vec![];
    while let Some(source) = state.artwork_cache.next() {
        let key = source.clone();
        tasks.push(Task::perform(
            background(move || source.decode().ok()),
            move |result| Message::ArtworkLoaded(key.clone(), result.ok().flatten()),
        ));
    }
    Task::batch(tasks)
}

/// An endless stream of worker snapshots, each requested one second after
/// the previous reply. A failed request is delivered as an error and the
/// stream continues.
pub fn polls() -> impl iced::futures::Stream<Item = Message> {
    iced::futures::stream::unfold((), |_| async {
        let result = background(|| {
            std::thread::sleep(Duration::from_secs(1));
            jobs::request(Command::Snapshot).map_err(|e| e.to_string())
        })
        .await;
        Some((Message::Snapshot(result.and_then(|r| r)), ()))
    })
}

impl std::fmt::Display for Sort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}
impl std::fmt::Display for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{GameId, InstallState, Launcher},
        testutil::{Ctx, TestResult, check, check_eq},
    };

    fn game() -> Game {
        Game {
            id: GameId::new(Launcher::Manual, "fixture"),
            also: vec![],
            title: "Fixture".into(),
            install_dir: "/fixture/game".into(),
            build: Some("1".into()),
            size_hint: Some(10_000),
            state: InstallState::Idle,
            is_tool: false,
        }
    }

    fn row(supported: bool) -> GameRow {
        GameRow {
            game: game(),
            filesystem: "fixturefs".into(),
            mountpoint: Some("/fixture".into()),
            supported,
            native_supported: supported,
            pack_supported: false,
            note: (!supported).then(|| "Unavailable".into()),
            artwork: None,
            cover: None,
        }
    }

    #[test]
    fn navigation_and_stale_responses_preserve_jobs_and_context() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(row(true));
        state.query = "Fixture".into();
        state.selected.insert(game().id.to_string());
        state.snapshot = Snapshot {
            worker_epoch: 10,
            revision: 5,
            jobs: vec![Job {
                id: 42,
                game: game(),
                operation: Operation::Compress,
                options: Default::default(),
                phase: Phase::Running,
                files_done: 1,
                bytes_done: 10,
                files_total: 10,
                bytes_total: 100,
                estimate: None,
                message: "Compressing".into(),
                errors: vec![],
                created: 0,
                elapsed: 1,
                drive_change: None,
                user_paused: false,
                pack: None,
                pack_interruptible: false,
                space_plan: None,
            }],
            discovered: vec![game()],
            ..Default::default()
        };
        let _task = update(&mut state, Message::GoTo(Page::Games));
        check_eq(
            state.page_direction,
            1.0,
            "later page moves upward into view",
        )?;
        let _task = update(&mut state, Message::GoTo(Page::Overview));
        check_eq(
            state.page_direction,
            -1.0,
            "earlier page moves downward into view",
        )?;
        let _task = update(&mut state, Message::GoTo(Page::Queue));
        check_eq(state.page, Page::Queue, "jobs have their own page")?;
        let _task = update(&mut state, Message::Snapshot(Err("offline".into())));
        check(
            state.connection_error.is_some(),
            "connection failure is visible",
        )?;
        let _task = update(
            &mut state,
            Message::Snapshot(Ok(Snapshot {
                worker_epoch: 10,
                revision: 4,
                ..Default::default()
            })),
        );
        check_eq(
            state.snapshot.revision,
            5,
            "late response cannot overwrite current jobs",
        )?;
        check_eq(
            state.snapshot.discovered.len(),
            1,
            "late empty response does not clear content",
        )?;
        let _task = update(
            &mut state,
            Message::Snapshot(Ok(Snapshot {
                worker_epoch: 9,
                revision: 100,
                ..Default::default()
            })),
        );
        check_eq(
            state.snapshot.worker_epoch,
            10,
            "previous worker response is discarded",
        )?;
        check_eq(
            state.snapshot.jobs.first().ctx("retained job")?.phase,
            Phase::Running,
            "running job survives navigation, disconnect and old responses",
        )?;
        let _task = update(&mut state, Message::Scanned(Err("scan failed".into())));
        check_eq(state.games.len(), 1, "failed scan retains current games")?;
        check_eq(
            state.query.as_str(),
            "Fixture",
            "navigation preserves search",
        )?;
        check(
            state.selected.contains(&game().id.to_string()),
            "navigation preserves selection",
        )
    }

    #[test]
    fn completed_compression_consumes_the_estimate_and_an_update_invalidates_status() -> TestResult
    {
        let mut state = State::new(Env::from_home("/fixture"));
        let mut game = game();
        let analysis = Job {
            id: 1,
            game: game.clone(),
            operation: Operation::Analyze,
            options: Default::default(),
            phase: Phase::Completed,
            files_done: 0,
            bytes_done: 0,
            files_total: 0,
            bytes_total: 0,
            estimate: Some(crate::estimate::Estimate {
                disk_now: 10_000,
                disk_after: 2_000,
                ..Default::default()
            }),
            message: String::new(),
            errors: vec![],
            created: 0,
            elapsed: 0,
            drive_change: None,
            user_paused: false,
            pack: None,
            pack_interruptible: false,
            space_plan: None,
        };
        state.snapshot.jobs.push(analysis.clone());
        check(state.estimate(&game).is_some(), "fresh analysis is shown")?;
        state.snapshot.jobs.push(Job {
            id: 2,
            operation: Operation::Compress,
            ..analysis
        });
        check(
            state.estimate(&game).is_none(),
            "consumed saving is not promised again",
        )?;
        check(state.compressed(&game), "completed pass is visible")?;
        game.build = Some("2".into());
        check(!state.compressed(&game), "an update needs another check")
    }

    fn named(key: &str, supported: bool) -> GameRow {
        let mut row = row(supported);
        row.game.id = GameId::new(Launcher::Manual, key);
        row.game.title = key.into();
        row.game.install_dir = format!("/fixture/{key}").into();
        row.game.size_hint = Some(4_000_000_000);
        row
    }

    fn job(id: i64, game: &Game, operation: Operation, phase: Phase, saving: u64) -> Job {
        Job {
            id,
            game: game.clone(),
            operation,
            options: Default::default(),
            phase,
            files_done: 0,
            bytes_done: 0,
            files_total: 0,
            bytes_total: 0,
            estimate: (saving > 0).then(|| crate::estimate::Estimate {
                install_bytes: 4_000_000_000,
                bytes: 4_000_000_000,
                disk_now: 4_000_000_000,
                disk_after: 4_000_000_000 - saving,
                files: 10,
                inspected_files: 10,
                sampled: 64 * 1024 * 1024,
                ..Default::default()
            }),
            message: String::new(),
            errors: vec![],
            created: 0,
            elapsed: 0,
            drive_change: None,
            user_paused: false,
            pack: None,
            pack_interruptible: false,
            space_plan: None,
        }
    }

    fn order(state: &State) -> Vec<String> {
        state
            .filtered()
            .iter()
            .map(|row| row.game.title.clone())
            .collect()
    }

    #[test]
    fn games_worth_compressing_come_first_and_results_are_reported() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        for (key, supported) in [
            ("unsupported", false),
            ("done", true),
            ("unknown", true),
            ("small-win", true),
            ("big-win", true),
        ] {
            state.games.push(named(key, supported));
        }
        let game = |key: &str| named(key, true).game;
        state.snapshot.jobs = vec![
            job(
                1,
                &game("done"),
                Operation::Compress,
                Phase::Completed,
                1_500_000_000,
            ),
            job(
                2,
                &game("small-win"),
                Operation::Analyze,
                Phase::Completed,
                500_000_000,
            ),
            job(
                3,
                &game("big-win"),
                Operation::Analyze,
                Phase::Completed,
                2_000_000_000,
            ),
        ];
        check_eq(state.sort, Sort::Worth, "the list opens sorted by worth")?;
        state.capture_order();
        check_eq(
            order(&state),
            ["big-win", "small-win", "unknown", "done", "unsupported"]
                .map(String::from)
                .to_vec(),
            "biggest saving first, then not analyzed, compressed, and the rest",
        )?;
        check_eq(
            state.result(&game("done")),
            Some(Outcome::Estimated {
                installed: 4_000_000_000,
                saved: 1_500_000_000,
            }),
            "a compressed game reports what its pass was estimated to save",
        )?;
        check_eq(
            state.result(&game("big-win")),
            None,
            "a game that is only analyzed has no result",
        )
    }

    #[test]
    fn compress_uses_standard_unless_the_game_was_set_to_maximum() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let mut both = named("both", true);
        both.pack_supported = true;
        let mut store_only = named("store-only", true);
        store_only.native_supported = false;
        store_only.pack_supported = true;
        state.games.push(both.clone());
        state.games.push(store_only.clone());
        let mut analysis = job(
            1,
            &both.game,
            Operation::Analyze,
            Phase::Completed,
            900_000_000,
        );
        if let Some(estimate) = &mut analysis.estimate {
            estimate.maximum_after = Some(2_000_000_000);
        }
        state.snapshot.jobs.push(analysis);

        check_eq(
            state.choice_for(&both.game),
            StorageChoice::Standard,
            "a drive with native compression defaults to Standard",
        )?;
        check_eq(
            state.choice_for(&store_only.game),
            StorageChoice::Maximum,
            "a drive without it has only Maximum",
        )?;
        check_eq(
            state.prospect(&both.game, StorageChoice::Standard),
            Some(900_000_000),
            "Standard's predicted saving",
        )?;
        check_eq(
            state.prospect(&both.game, StorageChoice::Maximum),
            Some(2_000_000_000),
            "Maximum's predicted saving",
        )?;
        check(
            matches!(
                state.optimize_command(both.game.clone(), state.choice_for(&both.game)),
                Ok(Command::Enqueue {
                    operation: Operation::Compress,
                    ..
                })
            ),
            "the default is a native compression",
        )?;
        let _task = update(
            &mut state,
            Message::Choice(both.game.id.to_string(), StorageChoice::Maximum),
        );
        check(
            matches!(
                state.optimize_command(both.game.clone(), state.choice_for(&both.game)),
                Ok(Command::EnqueuePack {
                    task: PackTask::Activate {
                        create: true,
                        qualification: None,
                        ..
                    },
                    ..
                })
            ),
            "a game set to Maximum builds and mounts a store, with no report needed",
        )?;
        check(
            state
                .optimize_command(store_only.game.clone(), StorageChoice::Standard)
                .is_err(),
            "Standard is refused where the drive cannot do it",
        )?;
        // Several games at once use Standard, and the one that cannot is
        // left out without stopping the other.
        state.selected.insert(both.game.id.to_string());
        state.selected.insert(store_only.game.id.to_string());
        let _task = update(&mut state, Message::Queue(Operation::Compress));
        check(
            state
                .status
                .as_ref()
                .is_some_and(|status| status.text.contains("1 game was left out")),
            format!(
                "the skipped game is reported: {:?}",
                state.status.as_ref().map(|s| &s.text)
            ),
        )
    }

    #[test]
    fn estimates_arriving_while_a_row_is_open_do_not_move_it() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(named("alpha", true));
        state.games.push(named("beta", true));
        let beta = named("beta", true).game;
        let running = Snapshot {
            worker_epoch: 1,
            revision: 1,
            jobs: vec![job(1, &beta, Operation::Analyze, Phase::Analyzing, 0)],
            ..Snapshot::default()
        };
        let _task = update(&mut state, Message::Snapshot(Ok(running)));
        check_eq(
            order(&state),
            ["alpha", "beta"].map(String::from).to_vec(),
            "control: with no estimates the order is by title",
        )?;
        state.expanded = Some("manual:alpha".into());
        let finished = Snapshot {
            worker_epoch: 1,
            revision: 2,
            jobs: vec![job(
                1,
                &beta,
                Operation::Analyze,
                Phase::Completed,
                2_000_000_000,
            )],
            ..Snapshot::default()
        };
        let _task = update(&mut state, Message::Snapshot(Ok(finished)));
        check(state.order_stale, "the page offers to sort again")?;
        check_eq(
            order(&state),
            ["alpha", "beta"].map(String::from).to_vec(),
            "rows hold still while a game is open",
        )?;
        let _task = update(&mut state, Message::Sort(Sort::Worth));
        check(!state.order_stale, "sorting clears the offer")?;
        check_eq(
            order(&state),
            ["beta", "alpha"].map(String::from).to_vec(),
            "the game with a saving moves up when asked",
        )
    }

    #[test]
    fn search_and_selection_survive_background_snapshots() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(row(true));
        let _task = update(&mut state, Message::Query("Fixture".into()));
        let _task = update(&mut state, Message::Select("manual:fixture".into(), true));
        let _task = update(&mut state, Message::Snapshot(Ok(Snapshot::default())));
        check_eq(state.query.as_str(), "Fixture", "query stays put")?;
        check(
            state.selected.contains("manual:fixture"),
            "selection stays put",
        )
    }

    #[test]
    fn unsupported_and_pending_games_cannot_enter_bulk_actions() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(row(false));
        let id = "manual:fixture".to_owned();
        let _task = update(&mut state, Message::Select(id.clone(), true));
        check(
            state.selected.is_empty(),
            "unsupported selection is refused",
        )?;

        state.games.clear();
        state.games.push(row(true));
        state.pending.insert(id.clone());
        let _task = update(&mut state, Message::Select(id, true));
        check(
            state.actionable_selection().is_empty(),
            "pending work is not queued twice",
        )
    }

    #[test]
    fn a_scan_removes_selection_for_games_that_disappeared() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.selected.insert("manual:fixture".into());
        state.expanded = Some("manual:fixture".into());
        let _task = update(
            &mut state,
            Message::Scanned(Ok(ScanResult {
                worker_epoch: 0,
                generation: 0,
                discovered: vec![],
                reports: vec![],
                games: vec![],
                drives: vec![],
                records: vec![],
                activity: vec![],
                warnings: vec![],
            })),
        );
        check(state.selected.is_empty(), "stale selection is removed")?;
        check(state.expanded.is_none(), "stale details are closed")
    }
}
