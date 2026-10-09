//! Window state. Blocking work returns snapshots through tasks and subscriptions.

use crate::{
    db::{Activity, Db, GameRecord},
    fsprobe,
    jobs::{
        self, Command, FolderKind, Job, Library, MotionPreference, Operation, PackTask, Phase,
        Snapshot, ThemePreference,
    },
    launchers::Env,
    model::{Game, GameId},
};
use iced::{Animation, Task};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Animation lengths in milliseconds as (expressive, subtle). Reduced motion
/// gives zero. Every transition in the window takes its length from here.
mod timing {
    pub const NAV: (u64, u64) = (220, 140);
    pub const PAGE: (u64, u64) = (180, 120);
    pub const DETAIL: (u64, u64) = (260, 150);
    pub const TOAST: (u64, u64) = (240, 150);
}
use super::shell::EASING;

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
            Self::Drives => "Locations",
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
    /// A Maximum store can be mounted over this game.
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
    /// it runs inside `scan`. A game that runs from a mounted store is probed
    /// through the folder that holds the mount, since the mount itself is FUSE.
    fn probe(
        game: Game,
        artwork: Option<super::artwork::Source>,
        cover: Option<super::artwork::Source>,
        packs: &[crate::pack::Install],
    ) -> Self {
        // `libraries` marks a game it could not rediscover with this detail
        // prefix. Its directory may be gone, so it is not probed.
        if matches!(&game.state, crate::model::InstallState::Broken { detail } if detail.starts_with("Location unavailable:"))
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
        let (target, stored) = probe_target(&game, packs);
        match fsprobe::probe(target) {
            Ok(fs) => {
                let tier = fsprobe::tier_for(&fs);
                let native_supported = match &tier {
                    fsprobe::Tier::Native(kind) => crate::backend::for_kind(*kind).is_some(),
                    fsprobe::Tier::Pack | fsprobe::Tier::Unsupported(_) => false,
                };
                // Mounting a store needs the `pack-mount` feature and FUSE.
                let pack_supported = stored
                    || (cfg!(feature = "pack-mount")
                        && std::path::Path::new("/dev/fuse").exists()
                        && matches!(tier, fsprobe::Tier::Native(_) | fsprobe::Tier::Pack));
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
/// The directory to probe for `game`, and whether the game runs from a
/// mounted Maximum store.
///
/// A store is mounted at the game's own path, so probing that path reports the
/// FUSE mount. The drive is the one holding the mount's parent folder, which
/// is what `storage::volume_existing` reads.
fn probe_target<'a>(game: &'a Game, packs: &[crate::pack::Install]) -> (&'a std::path::Path, bool) {
    if packs
        .iter()
        .any(|install| install.game_path == game.install_dir)
    {
        let parent = game.install_dir.parent().unwrap_or(&game.install_dir);
        return (parent, true);
    }
    (&game.install_dir, false)
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
    /// The worker epoch of the snapshot the scan started from. `update`
    /// discards the result when the worker has restarted or its game list has
    /// changed since.
    pub worker_epoch: u64,
    /// The worker's game list as scanned, tools included.
    pub discovered: Vec<Game>,
    pub reports: Vec<crate::compatibility::Report>,
    pub games: Vec<GameRow>,
    pub drives: Vec<Drive>,
    pub records: Vec<GameRecord>,
    pub activity: Vec<Activity>,
    pub warnings: Vec<String>,
}
pub use super::shell::Status;
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
    /// A Maximum store mounted at the game's path. Saves more, takes
    /// minutes, and keeps the original until the user confirms the game runs.
    Maximum,
}

/// What compressing a game gained, for its row and its History entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A native pass. Both sizes are sampled predictions, since the
    /// filesystem does not report what compression saved.
    Estimated { installed: u64, saved: u64 },
    /// Maximum with the original deleted. Both sizes are the store's.
    Measured { before: u64, after: u64 },
    /// Maximum with the original still kept, so nothing is saved yet.
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
/// not analyzed yet, compressed games, then games with little to save or on
/// a drive that cannot compress.
pub const WORTH_GROUPS: [&str; 4] = [
    "Worth compressing",
    "Not analyzed yet",
    "Compressed",
    "Little to save",
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
            Self::Updated => "Updated",
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
    pub toast: super::shell::Toast,
    /// The newest state received from the worker.
    pub snapshot: Snapshot,
    /// A command waiting for the user to accept its space plan.
    pub planned: Option<(Command, crate::storage::SpacePlan)>,
    pub qualification: Option<crate::qualification::Wizard>,
    /// Whether the wizard is still hashing a game's files.
    pub qualifying: bool,
    /// Set to stop that hash, which reads the whole install.
    pub qualify_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Id of the newest compatibility run. A result with another id is dropped.
    pub qualify_run: u64,
    /// Something was typed or ticked in the compatibility form since it opened.
    pub qualify_dirty: bool,
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
    worth_order: HashMap<GameId, (u8, u64)>,
    /// Estimates arrived while a row was open or selected, so the list was
    /// left as it was and the page offers to sort again.
    pub order_stale: bool,
    /// The "Little to save" group is expanded.
    pub show_low: bool,
    /// Modes the user picked, by game. A game without an entry uses
    /// `State::choice_for`'s default.
    pub choices: std::collections::HashMap<String, StorageChoice>,
    /// A snapshot has been applied at least once.
    snapshot_loaded_before: bool,
    /// A refresh was asked for while a scan ran, so one more follows it.
    rescan_wanted: bool,
    /// Games whose automatic analysis the worker refused since the last scan.
    /// They are skipped until the next scan so one refusal cannot repeat.
    analysis_refused: HashSet<String>,
    /// The last finished estimate per install folder and build. The worker
    /// keeps only its newest 300 finished jobs, so this outlives them.
    remembered: HashMap<(PathBuf, Option<String>), crate::estimate::Estimate>,
    /// Lookups over the job list, the rows and the records.
    index: Index,
}

/// Lookups built when a snapshot or a scan is applied, so rebuilding the page
/// does not search the job list once per row. A lookup checks that its map
/// still describes the data and searches directly when it does not.
#[derive(Default)]
struct Index {
    /// Length and newest id of the job list `by_dir` was built from.
    jobs: (usize, Option<i64>),
    /// Positions in `snapshot.jobs`, oldest first, by install folder.
    by_dir: HashMap<PathBuf, Vec<usize>>,
    rows_len: usize,
    rows: HashMap<GameId, usize>,
    records_len: usize,
    records: HashMap<GameId, usize>,
}
impl Index {
    fn job_key(jobs: &[Job]) -> (usize, Option<i64>) {
        (jobs.len(), jobs.last().map(|job| job.id))
    }
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
            toast: Default::default(),
            snapshot: Snapshot::default(),
            planned: None,
            qualification: None,
            qualifying: false,
            qualify_cancel: Default::default(),
            qualify_run: 0,
            qualify_dirty: false,
            nav: PAGES
                .into_iter()
                .chain(std::iter::once(Page::Settings))
                .map(|page| {
                    (
                        page,
                        Animation::new(page == Page::Overview)
                            .duration(Duration::from_millis(timing::NAV.0))
                            .easing(EASING),
                    )
                })
                .collect(),
            scroll_redraw_until: None,
            page_reveal: Animation::new(true)
                .duration(Duration::from_millis(timing::PAGE.0))
                .easing(EASING),
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
            detail: Animation::new(false)
                .duration(Duration::from_millis(timing::DETAIL.0))
                .easing(EASING),
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
            rescan_wanted: false,
            analysis_refused: Default::default(),
            remembered: Default::default(),
            index: Default::default(),
        }
    }
    /// Rebuilds the lookups over the snapshot, the rows and the records.
    fn reindex(&mut self) {
        let mut by_dir: HashMap<PathBuf, Vec<usize>> = HashMap::new();
        for (position, job) in self.snapshot.jobs.iter().enumerate() {
            by_dir
                .entry(job.game.install_dir.clone())
                .or_default()
                .push(position);
        }
        self.index = Index {
            jobs: Index::job_key(&self.snapshot.jobs),
            by_dir,
            rows_len: self.games.len(),
            rows: self
                .games
                .iter()
                .enumerate()
                .map(|(position, row)| (row.game.id.clone(), position))
                .collect(),
            records_len: self.records.len(),
            records: self
                .records
                .iter()
                .enumerate()
                .map(|(position, record)| (record.id.clone(), position))
                .collect(),
        };
    }
    /// The jobs for an install folder, newest first.
    fn jobs_for<'a>(&'a self, dir: &Path) -> Box<dyn Iterator<Item = &'a Job> + 'a> {
        let jobs = &self.snapshot.jobs;
        let positions: Cow<'a, [usize]> = if self.index.jobs == Index::job_key(jobs) {
            Cow::Borrowed(self.index.by_dir.get(dir).map(Vec::as_slice).unwrap_or(&[]))
        } else {
            Cow::Owned(
                jobs.iter()
                    .enumerate()
                    .filter(|(_, job)| job.game.install_dir == dir)
                    .map(|(position, _)| position)
                    .collect(),
            )
        };
        match positions {
            Cow::Borrowed(list) => Box::new(list.iter().rev().filter_map(move |i| jobs.get(*i))),
            Cow::Owned(list) => Box::new(list.into_iter().rev().filter_map(move |i| jobs.get(i))),
        }
    }
    /// The row for a game id.
    fn row_of(&self, id: &GameId) -> Option<&GameRow> {
        if self.index.rows_len == self.games.len()
            && let Some(row) = self
                .index
                .rows
                .get(id)
                .and_then(|position| self.games.get(*position))
                .filter(|row| row.game.id == *id)
        {
            return Some(row);
        }
        self.games.iter().find(|row| row.game.id == *id)
    }
    /// The database record of a compression at this game's installed build.
    /// A record at level 0 marks a decompressed game and does not count.
    fn record_of(&self, game: &Game) -> Option<&GameRecord> {
        let matches = |r: &&GameRecord| r.id == game.id && r.build == game.build && r.level > 0;
        if self.index.records_len == self.records.len() {
            return self
                .index
                .records
                .get(&game.id)
                .and_then(|position| self.records.get(*position))
                .filter(matches);
        }
        self.records.iter().find(matches)
    }
    /// Whether the worker was told not to touch this game.
    pub fn is_excluded(&self, game: &Game) -> bool {
        self.snapshot.excluded.iter().any(|id| {
            id.split_once(':').is_some_and(|(launcher, key)| {
                game.ids()
                    .any(|g| g.launcher.slug() == launcher && g.key == key)
            })
        })
    }
    /// Whether a bulk action may consider this row: its drive supports a
    /// mode and the game is not excluded.
    pub fn eligible(&self, row: &GameRow) -> bool {
        row.supported && !self.is_excluded(&row.game)
    }
    /// The title of the game with this id string, else the id itself.
    pub fn title_of(&self, id: &str) -> String {
        self.games
            .iter()
            .find(|row| row.game.ids().any(|g| g.to_string() == id))
            .map(|row| row.game.title.clone())
            .unwrap_or_else(|| id.to_owned())
    }
    /// Closes the detail pane. With no animation the row is released at once,
    /// otherwise `Tick` releases it when the animation ends.
    fn close_detail(&mut self) {
        self.detail.go_mut(false, Instant::now());
        if self.reduced_motion {
            self.expanded = None;
        }
    }
    /// Opens the detail pane for a game and leaves it open if it already is.
    fn open_detail(&mut self, id: String) {
        self.confirm_reclaim.clear();
        if self.expanded.as_ref() == Some(&id) {
            if !self.detail.value() {
                self.detail.go_mut(true, Instant::now());
            }
            return;
        }
        self.expanded = Some(id);
        self.detail = Animation::new(false)
            .duration(self.motion_duration(timing::DETAIL))
            .easing(EASING)
            .go(true, Instant::now());
    }
    /// Re-sorts the list, unless a pane is open or a row is ticked, in which
    /// case the page offers to sort again.
    fn refresh_order(&mut self) {
        if self.expanded.is_some() || !self.selected.is_empty() {
            self.order_stale = true;
        } else {
            self.capture_order();
        }
    }
    /// The Games list as drawn: the filtered rows, without the "Little to
    /// gain" group while it is collapsed.
    pub fn listed<'a>(&self, filtered: &[&'a GameRow]) -> Vec<&'a GameRow> {
        filtered
            .iter()
            .copied()
            .filter(|row| self.sort != Sort::Worth || self.show_low || self.worth(&row.game).0 != 3)
            .collect()
    }
    /// Makes the row for this id part of the page that is drawn: clears the
    /// filters that hide it and raises the page size and the collapsed group
    /// as far as it needs.
    fn reveal_game(&mut self, id: &str) {
        let visible = |state: &Self| {
            let filtered = state.filtered();
            state
                .listed(&filtered)
                .iter()
                .take(state.shown)
                .any(|row| row.game.id.to_string() == id)
        };
        if visible(self) || !self.games.iter().any(|row| row.game.id.to_string() == id) {
            return;
        }
        self.query.clear();
        self.filter = Filter::All;
        self.drive_filter = None;
        self.launcher_filter = None;
        self.capture_order();
        if self
            .games
            .iter()
            .any(|row| row.game.id.to_string() == id && self.worth(&row.game).0 == 3)
        {
            self.show_low = true;
        }
        let filtered = self.filtered();
        let position = self
            .listed(&filtered)
            .iter()
            .position(|row| row.game.id.to_string() == id);
        if let Some(position) = position {
            self.shown = self.shown.max(position / 40 * 40 + 40);
        }
    }
    /// Replaces the toast and restarts its reveal animation.
    pub fn show_status(&mut self, status: Status) {
        let fade = self.motion_duration(timing::TOAST);
        self.toast.show(status, fade);
    }

    /// An animation length for the current motion setting, from one of the
    /// `timing` pairs. Reduced motion gives zero.
    fn motion_duration(&self, (expressive, subtle): (u64, u64)) -> Duration {
        Duration::from_millis(match self.motion {
            MotionPreference::Expressive => expressive,
            MotionPreference::Subtle => subtle,
            MotionPreference::Reduced => 0,
        })
    }

    /// Shows the worker's refusal of something the user asked for. It leaves
    /// after a few seconds and is not a lost connection.
    fn show_refusal(&mut self, text: String) {
        let fade = self.motion_duration(timing::TOAST);
        self.toast.show_refusal(text, fade);
    }

    /// Starts hiding the toast.
    fn dismiss_status(&mut self) {
        self.toast.dismiss(self.reduced_motion);
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
    /// How big a game is: the launcher's figure, else what its analysis
    /// measured, else what its compression recorded. A custom folder has no
    /// launcher figure. The row, the total and the Size sort all use this.
    pub fn size_of(&self, game: &Game) -> Option<u64> {
        game.size_hint
            .or_else(|| self.estimate(game).map(|estimate| estimate.install_bytes))
            // An estimate stops counting once a job starts, but the size it
            // measured is still the size.
            .or_else(|| {
                self.jobs_for(&game.install_dir).find_map(|job| {
                    job.estimate
                        .as_ref()
                        .filter(|_| matches!(job.phase, Phase::Completed | Phase::Partial))
                        .map(|estimate| estimate.install_bytes)
                })
            })
            .or_else(|| match self.result(game) {
                Some(Outcome::Estimated { installed, .. }) => Some(installed),
                Some(Outcome::Measured { before, .. }) => Some(before),
                _ => None,
            })
            .filter(|bytes| *bytes > 0)
    }
    /// Sum of `size_of` over the games. A game with no known size counts as
    /// zero.
    pub fn total_bytes(&self) -> u64 {
        self.games
            .iter()
            .filter_map(|row| self.size_of(&row.game))
            .sum()
    }
    /// The newest job of any kind for this install directory.
    pub fn latest(&self, game: &Game) -> Option<&Job> {
        self.jobs_for(&game.install_dir).next()
    }
    /// What compressing this game gained, or `None` when it is not compressed.
    ///
    /// A Maximum install reports its store's own sizes. A native pass
    /// reports the saving in its record, else the estimate its
    /// job carried.
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
        // The record carries the saving across later passes, which analyse
        // only the files that changed. The newest job's own estimate covers
        // only its pass, so it is the fallback for a game with no record yet.
        self.record_of(game)
            .map(|record| Outcome::Estimated {
                installed: record.install_bytes,
                saved: u64::try_from(record.est_saving).unwrap_or(0),
            })
            .or_else(|| {
                self.jobs_for(&game.install_dir)
                    .filter(|job| job.game.build == game.build)
                    .find_map(job_outcome)
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
            .row_of(&game.id)
            .is_some_and(|row| !row.native_supported && row.pack_supported);
        if stored || only_mode {
            StorageChoice::Maximum
        } else {
            StorageChoice::Standard
        }
    }
    /// Closes the wizard and stops a hash that is still running.
    pub fn stop_qualifying(&mut self) {
        self.qualify_cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.qualifying = false;
        self.qualification = None;
        self.qualify_dirty = false;
    }
    /// What `choice` is predicted to save for this game. `None` until it has
    /// been analyzed, and zero when the saving is too small to bother with
    /// or the drive does not support that mode.
    pub fn prospect(&self, game: &Game, choice: StorageChoice) -> Option<u64> {
        let estimate = self.estimate(game)?;
        let row = self.row_of(&game.id)?;
        let saving = match choice {
            StorageChoice::Standard if row.native_supported => estimate.saving(),
            StorageChoice::Maximum if row.pack_supported => estimate.maximum_saving().unwrap_or(0),
            _ => 0,
        };
        let worthwhile = crate::recommendation::clears_threshold(
            saving,
            estimate.current_bytes(),
            crate::recommendation::Policy::default(),
        );
        Some(if worthwhile { saving } else { 0 })
    }
    /// A game's place in the Worth order as captured, with games seen since
    /// the capture counted as not analyzed.
    pub fn worth(&self, game: &Game) -> (u8, u64) {
        self.worth_order.get(&game.id).copied().unwrap_or((1, 0))
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
                (row.game.id.clone(), place)
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
    /// Jobs are read newest first. A finished analysis supplies it. A
    /// compression or decompression that completed or is running used the
    /// saving up, so the answer is `None`; one that was cancelled or failed
    /// changed nothing. With every job pruned, the remembered estimate applies.
    pub fn estimate(&self, game: &Game) -> Option<&crate::estimate::Estimate> {
        for job in self
            .jobs_for(&game.install_dir)
            .filter(|job| job.game.build == game.build)
        {
            if job.operation == Operation::Analyze {
                if matches!(job.phase, Phase::Completed | Phase::Partial)
                    && let Some(estimate) = &job.estimate
                {
                    return Some(estimate);
                }
            } else if matches!(job.phase, Phase::Completed | Phase::Partial)
                || (job.phase.active() && job.phase != Phase::Queued)
            {
                return None;
            }
        }
        if self.compressed(game) {
            return None;
        }
        self.remembered
            .get(&(game.install_dir.clone(), game.build.clone()))
    }
    /// What to do with this game, from its estimate and what its drive
    /// supports. `None` until an estimate exists. Maximum is offered
    /// only when the estimate says a qualification matched.
    pub fn recommendation(&self, game: &Game) -> Option<crate::recommendation::Recommendation> {
        let row = self.row_of(&game.id)?;
        self.estimate(game).map(|estimate| {
            crate::recommendation::choose(
                estimate,
                row.native_supported,
                row.pack_supported && estimate.maximum_qualified,
                crate::recommendation::Policy::default(),
            )
        })
    }
    /// Where this game's Maximum store goes: the path the user entered,
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
            .ok_or_else(|| "That game is no longer listed. Refresh and try again.".to_owned())?;
        match choice {
            StorageChoice::Standard => {
                if !row.native_supported {
                    return Err(format!(
                        "Standard is not available for {} on this drive. Open the game and choose Maximum.",
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
            .filter(|row| self.eligible(row) && !self.compressed(&row.game))
            // The library-wide action only ever uses Standard.
            .filter_map(|row| self.prospect(&row.game, StorageChoice::Standard))
            .sum()
    }

    /// Bytes saved so far. An estimate for natively compressed games.
    pub fn current_saving(&self) -> u64 {
        // One figure per installed game, from the same source its row shows.
        // A Maximum game whose original is still kept has saved nothing.
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
    /// Whether any job or remembered estimate exists for the game's installed
    /// build, so another analysis would repeat one.
    fn analysis_known(&self, game: &Game) -> bool {
        self.jobs_for(&game.install_dir)
            .any(|job| job.game.build == game.build)
            || self
                .remembered
                .contains_key(&(game.install_dir.clone(), game.build.clone()))
    }
    /// Keeps the estimate of each finished analysis in the snapshot, and
    /// drops the one a completed compression or decompression used up.
    fn remember_estimates(&mut self) {
        for job in &self.snapshot.jobs {
            let key = (job.game.install_dir.clone(), job.game.build.clone());
            if job.operation == Operation::Analyze {
                if matches!(job.phase, Phase::Completed | Phase::Partial)
                    && let Some(estimate) = &job.estimate
                {
                    self.remembered.insert(key, *estimate);
                }
            } else if matches!(job.phase, Phase::Completed | Phase::Partial) {
                self.remembered.remove(&key);
            }
        }
    }
    /// Whether the game counts as compressed at its installed build.
    ///
    /// True with a pack install. Otherwise the newest job that is not an
    /// analysis decides, skipping cancelled, failed and interrupted ones: it
    /// must be a completed compression of this build. With none, a record of
    /// this build above level 0 decides.
    pub fn compressed(&self, game: &Game) -> bool {
        if self
            .snapshot
            .packs
            .iter()
            .any(|install| install.game_path == game.install_dir)
        {
            return true;
        }
        let action = self.jobs_for(&game.install_dir).find(|j| {
            j.operation != Operation::Analyze
                && !matches!(
                    j.phase,
                    Phase::Queued | Phase::Cancelled | Phase::Failed | Phase::Interrupted
                )
        });
        match action {
            Some(j) => {
                j.operation == Operation::Compress
                    && j.phase == Phase::Completed
                    && j.game.build == game.build
            }
            None => self.record_of(game).is_some(),
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
                !self.is_excluded(&row.game)
                    && (query.is_empty() || row.game.title.to_lowercase().contains(&query))
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
        match self.sort {
            Sort::Name => games
                .sort_by_cached_key(|row| (row.game.title.to_lowercase(), row.game.title.clone())),
            Sort::Size => {
                let sizes: HashMap<&GameId, Option<u64>> = games
                    .iter()
                    .map(|row| (&row.game.id, self.size_of(&row.game)))
                    .collect();
                games.sort_by(|a, b| {
                    sizes
                        .get(&b.game.id)
                        .cmp(&sizes.get(&a.game.id))
                        .then(a.game.title.cmp(&b.game.title))
                });
            }
            Sort::Worth => games.sort_by(|a, b| {
                let (group_a, key_a) = self.worth(&a.game);
                let (group_b, key_b) = self.worth(&b.game);
                group_a
                    .cmp(&group_b)
                    .then(key_b.cmp(&key_a))
                    .then(a.game.title.cmp(&b.game.title))
            }),
        }
        games
    }
    /// The job for the bar under the page: the first one in progress, else
    /// the first one queued. Analyses are not listed on the Jobs page, so
    /// they are not here either.
    pub fn active(&self) -> Option<&Job> {
        let mut shown = self
            .snapshot
            .jobs
            .iter()
            .filter(|j| j.operation != Operation::Analyze);
        shown
            .clone()
            .find(|j| j.phase.active() && j.phase != Phase::Queued)
            .or_else(|| shown.find(|j| j.phase == Phase::Queued))
    }

    /// Selected games a bulk action may queue: supported, not excluded, idle
    /// and without an active pack job.
    pub fn actionable_selection(&self) -> Vec<Game> {
        self.games
            .iter()
            .filter(|row| {
                self.selected.contains(&row.game.id.to_string())
                    && self.eligible(row)
                    && row.game.state.is_idle()
                    && !self.pending.contains(&row.game.id.to_string())
            })
            .map(|row| row.game.clone())
            .collect()
    }
}

/// What a batch of commands came back with.
#[derive(Debug, Clone)]
pub struct Batch {
    /// The worker's state after the last command it accepted.
    pub snapshot: Snapshot,
    /// Each item the worker refused.
    pub refused: Vec<Refused>,
}

/// One item of a batch that the worker refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The game's `GameId::to_string()`.
    pub id: String,
    pub title: String,
    pub reason: String,
}

/// Why adding a folder failed.
#[derive(Debug, Clone)]
pub enum FolderError {
    /// The path is not a folder that exists.
    Missing,
    /// The worker refused it, with its reason.
    Refused(String),
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
    /// The run id it was started with, then the form or the reason it failed.
    QualificationReady(u64, Result<crate::qualification::Wizard, String>),
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
    AnalysisQueued(Result<Batch, String>),
    /// The reply to a batch of commands the user gave.
    Batched(Result<Batch, String>),
    /// The folder check and the worker's answer to "Add folder".
    FolderAdded(Result<Snapshot, FolderError>),
    /// Go to Games, filtered to the games that need attention.
    ReviewAttention,
    /// Clear the search, the status filter and the drive and launcher filters.
    ClearFilters,
    /// The measured position of a game's row, ready to scroll to.
    RowOffset(f32),
    /// Go back to the game's default artwork.
    ClearArtwork(String),
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
    /// Expand or collapse the "Little to save" group.
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
    /// Show the confirm step for deleting the kept original.
    PackReclaimPrompt(String),
    ToggleAdvanced(String),
}

use super::shell::background;

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

/// One job to queue: the game, the operation and its options.
type Item = (Game, Operation, crate::backend::CompressOpts);

/// Queues the items in one request, with no space plan review, and delivers
/// the worker's state with the items it refused. A refusal does not stop the
/// items after it.
fn send_many(items: Vec<Item>) -> Task<Message> {
    Task::perform(background(move || send_batch(items)), |result| {
        Message::Batched(result.and_then(|result| result))
    })
}

/// Sends the items as one `request_many`. Blocks. Fails only when the worker
/// cannot be reached.
fn send_batch(items: Vec<Item>) -> Result<Batch, String> {
    let sent: Vec<(String, String)> = items
        .iter()
        .map(|(game, _, _)| (game.id.to_string(), game.title.clone()))
        .collect();
    let (snapshot, refusals) = jobs::request_many(items).map_err(|e| e.to_string())?;
    Ok(Batch {
        snapshot,
        refused: pair_refusals(&sent, refusals),
    })
}

/// Attaches game ids to the worker's refusals, which name titles only. Two
/// games with one title take their refusals in the order sent.
fn pair_refusals(sent: &[(String, String)], refusals: Vec<jobs::Refusal>) -> Vec<Refused> {
    let mut used = vec![false; sent.len()];
    refusals
        .into_iter()
        .map(|refusal| {
            let slot = sent
                .iter()
                .zip(used.iter_mut())
                .find(|((_, title), used)| !**used && *title == refusal.title);
            let id = slot
                .map(|((id, _), used)| {
                    *used = true;
                    id.clone()
                })
                .unwrap_or_default();
            Refused {
                id,
                title: refusal.title,
                reason: refusal.reason,
            }
        })
        .collect()
}

/// One notice for the items of a batch that the worker refused: how many, then
/// at most three titles with their reasons. `noun` and `outcome` are each the
/// singular and plural form.
fn refusal_text(refused: &[Refused], noun: (&str, &str), outcome: (&str, &str)) -> Option<String> {
    if refused.is_empty() {
        return None;
    }
    let count = u64::try_from(refused.len()).unwrap_or(u64::MAX);
    let listed: Vec<String> = refused
        .iter()
        .take(3)
        .map(|item| format!("{}: {}.", item.title, item.reason.trim_end_matches('.')))
        .collect();
    let more = refused.len().saturating_sub(3);
    Some(format!(
        "{} {}: {}{}",
        crate::text::count(count, noun.0, noun.1),
        if count == 1 { outcome.0 } else { outcome.1 },
        listed.join(" "),
        if more > 0 {
            format!(" And {more} more.")
        } else {
            String::new()
        }
    ))
}

/// Queues one job for each game and shows the Jobs page. Several games at
/// once are always compressed with Standard: Maximum is chosen game by game.
/// A game that cannot take the job is left out and counted in a notice, so
/// one of them does not stop the rest.
fn queue_standard(state: &mut State, games: Vec<Game>, operation: Operation) -> Task<Message> {
    let wanted = games.len();
    let mut queued = vec![];
    let items: Vec<Item> = games
        .into_iter()
        .filter_map(|game| {
            let id = game.id.to_string();
            let item = if operation == Operation::Compress {
                match state.optimize_command(game, StorageChoice::Standard) {
                    Ok(Command::Enqueue {
                        game,
                        operation,
                        options,
                    }) => Some((game, operation, options)),
                    _ => None,
                }
            } else {
                let options = crate::backend::CompressOpts {
                    preset: state.preset_for(&id),
                    ..Default::default()
                };
                Some((game, operation, options))
            };
            if item.is_some() {
                queued.push(id);
            }
            item
        })
        .collect();
    // The ticks have done their job once the commands are on their way.
    for id in &queued {
        state.selected.remove(id);
    }
    let skipped = wanted - items.len();
    if skipped > 0 {
        state.show_status(Status::info(format!(
            "{} left out: their drive needs Maximum, which is chosen per game.",
            crate::text::count(
                u64::try_from(skipped).unwrap_or(u64::MAX),
                "game was",
                "games were"
            )
        )));
    }
    if items.is_empty() {
        return Task::none();
    }
    let navigation = update(state, Message::GoTo(Page::Queue));
    Task::batch([navigation, send_many(items)])
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
    let packs = snapshot.packs;
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
            GameRow::probe(g, source, cover, &packs)
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
        discovered: scanned_discovery,
        reports,
        games,
        drives,
        records,
        activity,
        warnings,
    })
}

/// The games to analyse next: every eligible, idle game with no job, no
/// remembered estimate and no refusal for its installed build, largest first.
///
/// The Games page does not decide this. At most 40 analyses are queued or
/// running at once, which keeps a batch under the worker's queue limit.
fn analysis_candidates(state: &State) -> Vec<Game> {
    let active = state
        .snapshot
        .jobs
        .iter()
        .filter(|job| job.operation == Operation::Analyze && job.phase.active())
        .count();
    let room = 40_usize.saturating_sub(active);
    if room == 0 {
        return vec![];
    }
    let mut rows: Vec<&GameRow> = state
        .games
        .iter()
        .filter(|row| {
            state.eligible(row)
                && row.game.state.is_idle()
                && !state.analysis_refused.contains(&row.game.id.to_string())
                && !state.analysis_known(&row.game)
        })
        .collect();
    rows.sort_by_cached_key(|row| std::cmp::Reverse(state.size_of(&row.game)));
    rows.into_iter()
        .take(room)
        .map(|row| row.game.clone())
        .collect()
}

/// Queues analyses for the games `analysis_candidates` picks. Existing results
/// and failures are kept until a user requests another analysis or the
/// launcher's build changes.
fn analyze_pending(state: &mut State) -> Task<Message> {
    if state.analysis_queuing || state.scanning {
        return Task::none();
    }
    let games = analysis_candidates(state);
    if games.is_empty() {
        return Task::none();
    }
    state.analysis_queuing = true;
    let items: Vec<Item> = games
        .into_iter()
        .map(|game| (game, Operation::Analyze, Default::default()))
        .collect();
    Task::perform(background(move || send_batch(items)), |result| {
        Message::AnalysisQueued(result.and_then(|result| result))
    })
}

/// Applies one message to the state and returns the work it starts.
///
/// Runs on the window thread, so anything that blocks goes through
/// `background` and comes back as another message. Arms that do not return
/// early fall through to `artwork_tasks`.
pub fn update(state: &mut State, message: Message) -> Task<Message> {
    let task = apply(state, message);
    Task::batch([task, state.toast.timer(Message::Tick)])
}

/// The body of `update`, before the toast timer is attached.
fn apply(state: &mut State, message: Message) -> Task<Message> {
    match message {
        // Compatibility qualification wizard.
        Message::Qualify(id) => {
            // The hash reads every file, so a second press while it runs
            // would start a second read of the install.
            if state.qualifying {
                return Task::none();
            }
            if let Some(game) = state
                .games
                .iter()
                .find(|row| row.game.id.to_string() == id && row.game.state.is_idle())
                .map(|row| row.game.clone())
            {
                state.qualifying = true;
                state.qualify_run += 1;
                let run = state.qualify_run;
                state.qualify_cancel = Default::default();
                let cancel = state.qualify_cancel.clone();
                return Task::perform(
                    background(move || {
                        crate::qualification::Wizard::start_cancellable(
                            game,
                            &cancel,
                            &crate::pack::NoObserver,
                        )
                        .map_err(|error| error.to_string())
                    }),
                    move |result| {
                        Message::QualificationReady(run, result.and_then(|result| result))
                    },
                );
            }
        }
        Message::QualificationReady(run, result) => {
            // A result that arrives after the user cancelled, or that belongs
            // to an earlier run, is dropped.
            if super::shell::run_is_current(state.qualifying, state.qualify_run, run) {
                state.qualifying = false;
                match result {
                    Ok(wizard) => {
                        state.qualification = Some(wizard);
                        state.qualify_dirty = false;
                    }
                    Err(error) => state.show_status(Status::error(format!(
                        "Could not start the compatibility test: {error}"
                    ))),
                }
            }
        }
        Message::QualificationField(field, text) => {
            if let Some(wizard) = &mut state.qualification {
                state.qualify_dirty = true;
                wizard.field(field, text);
            }
        }
        Message::QualificationCheck(check, value) => {
            if let Some(wizard) = &mut state.qualification {
                state.qualify_dirty = true;
                wizard.check(check, value);
            }
        }
        Message::QualificationMode(mode) => {
            if let Some(wizard) = &mut state.qualification {
                state.qualify_dirty = true;
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
                        wizard.measured(Err("Switch this game to Maximum first".into()));
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
                state.qualify_dirty = true;
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
                    Err(error) => state.show_status(Status::error(format!(
                        "Could not save the compatibility report: {error}"
                    ))),
                }
            }
        }
        Message::CloseQualification => state.stop_qualifying(),
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
                state.stop_qualifying();
                state.show_status(Status::info(format!(
                    "Compatibility report saved to {}",
                    path.display()
                )));
                return update(state, Message::Refresh);
            }
            Err(error) => state.show_status(Status::error(format!(
                "Could not save the compatibility report: {error}"
            ))),
        },
        // Space plan review.
        Message::Planned(result) => match result {
            // A plan with room to spare starts at once. The review appears
            // only when the plan fails its own check, to say what is short.
            Ok((command, plan)) if plan.check().is_ok() => {
                state.planned = None;
                return send_unchecked(Command::EnqueuePlanned {
                    command: Box::new(command),
                    plan,
                });
            }
            Ok(plan) => state.planned = Some(plan),
            Err(error) => state.show_status(Status::error(format!(
                "Could not check free space: {error}"
            ))),
        },
        Message::StartPlanned => {
            // The stored numbers are the ones that failed, so checking them
            // again could not change the answer. The command is planned
            // afresh from the drive's current free space. The panel stays up
            // until that plan passes and the job starts.
            if let Some((command, _)) = &state.planned {
                return send(command.clone());
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
            Err(error) => state.show_status(Status::error(format!(
                "Could not save diagnostics: {error}"
            ))),
        },
        // Artwork. Decodes are started by `artwork_tasks` after the match.
        Message::ArtworkVisible(source) => state.artwork_cache.request(source),
        Message::ArtworkLoaded(source, image) => state.artwork_cache.loaded(source, image),
        Message::ClearArtwork(game) => {
            return Task::perform(
                background(move || {
                    super::artwork::clear_override(&game).map_err(|error| error.to_string())
                }),
                |result| Message::ArtworkSaved(result.and_then(|result| result)),
            );
        }
        Message::ArtworkSaved(result) => match result {
            Ok(()) => return update(state, Message::Refresh),
            Err(error) => state.show_status(Status::error(format!(
                "Could not change the artwork: {error}"
            ))),
        },
        // Navigation and scrolling.
        Message::Jump(section) => {
            // `GoTo` can return artwork decodes it has already marked as
            // pending. Dropping that task left them pending for good, and
            // with two decodes allowed at once artwork loading could stall.
            let navigation = update(state, Message::GoTo(Page::Settings));
            return navigation.chain(super::surface::jump(
                "Settings",
                section,
                Message::JumpOffset,
            ));
        }
        Message::JumpOffset(offset) => {
            return super::surface::scroll_to_offset(
                &mut state.scroll_redraw_until,
                "Settings",
                offset,
                Message::Tick,
            );
        }
        Message::RowOffset(offset) => {
            return super::surface::scroll_to_offset(
                &mut state.scroll_redraw_until,
                Page::Games.label(),
                offset,
                Message::Tick,
            );
        }
        Message::ClearFilters => {
            state.query.clear();
            state.filter = Filter::All;
            state.drive_filter = None;
            state.launcher_filter = None;
            state.shown = 40;
            state.capture_order();
        }
        Message::ReviewAttention => {
            state.filter = Filter::Attention;
            state.query.clear();
            state.drive_filter = None;
            state.launcher_filter = None;
            state.shown = 40;
            state.capture_order();
            return update(state, Message::GoTo(Page::Games));
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
                    .duration(state.motion_duration(timing::PAGE))
                    .easing(EASING)
                    .go(true, Instant::now());
            }
            state.confirm_reclaim.clear();
            for (target, animation) in &mut state.nav {
                animation.go_mut(*target == page, Instant::now());
            }
            // A section destination scrolls to its section. Any other page
            // change returns to where that page was last scrolled.
            if let Some(section) = destination.section() {
                return super::surface::jump("Settings", section, Message::JumpOffset);
            }
            if changed {
                let offset = super::surface::recorded(&state.scroll_positions, page.label());
                return super::surface::restore_offset(page.label(), offset, Message::Tick);
            }
        }
        // Finding games and worker snapshots.
        Message::Rescan => return send(Command::RefreshDiscovery),
        Message::Refresh => {
            if state.scanning {
                // The running scan may have read the data before whatever
                // asked for this, so one more follows it.
                state.rescan_wanted = true;
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
                            || scan.discovered != state.snapshot.discovered)
                    {
                        return update(state, Message::Refresh);
                    }
                    state.games = scan.games;
                    state.reports = scan.reports;
                    state.drives = scan.drives;
                    state.records = scan.records;
                    state.reindex();
                    state.refresh_order();
                    state.analysis_refused.clear();
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
                    let dirs: HashSet<_> = state
                        .games
                        .iter()
                        .map(|row| row.game.install_dir.clone())
                        .collect();
                    state.remembered.retain(|(dir, _), _| dirs.contains(dir));
                    state.confirm_reclaim.retain(|id| valid.contains(id));
                    if state
                        .expanded
                        .as_ref()
                        .is_some_and(|id| !valid.contains(id))
                    {
                        state.expanded = None;
                    }
                }
                Err(e) => state.show_status(Status::error(format!(
                    "Could not refresh the game list: {e}"
                ))),
            }
            if std::mem::take(&mut state.rescan_wanted) {
                return update(state, Message::Refresh);
            }
            return analyze_pending(state);
        }
        Message::AnalysisQueued(result) => {
            state.analysis_queuing = false;
            match result {
                // A refused analysis is not a lost connection. Those games
                // are skipped until the next scan and the rest went through.
                Ok(batch) => {
                    state
                        .analysis_refused
                        .extend(batch.refused.iter().map(|item| item.id.clone()));
                    let notice = refusal_text(
                        &batch.refused,
                        ("game", "games"),
                        ("could not be analyzed", "could not be analyzed"),
                    );
                    let task = update(state, Message::Snapshot(Ok(batch.snapshot)));
                    if let Some(text) = notice {
                        state.show_status(Status::info(text));
                    }
                    return task;
                }
                Err(error) => return update(state, Message::Snapshot(Err(error))),
            }
        }
        Message::Batched(result) => match result {
            Ok(batch) => {
                let task = update(state, Message::Snapshot(Ok(batch.snapshot)));
                if let Some(text) = refusal_text(
                    &batch.refused,
                    ("game", "games"),
                    ("could not be queued", "could not be queued"),
                ) {
                    state.show_refusal(text);
                }
                return task;
            }
            Err(refusal) => state.show_refusal(refusal),
        },
        Message::FolderAdded(result) => match result {
            Ok(snapshot) => return update(state, Message::Snapshot(Ok(snapshot))),
            Err(FolderError::Missing) => {
                state.folder_error = Some("Choose an existing game folder.".into());
            }
            Err(FolderError::Refused(refusal)) => state.show_refusal(refusal),
        },

        // The reply to something the user asked for. A refusal is about that
        // request, so it is shown for a few seconds and the connection is
        // not marked as lost.
        Message::Commanded(result) => match result {
            Ok(snapshot) => return update(state, Message::Snapshot(Ok(snapshot))),
            Err(refusal) => state.show_refusal(refusal),
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
                            .easing(EASING)
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
                state.remember_estimates();
                state.reindex();
                // An excluded game cannot take part in a bulk action.
                let excluded = &state.snapshot.excluded;
                state.selected.retain(|id| !excluded.contains(id));
                // Estimates settle when the last analysis ends. The list is
                // sorted then, unless that would move a row the user has open
                // or ticked.
                if first || (analyzing && !state.analysis_active()) {
                    state.refresh_order();
                }
                state.polling = true;
                // A changed library list, a changed game list or finished work
                // all mean the scanned data is out of date. The first
                // snapshot differs from the empty one only because it is the
                // first.
                if libraries_changed {
                    if !first {
                        state.show_status(Status::info("Location settings saved."));
                    }
                    return update(state, Message::Refresh);
                }
                if completed_work || discovery_changed {
                    return update(state, Message::Refresh);
                }
                return analyze_pending(state);
            }
            Err(e) => {
                // Announce the loss once. Every failed poll after it would
                // bring the toast back after the user dismissed it.
                let first = state.connection_error.is_none();
                state.connection_error = Some(e.clone());
                state.polling = true;
                if first {
                    state.show_status(Status::error(format!(
                        "Lost connection to the background worker: {e}"
                    )));
                }
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
                    && state.eligible(row)
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
            state.reveal_game(&id);
            state.open_detail(id.clone());
            let scroll = super::surface::jump(
                Page::Games.label(),
                format!("game-{id}"),
                Message::RowOffset,
            );
            return Task::batch([navigation, scroll]);
        }
        Message::Expand(id) => {
            if state.expanded.as_ref() == Some(&id) && state.detail.value() {
                state.confirm_reclaim.clear();
                state.close_detail();
            } else {
                state.open_detail(id);
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
            state.refresh_order();
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
                    state.eligible(row)
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
                            state.show_status(Status::error("This path cannot be shown in the store field. Choose another folder."));
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
                state.show_status(Status::info("Report imported. Analysis will check the installed files before Maximum is chosen automatically."));
                if let Some(game) = game {
                    return send(Command::Enqueue {
                        game,
                        operation: Operation::Analyze,
                        options: Default::default(),
                    });
                }
            }
            Err(error) => state.show_status(Status::error(format!(
                "Could not import the report: {error}"
            ))),
        },
        // Locations and preferences.
        Message::Folder(folder) => {
            state.folder = folder;
            state.folder_error = None;
        }
        Message::FolderKind(kind) => state.folder_kind = kind,
        Message::AddFolder => {
            let folder = jobs::folder_path(&state.folder, &state.env.home);
            state.folder_error = None;
            let library = Library {
                path: folder.clone(),
                automatic: false,
                custom: true,
                folder_kind: state.folder_kind,
            };
            // A folder on a dead network mount can stall `is_dir`, so the
            // check runs away from the window thread.
            return Task::perform(
                background(move || {
                    if !folder.is_dir() || folder.parent().is_none() {
                        return Err(FolderError::Missing);
                    }
                    jobs::request(Command::Library(library))
                        .map_err(|error| FolderError::Refused(error.to_string()))
                }),
                |result| {
                    Message::FolderAdded(
                        result
                            .map_err(FolderError::Refused)
                            .and_then(|result| result),
                    )
                },
            );
        }
        Message::Preset(id, preset) => {
            state.presets.insert(id, preset);
        }
        Message::Motion(motion) => {
            state.motion = motion;
            state.reduced_motion = motion == MotionPreference::Reduced;
            // The sidebar animations are rebuilt at their current value with
            // the new timing. The setting is also sent to the worker.
            let duration = state.motion_duration(timing::NAV);
            for (_, animation) in &mut state.nav {
                *animation = Animation::new(animation.value())
                    .duration(duration)
                    .easing(EASING);
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
        // Maximum stores.
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
            state.toast.tick(state.reduced_motion);
            // The row of a closed pane stays in `expanded` while the pane
            // animates out, and is released when the animation ends.
            if state.expanded.is_some()
                && !state.detail.value()
                && !state.detail.is_animating(Instant::now())
            {
                state.expanded = None;
            }
        }
        Message::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            use super::shell::Shortcut;
            use iced::keyboard::{Key, key::Named};
            // Tab moves focus, Escape closes the storage plan, else the detail
            // pane and the selection, and the command key with F or R searches or rescans.
            if let Some(focus) = super::shell::tab_focus(&key, modifiers) {
                return focus;
            }
            match (&key, super::shell::shortcut(&key, modifiers)) {
                (_, Some(Shortcut::Search)) => {
                    let navigation = update(state, Message::GoTo(Page::Games));
                    return Task::batch([
                        navigation,
                        iced::widget::operation::focus(iced::widget::Id::new("game-search")),
                    ]);
                }
                (_, Some(Shortcut::Rescan)) => return update(state, Message::Rescan),
                (Key::Named(Named::Escape), None) => {
                    use super::shell::Escape;
                    match super::shell::escape_step(
                        state.planned.is_some(),
                        state.qualification.is_some(),
                        state.qualify_dirty,
                    ) {
                        Escape::ClosePlan => state.planned = None,
                        Escape::CloseForm => state.stop_qualifying(),
                        Escape::KeepForm => {
                            state.show_status(Status::info(super::shell::DISCARD_NOTICE));
                        }
                        Escape::Other => {
                            state.close_detail();
                            state.selected.clear();
                        }
                    }
                }
                _ => {}
            }
        }
        Message::Keyboard(_) => {}
    }
    artwork_tasks(state)
}

