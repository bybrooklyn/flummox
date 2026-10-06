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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Overview,
    Games,
    Queue,
    Drives,
    Recovery,
    Settings,
}
pub const PAGES: [Page; 2] = [Page::Overview, Page::Games];
impl Page {
    pub fn main(self) -> Self {
        match self {
            Self::Overview | Self::Games => self,
            _ => Self::Settings,
        }
    }
    pub fn rank(self) -> u8 {
        match self.main() {
            Self::Overview => 0,
            Self::Games => 1,
            _ => 2,
        }
    }
    pub fn section(self) -> Option<&'static str> {
        match self {
            Self::Queue => Some("settings-jobs"),
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
            Self::Queue => "Queue",
            Self::Drives => "Drives",
            Self::Recovery => "Recovery",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GameRow {
    pub game: Game,
    pub filesystem: String,
    pub mountpoint: Option<PathBuf>,
    pub supported: bool,
    pub native_supported: bool,
    pub pack_supported: bool,
    pub note: Option<String>,
    pub artwork: Option<super::artwork::Source>,
    pub cover: Option<super::artwork::Source>,
}
impl GameRow {
    fn probe(
        game: Game,
        artwork: Option<super::artwork::Source>,
        cover: Option<super::artwork::Source>,
    ) -> Self {
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
#[derive(Debug, Clone)]
pub struct Drive {
    pub path: PathBuf,
    pub free: Option<u64>,
    pub games: usize,
}
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub worker_epoch: u64,
    pub generation: u64,
    pub discovered: Vec<Game>,
    pub reports: Vec<crate::compatibility::Report>,
    pub games: Vec<GameRow>,
    pub drives: Vec<Drive>,
    pub records: Vec<GameRecord>,
    pub activity: Vec<Activity>,
    pub warnings: Vec<String>,
}
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Name,
    Size,
    Saving,
}
impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Size => "Size",
            Self::Saving => "Potential saving",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Ready,
    Compressed,
    Attention,
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

pub struct State {
    pub env: Env,
    pub reports: Vec<crate::compatibility::Report>,
    pub page: Page,
    pub games: Vec<GameRow>,
    pub artwork_cache: super::artwork::Cache,
    pub drives: Vec<Drive>,
    pub records: Vec<GameRecord>,
    pub activity: Vec<Activity>,
    pub warnings: Vec<String>,
    pub status: Option<Status>,
    pub status_reveal: Animation<bool>,
    pub status_deadline: Option<Instant>,
    pub snapshot: Snapshot,
    pub planned: Option<(Command, crate::storage::SpacePlan)>,
    pub qualification: Option<crate::qualification::Wizard>,
    pub nav: Vec<(Page, Animation<bool>)>,
    pub page_reveal: Animation<bool>,
    pub scroll_redraw_until: Option<Instant>,
    pub page_direction: f32,
    pub scroll_positions: std::collections::HashMap<String, f32>,
    pub snapshot_loaded: bool,
    pub connection_error: Option<String>,
    pub query: String,
    pub selected: std::collections::HashSet<String>,
    pub expanded: Option<String>,
    pub sort: Sort,
    pub filter: Filter,
    pub drive_filter: Option<PathBuf>,
    pub launcher_filter: Option<String>,
    pub presets: std::collections::HashMap<String, crate::backend::Preset>,
    pub reduced_motion: bool,
    pub motion: MotionPreference,
    pub theme: ThemePreference,
    pub system_theme: iced::theme::Mode,
    pub scanning: bool,
    pub folder: String,
    pub folder_error: Option<String>,
    pub folder_kind: FolderKind,
    pub picker_busy: bool,
    pub shown: usize,
    pub detail: Animation<bool>,
    pub polling: bool,
    pub progress: std::collections::HashMap<i64, Animation<f32>>,
    pub pack_paths: std::collections::HashMap<String, String>,
    pub confirm_reclaim: std::collections::HashSet<String>,
    pub confirm_prune: std::collections::HashSet<String>,
    pub advanced: std::collections::HashSet<String>,
    pub pending: std::collections::HashSet<String>,
    analysis_queuing: bool,
    saving_order: std::collections::HashMap<String, u64>,
}
impl State {
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
            sort: Sort::Name,
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
            confirm_prune: Default::default(),
            advanced: Default::default(),
            pending: Default::default(),
            analysis_queuing: false,
            saving_order: Default::default(),
        }
    }
    pub fn show_status(&mut self, status: Status) {
        self.status_deadline = (!status.is_error).then(|| Instant::now() + Duration::from_secs(4));
        self.status = Some(status);
        self.status_reveal = Animation::new(false)
            .duration(self.motion_duration(240, 150))
            .easing(self.motion_easing())
            .go(true, Instant::now());
    }

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

    fn dismiss_status(&mut self) {
        self.status_deadline = None;
        if self.reduced_motion {
            self.status = None;
        } else {
            self.status_reveal.go_mut(false, Instant::now());
        }
    }
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
    pub fn total_bytes(&self) -> u64 {
        self.games.iter().filter_map(|g| g.game.size_hint).sum()
    }
    pub fn latest(&self, game: &Game) -> Option<&Job> {
        self.snapshot
            .jobs
            .iter()
            .rev()
            .find(|j| j.game.install_dir == game.install_dir)
    }
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

    fn optimize_command(&self, game: Game) -> Result<Command, String> {
        if self
            .recommendation(&game)
            .is_some_and(|r| r.mode == crate::recommendation::StorageMode::MaximumSpace)
        {
            let identity = self
                .estimate(&game)
                .and_then(|estimate| estimate.maximum_qualification);
            let report = self
                .reports
                .iter()
                .find(|r| {
                    r.identity().ok() == identity
                        && r.qualifies(
                            &game,
                            &r.corpus.sha256,
                            crate::compatibility::Policy::default(),
                        )
                })
                .cloned()
                .ok_or_else(|| {
                    "Refresh and analyze this game after importing its compatibility report."
                        .to_owned()
                })?;
            Ok(Command::EnqueuePack {
                task: PackTask::Activate {
                    store: self.store_path(&game),
                    create: true,
                    qualification: Some(Box::new(report)),
                },
                game,
            })
        } else {
            let row = self
                .games
                .iter()
                .find(|r| r.game.id == game.id)
                .ok_or_else(|| "Game no longer exists".to_owned())?;
            if !row.native_supported {
                return Err("Maximum Space needs a matching qualification for the primary action. Use Advanced storage to test this game locally.".into());
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
    }

    pub fn potential_saving(&self) -> u64 {
        self.games
            .iter()
            .filter(|row| row.supported && !self.compressed(&row.game))
            .filter_map(|row| self.recommendation(&row.game))
            .filter(|choice| choice.mode != crate::recommendation::StorageMode::Skip)
            .map(|choice| choice.predicted_saving)
            .sum()
    }

    pub fn current_saving(&self) -> u64 {
        let native = self
            .records
            .iter()
            .filter(|record| {
                self.games.iter().any(|row| {
                    row.game.id == record.id
                        && row.game.build == record.build
                        && !self
                            .snapshot
                            .packs
                            .iter()
                            .any(|pack| pack.game_path == row.game.install_dir)
                })
            })
            .filter_map(|record| u64::try_from(record.est_saving).ok())
            .sum::<u64>();
        let packed = self
            .snapshot
            .packs
            .iter()
            .filter_map(|install| install.summary.as_ref())
            .map(|summary| summary.logical_bytes.saturating_sub(summary.archive_bytes))
            .sum::<u64>();
        native.saturating_add(packed)
    }
    pub fn analysis_queuing(&self) -> bool {
        self.analysis_queuing
    }
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
                Sort::Saving => self
                    .saving_order
                    .get(&b.game.id.to_string())
                    .cmp(&self.saving_order.get(&a.game.id.to_string())),
            }
            .then(a.game.title.cmp(&b.game.title))
        });
        games
    }
    pub fn active(&self) -> Option<&Job> {
        self.snapshot
            .jobs
            .iter()
            .find(|j| j.phase.active() && j.phase != Phase::Queued)
            .or_else(|| self.snapshot.jobs.iter().find(|j| j.phase == Phase::Queued))
    }

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