/// Starts a background decode for each source the cache hands out.
fn artwork_tasks(state: &mut State) -> Task<Message> {
    super::shell::artwork_tasks(&mut state.artwork_cache, Message::ArtworkLoaded)
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
                .toast
                .status
                .as_ref()
                .is_some_and(|status| status.text.contains("1 game was left out")),
            format!(
                "the skipped game is reported: {:?}",
                state.toast.status.as_ref().map(|s| &s.text)
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

    // The tests below cover the second audit pass.

    fn analysis_of(id: i64, key: &str, saving: u64) -> Job {
        job(
            id,
            &named(key, true).game,
            Operation::Analyze,
            Phase::Completed,
            saving,
        )
    }

    fn snapshot_of(revision: u64, jobs: Vec<Job>) -> Snapshot {
        Snapshot {
            worker_epoch: 1,
            revision,
            jobs,
            ..Snapshot::default()
        }
    }

    fn escape() -> iced::keyboard::Event {
        use iced::keyboard::{Key, Location, Modifiers, key};
        iced::keyboard::Event::KeyPressed {
            key: Key::Named(key::Named::Escape),
            modified_key: Key::Named(key::Named::Escape),
            physical_key: key::Physical::Code(key::Code::Escape),
            location: Location::Standard,
            modifiers: Modifiers::empty(),
            text: None,
            repeat: false,
        }
    }

    fn short_plan() -> (Command, crate::storage::SpacePlan) {
        (
            Command::Enqueue {
                game: game(),
                operation: Operation::Compress,
                options: Default::default(),
            },
            crate::storage::SpacePlan {
                retained_original: false,
                requirements: vec![crate::storage::Requirement {
                    volume: crate::storage::Volume {
                        identity: "fixture".into(),
                        path: "/fixture".into(),
                        available: 1_000,
                    },
                    additional: 4_000_000_000,
                    headroom: 200_000_000,
                    reasons: vec![],
                }],
            },
        )
    }

    fn scan_of(games: Vec<GameRow>) -> ScanResult {
        ScanResult {
            worker_epoch: 0,
            discovered: vec![],
            reports: vec![],
            games,
            drives: vec![],
            records: vec![],
            activity: vec![],
            warnings: vec![],
        }
    }

    #[test]
    fn a_closed_detail_pane_stops_holding_the_list_still() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.reduced_motion = true;
        state.games.push(named("alpha", true));
        state.games.push(named("beta", true));
        let beta = named("beta", true).game;
        let _task = update(
            &mut state,
            Message::Snapshot(Ok(snapshot_of(
                1,
                vec![job(1, &beta, Operation::Analyze, Phase::Analyzing, 0)],
            ))),
        );
        // The snapshot carries the motion setting, so this comes after it.
        state.reduced_motion = true;
        let _task = update(&mut state, Message::Expand("manual:alpha".into()));
        let _task = update(&mut state, Message::Expand("manual:alpha".into()));
        check(
            state.expanded.is_none(),
            "closing the pane releases the row",
        )?;
        // With motion, the row is released when the closing animation ends.
        state.reduced_motion = false;
        state.expanded = Some("manual:alpha".into());
        state.detail = Animation::new(false);
        let _task = update(&mut state, Message::Tick);
        check(
            state.expanded.is_none(),
            "a finished close releases the row",
        )?;
        let done = job(
            1,
            &beta,
            Operation::Analyze,
            Phase::Completed,
            2_000_000_000,
        );
        let _task = update(
            &mut state,
            Message::Snapshot(Ok(snapshot_of(2, vec![done]))),
        );
        check(!state.order_stale, "nothing is held, so no banner")?;
        check_eq(
            order(&state),
            ["beta", "alpha"].map(String::from).to_vec(),
            "the list sorted itself when the analysis finished",
        )
    }

    #[test]
    fn a_failed_poll_shows_the_disconnect_toast_once() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.reduced_motion = true;
        let _task = update(&mut state, Message::Snapshot(Err("offline".into())));
        check(
            state.toast.status.is_some(),
            "the first failure is announced",
        )?;
        let _task = update(&mut state, Message::Dismiss);
        check(
            state.toast.status.is_none(),
            "control: dismissal hides the toast",
        )?;
        let _task = update(&mut state, Message::Snapshot(Err("offline".into())));
        check(
            state.toast.status.is_none(),
            "a later failed poll does not undo the dismissal",
        )
    }

    #[test]
    fn a_failing_plan_can_be_checked_again() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let _task = update(&mut state, Message::Planned(Ok(short_plan())));
        check(state.planned.is_some(), "a plan that fails is shown")?;
        let _task = update(&mut state, Message::StartPlanned);
        check(
            state.planned.is_some(),
            "the plan stays on screen while it is checked again",
        )?;
        check(
            state.toast.status.is_none(),
            "checking again does not report the old numbers as a new error",
        )
    }

    #[test]
    fn the_work_bar_ignores_analyses() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let mut running = analysis_of(1, "alpha", 0);
        running.phase = Phase::Analyzing;
        state.snapshot.jobs.push(running);
        check(state.active().is_none(), "the Jobs page hides analyses")?;
        let mut compress = analysis_of(2, "beta", 0);
        compress.operation = Operation::Compress;
        compress.phase = Phase::Running;
        state.snapshot.jobs.push(compress);
        check_eq(
            state.active().map(|job| job.id),
            Some(2),
            "control: a compression is shown",
        )
    }

    #[test]
    fn compress_selected_clears_the_ticks() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(row(true));
        let _task = update(&mut state, Message::Select("manual:fixture".into(), true));
        check_eq(state.selected.len(), 1, "control: the row is ticked")?;
        let _task = update(&mut state, Message::Queue(Operation::Compress));
        check(state.selected.is_empty(), "queued games are unticked")
    }

    #[test]
    fn one_size_serves_the_total_and_the_sort() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let mut known = named("known", true);
        known.game.size_hint = Some(1_000_000_000);
        let mut custom = named("custom", true);
        custom.game.size_hint = None;
        state.games.push(known);
        state.games.push(custom);
        state
            .snapshot
            .jobs
            .push(analysis_of(1, "custom", 100_000_000));
        check_eq(
            state.total_bytes(),
            5_000_000_000,
            "a custom folder counts its analysed size",
        )?;
        let _task = update(&mut state, Message::Sort(Sort::Size));
        check_eq(
            order(&state),
            ["custom", "known"].map(String::from).to_vec(),
            "and sorts by it",
        )
    }

    #[test]
    fn reviewing_the_open_game_keeps_it_open_and_visible() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(named("alpha", true));
        let _task = update(&mut state, Message::Expand("manual:alpha".into()));
        let _task = update(&mut state, Message::ReviewGame("manual:alpha".into()));
        check(state.detail.value(), "review does not toggle the pane shut")?;
        state.query = "zzz".into();
        let _task = update(&mut state, Message::ReviewGame("manual:alpha".into()));
        check(
            state.query.is_empty(),
            "a search that hides the game is cleared",
        )
    }

    #[test]
    fn a_cancelled_compression_keeps_the_estimate_and_the_state() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(named("alpha", true));
        let alpha = named("alpha", true).game;
        state
            .snapshot
            .jobs
            .push(analysis_of(1, "alpha", 900_000_000));
        state
            .snapshot
            .jobs
            .push(job(2, &alpha, Operation::Compress, Phase::Cancelled, 0));
        check(
            state.estimate(&alpha).is_some(),
            "the analysis still applies after a cancel",
        )?;
        check(!state.compressed(&alpha), "nothing was compressed")?;
        check(state.potential_saving() > 0, "it stays in the total")?;
        state.snapshot.jobs = vec![
            job(
                3,
                &alpha,
                Operation::Compress,
                Phase::Completed,
                500_000_000,
            ),
            job(4, &alpha, Operation::Compress, Phase::Failed, 0),
        ];
        check(
            state.compressed(&alpha),
            "a failed retry does not undo a finished pass",
        )
    }

    #[test]
    fn an_analysis_in_progress_is_not_an_estimate() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let mut running = analysis_of(1, "alpha", 900_000_000);
        running.phase = Phase::Analyzing;
        state.snapshot.jobs.push(running);
        check(
            state.estimate(&named("alpha", true).game).is_none(),
            "partial sums are not shown as a result",
        )
    }

    #[test]
    fn the_record_decides_the_saving_and_level_zero_is_not_compressed() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let alpha = named("alpha", true).game;
        state.games.push(named("alpha", true));
        state.snapshot.jobs.push(job(
            1,
            &alpha,
            Operation::Compress,
            Phase::Completed,
            200_000_000,
        ));
        let mut record = GameRecord::new(
            alpha.id.clone(),
            "alpha",
            alpha.install_dir.clone(),
            fsprobe::BackendKind::Btrfs,
            &Default::default(),
        );
        record.build = alpha.build.clone();
        record.level = 3;
        record.install_bytes = 4_000_000_000;
        record.est_saving = 5_000_000_000;
        state.records.push(record.clone());
        check_eq(
            state.result(&alpha),
            Some(Outcome::Estimated {
                installed: 4_000_000_000,
                saved: 5_000_000_000,
            }),
            "the carried figure beats the last pass's own estimate",
        )?;
        state.snapshot.jobs.clear();
        record.level = 0;
        state.records = vec![record];
        check(
            !state.compressed(&alpha),
            "a decompressed record is not a compressed game",
        )
    }

    #[test]
    fn estimates_outlive_the_workers_job_history() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(named("alpha", true));
        let alpha = named("alpha", true).game;
        let _task = update(
            &mut state,
            Message::Snapshot(Ok(snapshot_of(
                1,
                vec![analysis_of(1, "alpha", 900_000_000)],
            ))),
        );
        check(state.estimate(&alpha).is_some(), "control: estimate known")?;
        let _task = update(&mut state, Message::Snapshot(Ok(snapshot_of(2, vec![]))));
        check(
            state.estimate(&alpha).is_some(),
            "pruning the finished job does not forget the estimate",
        )
    }

    #[test]
    fn a_scan_applied_with_a_row_open_leaves_the_order_alone() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(named("alpha", true));
        state.games.push(named("beta", true));
        state.capture_order();
        state.expanded = Some("manual:alpha".into());
        state
            .snapshot
            .jobs
            .push(analysis_of(1, "beta", 2_000_000_000));
        let _task = update(
            &mut state,
            Message::Scanned(Ok(scan_of(vec![named("alpha", true), named("beta", true)]))),
        );
        check_eq(
            order(&state),
            ["alpha", "beta"].map(String::from).to_vec(),
            "rows hold still under an open pane",
        )?;
        check(state.order_stale, "and the page offers to sort again")
    }

    #[test]
    fn a_scan_is_kept_when_only_the_generation_moved() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.snapshot_loaded = true;
        state.snapshot.worker_epoch = 5;
        state.snapshot.scan_generation = 2;
        state.snapshot.discovered = vec![game()];
        let mut scan = scan_of(vec![row(true)]);
        scan.worker_epoch = 5;
        scan.discovered = vec![game()];
        let _task = update(&mut state, Message::Scanned(Ok(scan)));
        check_eq(state.games.len(), 1, "an identical game list is applied")
    }

    #[test]
    fn a_refresh_asked_for_during_a_scan_runs_afterwards() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.scanning = true;
        let _task = update(&mut state, Message::Refresh);
        let _task = update(&mut state, Message::Scanned(Ok(scan_of(vec![]))));
        check(state.scanning, "the dropped refresh was started again")
    }

    #[test]
    fn library_settings_are_not_announced_on_the_first_snapshot() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let mut first = snapshot_of(1, vec![]);
        first.libraries.push(jobs::Library {
            path: "/games".into(),
            automatic: true,
            custom: true,
            folder_kind: jobs::FolderKind::Collection,
        });
        let _task = update(&mut state, Message::Snapshot(Ok(first)));
        check(
            state.toast.status.is_none(),
            "launch is not a settings change",
        )
    }

    #[test]
    fn analysis_continues_past_a_first_page_of_analysed_games() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let mut jobs = vec![];
        for number in 0..60_i64 {
            let key = format!("g{number:02}");
            state.games.push(named(&key, true));
            if number < 40 {
                jobs.push(analysis_of(number + 1, &key, 500_000_000));
            }
        }
        let _task = update(&mut state, Message::Snapshot(Ok(snapshot_of(1, jobs))));
        check(
            state.analysis_queuing(),
            "games beyond the visible page are still analysed",
        )
    }

    #[test]
    fn excluded_games_are_not_offered_by_the_library_action() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(named("a", true));
        state.games.push(named("b", true));
        state.snapshot.jobs.push(analysis_of(1, "a", 500_000_000));
        state.snapshot.jobs.push(analysis_of(2, "b", 700_000_000));
        state.snapshot.excluded = vec!["manual:b".into()];
        check_eq(
            state.potential_saving(),
            500_000_000,
            "an excluded game is not in the total",
        )?;
        state.selected.insert("manual:b".into());
        check(
            state.actionable_selection().is_empty(),
            "nor in a ticked selection",
        )
    }

    #[test]
    fn escape_closes_the_storage_plan() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.planned = Some(short_plan());
        let _task = update(&mut state, Message::Keyboard(escape()));
        check(state.planned.is_none(), "Escape dismisses the plan")
    }

    fn open_form(state: &mut State) {
        state.qualification = Some(crate::qualification::Wizard::new(
            named("a", true).game,
            crate::compatibility::Corpus {
                sha256: "a".repeat(64),
                files: 1,
                bytes: 1,
            },
        ));
    }

    #[test]
    fn escape_closes_an_untouched_compatibility_form_and_keeps_a_filled_one() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        open_form(&mut state);
        let _task = update(&mut state, Message::Keyboard(escape()));
        check(state.qualification.is_none(), "an untouched form closes")?;
        open_form(&mut state);
        let _task = update(
            &mut state,
            Message::QualificationField(crate::qualification::Field::Build, "1.2".into()),
        );
        let _task = update(&mut state, Message::Keyboard(escape()));
        check(state.qualification.is_some(), "a form with an entry stays")?;
        check_eq(
            state.toast.status.as_ref().map(|status| status.text.clone()),
            Some("Press Close to discard the compatibility test.".to_owned()),
            "and says how to leave",
        )?;
        let _task = update(&mut state, Message::CloseQualification);
        open_form(&mut state);
        let _task = update(&mut state, Message::Keyboard(escape()));
        check(
            state.qualification.is_none(),
            "closing cleared the flag for the next form",
        )
    }

    #[test]
    fn a_cancelled_compatibility_run_cannot_clear_the_next_ones_busy_flag() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.qualifying = true;
        state.qualify_run = 2;
        let _task = update(
            &mut state,
            Message::QualificationReady(1, Err("old run".into())),
        );
        check(state.qualifying, "the older run's result is ignored")?;
        check(state.toast.status.is_none(), "and says nothing")?;
        let _task = update(
            &mut state,
            Message::QualificationReady(2, Err("this run".into())),
        );
        check(!state.qualifying, "control: the current run's result ends it")
    }

    #[test]
    fn adding_a_folder_does_not_touch_the_disk_on_the_window_thread() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.folder = "/nonexistent/flummox-test-folder".into();
        let _task = update(&mut state, Message::AddFolder);
        check(
            state.folder_error.is_none(),
            "the folder is checked in the background",
        )
    }

    fn mounted(game: &Game) -> crate::pack::Install {
        crate::pack::Install {
            game_path: game.install_dir.clone(),
            store_path: "/fixture/store".into(),
            writes_path: "/fixture/updates".into(),
            backup_path: None,
            previous_store_path: None,
            previous_writes_path: None,
            summary: None,
            phase: crate::pack::InstallPhase::Mounted,
            message: String::new(),
        }
    }

    #[test]
    fn a_game_running_from_a_store_is_probed_through_the_mounts_parent() -> TestResult {
        let temp = tempfile::tempdir().ctx("probe fixture")?;
        let mut stored = game();
        stored.install_dir = temp.path().join("Stored Game");
        std::fs::create_dir(&stored.install_dir).ctx("game folder")?;
        let packs = [mounted(&stored)];
        let (target, is_stored) = probe_target(&stored, &packs);
        check_eq(target, temp.path(), "the folder that holds the mount")?;
        check(is_stored, "it is a stored game")?;
        let (target, is_stored) = probe_target(&stored, &[]);
        check_eq(target, stored.install_dir.as_path(), "control: no store")?;
        check(!is_stored, "control: not stored")?;
        let row = GameRow::probe(stored, None, None, &packs);
        check(row.pack_supported, "a stored game is a Maximum game")?;
        check(row.supported, "so it is not reported as unsupported")?;
        check(row.note.is_none(), "and carries no unsupported note")?;
        check(
            row.mountpoint.is_some(),
            "its drive is the one under the mount",
        )
    }

    #[test]
    fn the_list_leaves_out_the_collapsed_group_and_counts_what_is_hidden() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(named("a", true));
        state.games.push(named("b", true));
        state.games.push(named("c", false));
        state.games.push(named("d", false));
        state.capture_order();
        state.shown = 2;
        let collapsed: Vec<String> = {
            let filtered = state.filtered();
            let listed = state.listed(&filtered);
            listed.iter().map(|row| row.game.title.clone()).collect()
        };
        check_eq(
            collapsed,
            ["a", "b"].map(String::from).to_vec(),
            "the group of games with little to save is collapsed",
        )?;
        state.show_low = true;
        let filtered = state.filtered();
        check_eq(
            state.listed(&filtered).len(),
            4,
            "control: expanding it lists every game",
        )
    }

    #[test]
    fn a_game_is_named_by_its_title_and_unknown_ids_stay_as_they_are() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(named("alpha", true));
        check_eq(
            state.title_of("manual:alpha"),
            "alpha".to_owned(),
            "a known id shows its title",
        )?;
        check_eq(
            state.title_of("manual:gone"),
            "manual:gone".to_owned(),
            "an unknown id is not hidden",
        )
    }

    #[test]
    fn refused_commands_are_reported_and_the_connection_stays_up() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let batch = Batch {
            snapshot: snapshot_of(1, vec![]),
            refused: vec![
                refused("manual:a", "Alpha", "This game is excluded."),
                refused("manual:b", "Beta", "The queue is full."),
            ],
        };
        let _task = update(&mut state, Message::Batched(Ok(batch)));
        check(state.connection_error.is_none(), "not a lost worker")?;
        check(
            state.snapshot_loaded,
            "the commands that went through were applied",
        )?;
        let text = state
            .toast
            .status
            .as_ref()
            .map(|status| status.text.clone());
        check(
            text.as_deref()
                .is_some_and(|text| text.starts_with("2 games could not be queued: Alpha: This game is excluded.")),
            format!("the refusals are counted: {text:?}"),
        )?;
        check(
            state
                .toast
                .status
                .as_ref()
                .is_some_and(|status| status.is_error),
            "and shown as an error",
        )
    }

    #[test]
    fn a_refused_analysis_is_skipped_and_is_not_a_lost_connection() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(named("a", true));
        state.analysis_queuing = true;
        let batch = Batch {
            snapshot: snapshot_of(1, vec![]),
            refused: vec![refused("manual:a", "A", "The queue is full.")],
        };
        let _task = update(&mut state, Message::AnalysisQueued(Ok(batch)));
        check(!state.analysis_queuing(), "the batch is over")?;
        check(state.connection_error.is_none(), "not a lost worker")?;
        check(
            analysis_candidates(&state).is_empty(),
            "the refused game is not tried again at once",
        )?;
        let _task = update(&mut state, Message::AnalysisQueued(Err("offline".into())));
        check(
            state.connection_error.is_some(),
            "control: no reply at all is a lost connection",
        )
    }

    #[test]
    fn adding_a_folder_reports_a_missing_folder_and_a_refusal_differently() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let _task = update(&mut state, Message::FolderAdded(Err(FolderError::Missing)));
        check(
            state.folder_error.is_some(),
            "a missing folder is a form error",
        )?;
        check(state.toast.status.is_none(), "and not a toast")?;
        state.folder_error = None;
        let _task = update(
            &mut state,
            Message::FolderAdded(Err(FolderError::Refused("Not allowed.".into()))),
        );
        check(
            state.folder_error.is_none(),
            "a refusal is not a form error",
        )?;
        check(state.toast.status.is_some(), "it is a toast")?;
        check(state.connection_error.is_none(), "and not a lost worker")
    }

    #[test]
    fn review_attention_opens_the_games_that_need_it() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.query = "zzz".into();
        state.drive_filter = Some("/nowhere".into());
        let _task = update(&mut state, Message::ReviewAttention);
        check_eq(state.page, Page::Games, "the Games page")?;
        check_eq(state.filter, Filter::Attention, "filtered to attention")?;
        check(
            state.query.is_empty() && state.drive_filter.is_none(),
            "with nothing else hiding games",
        )
    }

    #[test]
    fn a_toast_gets_one_timer_for_its_deadline() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        check(state.toast.scheduled.is_none(), "control: none at first")?;
        let _task = update(
            &mut state,
            Message::DiagnosticsExported(Ok("/fixture/diagnostics.json".into())),
        );
        check(state.toast.deadline.is_some(), "the toast has a deadline")?;
        check_eq(
            state.toast.scheduled,
            state.toast.deadline,
            "a timer was started for it",
        )
    }

    #[test]
    fn every_transition_takes_its_length_from_one_table() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        check_eq(
            state.motion_duration(timing::NAV),
            Duration::from_millis(timing::NAV.0),
            "expressive",
        )?;
        state.motion = MotionPreference::Subtle;
        check_eq(
            state.motion_duration(timing::DETAIL),
            Duration::from_millis(timing::DETAIL.1),
            "subtle",
        )?;
        state.motion = MotionPreference::Reduced;
        check_eq(
            state.motion_duration(timing::PAGE),
            Duration::ZERO,
            "reduced motion resolves at once",
        )
    }

    #[test]
    fn clearing_the_filters_shows_every_game_again() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.games.push(named("alpha", true));
        state.query = "zzz".into();
        state.filter = Filter::Compressed;
        state.launcher_filter = Some("Steam".into());
        check(state.filtered().is_empty(), "control: the filters hide it")?;
        let _task = update(&mut state, Message::ClearFilters);
        check_eq(state.filtered().len(), 1, "cleared")
    }

    fn refused(id: &str, title: &str, reason: &str) -> Refused {
        Refused {
            id: id.into(),
            title: title.into(),
            reason: reason.into(),
        }
    }

    #[test]
    fn restart_is_blocked_only_by_active_jobs() -> TestResult {
        let game = named("a", true).game;
        let idle = snapshot_of(1, vec![]);
        check(
            crate::gui::view::restart_blocked(&idle).is_none(),
            "an idle worker can restart",
        )?;
        let mut busy = snapshot_of(
            1,
            vec![job(1, &game, Operation::Compress, Phase::Running, 0)],
        );
        check(
            crate::gui::view::restart_blocked(&busy).is_some(),
            "a running job blocks it",
        )?;
        busy.jobs.clear();
        check(
            crate::gui::view::restart_blocked(&busy).is_none(),
            "control: with the job gone it can restart",
        )
    }

    #[test]
    fn refusals_are_summarised_by_count_and_at_most_three_titles() -> TestResult {
        let games = ("game", "games");
        let outcome = ("could not be queued", "could not be queued");
        check(
            refusal_text(&[], games, outcome).is_none(),
            "nothing refused",
        )?;
        check_eq(
            refusal_text(&[refused("a", "Alpha", "Why.")], games, outcome),
            Some("1 game could not be queued: Alpha: Why.".to_owned()),
            "one",
        )?;
        let five: Vec<Refused> = ["A", "B", "C", "D", "E"]
            .iter()
            .map(|title| refused(title, title, "No"))
            .collect();
        check_eq(
            refusal_text(&five, games, outcome),
            Some("5 games could not be queued: A: No. B: No. C: No. And 2 more.".to_owned()),
            "five, three listed",
        )
    }

    #[test]
    fn a_mixed_batch_pairs_each_refusal_with_its_game() -> TestResult {
        let sent: Vec<(String, String)> = vec![
            ("manual:a".into(), "Same".into()),
            ("manual:b".into(), "Other".into()),
            ("manual:c".into(), "Same".into()),
        ];
        let refusals = vec![
            jobs::Refusal {
                title: "Same".into(),
                reason: "Full.".into(),
            },
            jobs::Refusal {
                title: "Other".into(),
                reason: "Excluded.".into(),
            },
            jobs::Refusal {
                title: "Same".into(),
                reason: "Full.".into(),
            },
            jobs::Refusal {
                title: "Unknown".into(),
                reason: "Odd.".into(),
            },
        ];
        let ids: Vec<String> = pair_refusals(&sent, refusals)
            .into_iter()
            .map(|item| item.id)
            .collect();
        check_eq(
            ids,
            vec![
                "manual:a".to_owned(),
                "manual:b".to_owned(),
                "manual:c".to_owned(),
                String::new(),
            ],
            "titles shared by two games take them in order, an unknown title has no id",
        )
    }

    #[test]
    fn a_prospect_is_judged_against_the_whole_install() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let row = named("video-heavy", true);
        let game = row.game.clone();
        state.games.push(row);
        let mut estimate_job = job(
            1,
            &game,
            Operation::Analyze,
            Phase::Completed,
            1_000_000_000,
        );
        if let Some(estimate) = &mut estimate_job.estimate {
            // Most of the install is video that will not shrink, so the
            // saving is a quarter of what can shrink and 1% of the install.
            estimate.install_bytes = 100_000_000_000;
            estimate.disk_now = 4_000_000_000;
            estimate.disk_after = 3_000_000_000;
        }
        state.snapshot.jobs.push(estimate_job);
        check_eq(
            state.prospect(&game, StorageChoice::Standard),
            Some(0),
            "a 1% saving of the install is too little to promise",
        )?;
        if let Some(estimate) = state
            .snapshot
            .jobs
            .first_mut()
            .and_then(|job| job.estimate.as_mut())
        {
            estimate.install_bytes = 4_000_000_000;
        }
        check_eq(
            state.prospect(&game, StorageChoice::Standard),
            Some(1_000_000_000),
            "control: the same saving of a small install clears the threshold",
        )
    }

    #[test]
    fn qualifying_runs_once_and_closing_stops_the_hash() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        let row = named("hashed", true);
        let id = row.game.id.to_string();
        state.games.push(row);
        let _first = update(&mut state, Message::Qualify(id.clone()));
        check(state.qualifying, "the first press starts the hash")?;
        let flag = state.qualify_cancel.clone();
        check(
            !flag.load(std::sync::atomic::Ordering::Relaxed),
            "control: the hash is not cancelled while it runs",
        )?;
        let _second = update(&mut state, Message::Qualify(id));
        check(
            std::sync::Arc::ptr_eq(&flag, &state.qualify_cancel),
            "a second press does not start another hash",
        )?;
        let _closed = update(&mut state, Message::CloseQualification);
        check(
            flag.load(std::sync::atomic::Ordering::Relaxed),
            "closing sets the cancel flag the hash reads",
        )?;
        check(!state.qualifying, "the button is available again")?;
        let _late = update(
            &mut state,
            Message::QualificationReady(0, Err("Compatibility verification stopped".into())),
        );
        check(
            state.toast.status.is_none(),
            "a result that arrives after cancelling shows nothing",
        )
    }
}