#[derive(Debug, Clone)]
pub enum Message {
    GoTo(Page),
    Jump(&'static str),
    JumpOffset(f32),
    Scrolled(Page, f32),
    ArtworkVisible(super::artwork::Source),
    ArtworkLoaded(super::artwork::Source, Option<iced::widget::image::Handle>),
    ArtworkSaved(Result<(), String>),
    Refresh,
    Rescan,
    Scanned(Result<ScanResult, String>),
    Snapshot(Result<Snapshot, String>),
    Planned(Result<(Command, crate::storage::SpacePlan), String>),
    StartPlanned,
    CancelPlanned,
    ExportDiagnostics,
    Qualify(String),
    QualificationReady(Result<crate::qualification::Wizard, String>),
    QualificationField(crate::qualification::Field, String),
    QualificationCheck(crate::qualification::Check, bool),
    QualificationMode(crate::compatibility::StorageMode),
    SaveQualification,
    CloseQualification,
    QualificationSaved(Result<PathBuf, String>),
    DiagnosticsExported(Result<PathBuf, String>),
    AnalysisQueued(Result<Snapshot, String>),
    Dismiss,
    Tick,
    Query(String),
    Select(String, bool),
    Expand(String),
    ReviewGame(String),
    Sort(Sort),
    Filter(Filter),
    ShowMore,
    Queue(Operation),
    OptimizeLibrary,
    One(String, Operation),
    Send(Command),
    Folder(String),
    AddFolder,
    FolderKind(FolderKind),
    Browse(super::dialog::Target),
    Chosen(super::dialog::Target, Result<Option<PathBuf>, String>),
    ReportImported(Result<crate::compatibility::Report, String>),
    Preset(String, crate::backend::Preset),
    Keyboard(iced::keyboard::Event),
    Motion(MotionPreference),
    Theme(ThemePreference),
    SystemTheme(iced::theme::Mode),
    DriveFilter(Option<PathBuf>),
    LauncherFilter(Option<String>),
    PackPath(String, String),
    PackActivate(String, bool),
    PackCreate(String),
    PackReclaimPrompt(String),
    PackPrunePrompt(String),
    ToggleAdvanced(String),
}

/// Runs blocking work without occupying iced's executor or window thread.
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

fn send_unchecked(command: Command) -> Task<Message> {
    Task::perform(
        background(move || jobs::request(command).map_err(|e| e.to_string())),
        |r| Message::Snapshot(r.and_then(|r| r)),
    )
}

fn send_many(commands: Vec<Command>) -> Task<Message> {
    Task::perform(
        background(move || {
            let mut snapshot = jobs::request(Command::Snapshot).map_err(|e| e.to_string())?;
            for command in commands {
                snapshot = jobs::request(command).map_err(|e| e.to_string())?;
            }
            Ok(snapshot)
        }),
        |result| Message::Snapshot(result.and_then(|snapshot| snapshot)),
    )
}

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

pub fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Qualify(id) => {
            if let Some(game) = state
                .games
                .iter()
                .find(|row| row.game.id.to_string() == id && row.game.state.is_idle())
                .map(|row| row.game.clone())
            {
                return Task::perform(
                    background(move || {
                        crate::qualification::baseline(&game)
                            .map(|corpus| crate::qualification::Wizard::new(game, corpus))
                            .map_err(|error| error.to_string())
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
        Message::Planned(result) => match result {
            Ok(plan) => state.planned = Some(plan),
            Err(error) => state.show_status(Status::error(error)),
        },
        Message::StartPlanned => {
            if let Some((command, plan)) = state.planned.take() {
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
        Message::ArtworkVisible(source) => state.artwork_cache.request(source),
        Message::ArtworkLoaded(source, image) => state.artwork_cache.loaded(source, image),
        Message::ArtworkSaved(result) => match result {
            Ok(()) => return update(state, Message::Refresh),
            Err(error) => state.show_status(Status::error(error)),
        },
        Message::Scrolled(page, offset) => {
            state
                .scroll_positions
                .insert(page.main().label().into(), offset);
        }
        Message::Jump(section) => {
            let _navigation = update(state, Message::GoTo(Page::Settings));
            return super::surface::jump(section, Message::JumpOffset);
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
            if changed {
                state.page_direction = if page.rank() < state.page.rank() {
                    -1.0
                } else {
                    1.0
                };
                state.page = page;
                state.page_reveal = Animation::new(false)
                    .duration(state.motion_duration(180, 120))
                    .easing(state.motion_easing())
                    .go(true, Instant::now());
            }
            state.confirm_reclaim.clear();
            state.confirm_prune.clear();
            for (target, animation) in &mut state.nav {
                animation.go_mut(*target == page, Instant::now());
            }
            if let Some(section) = destination.section() {
                return super::surface::jump(section, Message::JumpOffset);
            }
            if changed {
                let offset = state
                    .scroll_positions
                    .get(page.label())
                    .copied()
                    .unwrap_or_default();
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
                    state.activity = scan.activity;
                    state.warnings = scan.warnings;
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
                    state.confirm_prune.retain(|id| valid.contains(id));
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

        Message::Snapshot(result) => match result {
            Ok(snapshot) => {
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
                let completed_work = snapshot.jobs.iter().any(|job| {
                    job.operation != Operation::Analyze
                        && !job.phase.active()
                        && state
                            .snapshot
                            .jobs
                            .iter()
                            .any(|old| old.id == job.id && old.phase.active())
                });
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
                state.snapshot = snapshot;
                state.polling = true;
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
        Message::Query(query) => {
            state.query = query;
            state.shown = 40;
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
            state.confirm_prune.clear();
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
            state.saving_order = state
                .games
                .iter()
                .map(|row| {
                    (
                        row.game.id.to_string(),
                        state.estimate(&row.game).map(|e| e.saving()).unwrap_or(0),
                    )
                })
                .collect();
        }
        Message::Filter(filter) => state.filter = filter,
        Message::ShowMore => state.shown += 40,
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
                if operation == Operation::Compress {
                    match state.optimize_command(game) {
                        Ok(command) => {
                            let navigation = update(state, Message::GoTo(Page::Queue));
                            return Task::batch([navigation, send(command)]);
                        }
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
                let navigation = if operation != Operation::Analyze {
                    update(state, Message::GoTo(Page::Queue))
                } else {
                    Task::none()
                };
                return Task::batch([
                    navigation,
                    send(Command::Enqueue {
                        game,
                        operation,
                        options,
                    }),
                ]);
            }
        }
        Message::Queue(operation) => {
            let commands: Result<Vec<_>, _> = state
                .actionable_selection()
                .into_iter()
                .map(|game| {
                    if operation == Operation::Compress {
                        state.optimize_command(game)
                    } else {
                        Ok(Command::Enqueue {
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
            let commands = match commands {
                Ok(commands) => commands,
                Err(error) => {
                    state.show_status(Status::error(error));
                    return Task::none();
                }
            };
            let navigation = update(state, Message::GoTo(Page::Queue));
            return Task::batch([navigation, send_many(commands)]);
        }
        Message::OptimizeLibrary => {
            let commands: Result<Vec<_>, _> = state
                .games
                .iter()
                .filter(|row| {
                    row.supported
                        && row.game.state.is_idle()
                        && !state.compressed(&row.game)
                        && state
                            .recommendation(&row.game)
                            .is_some_and(|r| r.mode != crate::recommendation::StorageMode::Skip)
                })
                .map(|row| state.optimize_command(row.game.clone()))
                .collect();
            match commands {
                Ok(commands) if !commands.is_empty() => {
                    let navigation = update(state, Message::GoTo(Page::Queue));
                    return Task::batch([navigation, send_many(commands)]);
                }
                Ok(_) => state.show_status(Status::info(
                    "Analysis has not found a worthwhile compression job yet.",
                )),
                Err(error) => state.show_status(Status::error(error)),
            }
        }
        Message::Send(command) => {
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
        Message::DriveFilter(path) => state.drive_filter = path,
        Message::LauncherFilter(launcher) => state.launcher_filter = launcher,
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
        Message::PackReclaimPrompt(id) => {
            state.confirm_prune.clear();
            state.confirm_reclaim.clear();
            state.confirm_reclaim.insert(id);
        }
        Message::PackPrunePrompt(id) => {
            state.confirm_reclaim.clear();
            state.confirm_prune.clear();
            state.confirm_prune.insert(id);
        }
        Message::ToggleAdvanced(id) => {
            if !state.advanced.remove(&id) {
                state.advanced.insert(id);
            }
        }
        Message::Dismiss => state.dismiss_status(),
        Message::Tick => {
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
        check_eq(state.page, Page::Settings, "jobs live in Settings")?;
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
