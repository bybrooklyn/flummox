//! Window state. Blocking work returns snapshots through tasks and subscriptions.

use crate::{
    db::{Activity, Db, GameRecord},
    fsprobe,
    jobs::{
        self, Command, Job, Library, MotionPreference, Operation, Phase, Snapshot, ThemePreference,
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
    Updates,
    Drives,
    Activity,
    Settings,
}
pub const PAGES: [Page; 6] = [
    Page::Overview,
    Page::Games,
    Page::Queue,
    Page::Updates,
    Page::Drives,
    Page::Activity,
];
impl Page {
    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Games => "Games",
            Self::Queue => "Queue",
            Self::Updates => "Updates",
            Self::Drives => "Drives",
            Self::Activity => "Activity",
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
    pub artwork: Option<PathBuf>,
}
impl GameRow {
    fn probe(game: Game, env: &Env) -> Self {
        let artwork = game.id.steam_appid().and_then(|id| {
            crate::launchers::steam::roots(env)
                .into_iter()
                .find_map(|root| {
                    let cache = root.join("appcache/librarycache");
                    [
                        format!("{id}_icon.jpg"),
                        format!("{id}_header.jpg"),
                        format!("{id}_library_600x900.jpg"),
                    ]
                    .iter()
                    .map(|name| cache.join(name))
                    .find(|p| p.is_file())
                    .or_else(|| {
                        std::fs::read_dir(cache.join(id.to_string()))
                            .ok()?
                            .flatten()
                            .map(|e| e.path())
                            .find(|p| p.extension().is_some_and(|e| e == "jpg" || e == "png"))
                    })
                })
        });
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
}
impl Filter {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All games",
            Self::Ready => "Ready",
            Self::Compressed => "Compressed",
            Self::Attention => "Needs attention",
        }
    }
}

pub struct State {
    pub env: Env,
    pub page: Page,
    pub games: Vec<GameRow>,
    pub drives: Vec<Drive>,
    pub records: Vec<GameRecord>,
    pub activity: Vec<Activity>,
    pub warnings: Vec<String>,
    pub status: Option<Status>,
    pub status_reveal: Animation<bool>,
    pub status_deadline: Option<Instant>,
    pub snapshot: Snapshot,
    pub nav: Vec<(Page, Animation<bool>)>,
    pub page_reveal: Animation<bool>,
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
            page: Page::Overview,
            games: vec![],
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
            nav: PAGES
                .into_iter()
                .map(|page| {
                    (
                        page,
                        Animation::new(page == Page::Overview)
                            .duration(Duration::from_millis(200))
                            .easing(Easing::EaseOutCubic),
                    )
                })
                .collect(),
            page_reveal: Animation::new(true)
                .duration(Duration::from_millis(240))
                .easing(Easing::EaseOutCubic),
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
            shown: 40,
            detail: Animation::new(false).duration(Duration::from_millis(200)),
            polling: false,
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
            MotionPreference::Expressive => Easing::EaseOutBack,
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
                false,
                crate::recommendation::Policy::default(),
            )
        })
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
    Refresh,
    Scanned(Result<ScanResult, String>),
    Snapshot(Result<Snapshot, String>),
    PackFinished(Result<Snapshot, String>, &'static str, Option<String>),
    AnalysisQueued(Result<Snapshot, String>),
    Dismiss,
    Tick,
    Query(String),
    Select(String, bool),
    Expand(String),
    Sort(Sort),
    Filter(Filter),
    ShowMore,
    Queue(Operation),
    OptimizeLibrary,
    One(String, Operation),
    Send(Command),
    Folder(String),
    AddFolder,
    Preset(String, crate::backend::Preset),
    Keyboard(iced::keyboard::Event),
    Motion(MotionPreference),
    Theme(ThemePreference),
    SystemTheme(iced::theme::Mode),
    DriveFilter(Option<PathBuf>),
    LauncherFilter(Option<String>),
    PackPath(String, String),
    PackActivate(String, bool),
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
    Task::perform(
        background(move || jobs::request(command).map_err(|e| e.to_string())),
        |r| Message::Snapshot(r.and_then(|r| r)),
    )
}

fn send_pack(command: Command, completed: &'static str, id: String) -> Task<Message> {
    Task::perform(
        background(move || jobs::request(command).map_err(|e| e.to_string())),
        move |result| {
            Message::PackFinished(result.and_then(|snapshot| snapshot), completed, Some(id))
        },
    )
}

fn pack_activate(state: &mut State, id: &str, create: bool) -> Task<Message> {
    if state.pending.contains(id) {
        return Task::none();
    }
    let Some(game) = state
        .games
        .iter()
        .find(|row| row.game.id.to_string() == id)
        .map(|row| row.game.clone())
    else {
        return Task::none();
    };
    let Some(store) = state
        .pack_paths
        .get(id)
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
    else {
        state.show_status(Status::error("Choose where the pack store should live."));
        return Task::none();
    };
    if store == game.install_dir || store.starts_with(&game.install_dir) {
        state.show_status(Status::error(
            "Keep the pack store outside the installed game folder.",
        ));
        return Task::none();
    }
    let Some(parent) = store.parent().filter(|parent| parent.is_dir()) else {
        state.show_status(Status::error(
            "Choose a store path inside an existing folder.",
        ));
        return Task::none();
    };
    let mut writes_name = store.as_os_str().to_os_string();
    writes_name.push(".writes");
    let writes = PathBuf::from(writes_name);
    let pool = parent.join(".flummox-pool");
    state.show_status(Status::info(if create {
        "Building and verifying the store. The original game stays in place until it is ready."
    } else {
        "Verifying and activating the existing store."
    }));
    state.polling = false;
    state.pending.insert(id.to_owned());
    let pending_id = id.to_owned();
    Task::perform(
        background(move || {
            if create {
                let binary = std::env::current_exe()
                    .map_err(|error| error.to_string())?
                    .with_file_name("flummox");
                let output = std::process::Command::new(binary)
                    .args(["pack", "create"])
                    .arg(&game.install_dir)
                    .arg(&store)
                    .arg("--maximum")
                    .arg("--pool")
                    .arg(&pool)
                    .output()
                    .map_err(|error| error.to_string())?;
                if !output.status.success() {
                    let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                    return Err(if error.is_empty() {
                        "Store creation stopped before completion.".into()
                    } else {
                        error
                    });
                }
            }
            jobs::request(Command::PackActivate {
                game_path: game.install_dir,
                store_path: store,
                writes_path: writes,
            })
            .map_err(|error| error.to_string())
        }),
        move |result| {
            Message::PackFinished(
                result.and_then(|snapshot| snapshot),
                if create {
                    "The writable compressed install is ready."
                } else {
                    "The existing store is mounted and ready."
                },
                Some(pending_id),
            )
        },
    )
}

fn automatic_pack(game: Game) -> Result<Snapshot, String> {
    let parent = game
        .install_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "The game folder has no usable parent directory.".to_owned())?;
    let root = parent.join(".flummox");
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let identity = format!(
        "{}:{}:{}",
        game.id,
        game.install_dir.display(),
        game.build.as_deref().unwrap_or_default()
    );
    let short: String = blake3::hash(identity.as_bytes())
        .to_hex()
        .chars()
        .take(16)
        .collect();
    let store = root.join(format!("{short}.store"));
    let writes = root.join(format!("{short}.writes"));
    let pool = root.join("pool");
    if !store.exists() {
        let binary = std::env::current_exe()
            .map_err(|error| error.to_string())?
            .with_file_name("flummox");
        let output = std::process::Command::new(binary)
            .args(["pack", "create"])
            .arg(&game.install_dir)
            .arg(&store)
            .arg("--maximum")
            .arg("--pool")
            .arg(&pool)
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(if error.is_empty() {
                "Store creation stopped before completion.".into()
            } else {
                error
            });
        }
    }
    jobs::request(Command::PackActivate {
        game_path: game.install_dir,
        store_path: store,
        writes_path: writes,
    })
    .map_err(|error| error.to_string())
}
fn scan(env: Env) -> ScanResult {
    let scan = crate::launchers::scan_all(&env);
    let mut warnings: Vec<_> = scan.warnings.iter().map(ToString::to_string).collect();
    let games: Vec<_> = scan
        .games
        .into_iter()
        .filter(|g| !g.is_tool)
        .map(|g| GameRow::probe(g, &env))
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
    ScanResult {
        games,
        drives,
        records,
        activity,
        warnings,
    }
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
        Message::GoTo(page) => {
            if state.page != page {
                state.page = page;
                state.page_reveal = Animation::new(false)
                    .duration(state.motion_duration(280, 160))
                    .easing(state.motion_easing())
                    .go(true, Instant::now());
            }
            state.confirm_reclaim.clear();
            state.confirm_prune.clear();
            for (target, animation) in &mut state.nav {
                animation.go_mut(*target == page, Instant::now());
            }
        }
        Message::Refresh => {
            if state.scanning {
                return Task::none();
            }
            state.scanning = true;
            let env = state.env.clone();
            return Task::perform(background(move || scan(env)), Message::Scanned);
        }
        Message::Scanned(result) => {
            state.scanning = false;
            match result {
                Ok(scan) => {
                    state.games = scan.games;
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
                let libraries_changed = state.snapshot.libraries != snapshot.libraries;
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
                state.snapshot = snapshot;
                state.polling = true;
                if libraries_changed {
                    state.show_status(Status::info("Library settings saved."));
                    return update(state, Message::Refresh);
                }
                if completed_work {
                    return update(state, Message::Refresh);
                }
                return analyze_visible(state);
            }
            Err(e) => {
                state.polling = false;
                state.show_status(Status::error(e));
            }
        },
        Message::PackFinished(result, completed, id) => {
            if let Some(id) = id {
                state.pending.remove(&id);
                state.confirm_reclaim.remove(&id);
                state.confirm_prune.remove(&id);
            }
            if result.is_ok() {
                state.show_status(Status::info(completed));
            }
            return update(state, Message::Snapshot(result));
        }
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
                if operation == Operation::Compress
                    && state.recommendation(&game).is_some_and(|choice| {
                        choice.mode == crate::recommendation::StorageMode::MaximumSpace
                    })
                {
                    state.show_status(Status::info(
                        "Building and verifying Maximum Space storage. The original remains recoverable.",
                    ));
                    let _navigation = update(state, Message::GoTo(Page::Queue));
                    state.pending.insert(id.clone());
                    let pending_id = id;
                    return Task::perform(
                        background(move || automatic_pack(game)),
                        move |result| {
                            Message::PackFinished(
                                result.and_then(|snapshot| snapshot),
                                "Maximum Space is active and launcher updates remain writable.",
                                Some(pending_id),
                            )
                        },
                    );
                }
                let options = crate::backend::CompressOpts {
                    preset: state.preset_for(&id),
                    ..Default::default()
                };
                if operation != Operation::Analyze {
                    let _navigation = update(state, Message::GoTo(Page::Queue));
                }
                return send(Command::Enqueue {
                    game,
                    operation,
                    options,
                });
            }
        }
        Message::Queue(operation) => {
            let games: Vec<_> = state
                .actionable_selection()
                .into_iter()
                .map(|game| {
                    (
                        game.clone(),
                        crate::backend::CompressOpts {
                            preset: state.preset_for(&game.id.to_string()),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            let _navigation = update(state, Message::GoTo(Page::Queue));
            return Task::perform(
                background(move || {
                    let mut snapshot =
                        jobs::request(Command::Snapshot).map_err(|e| e.to_string())?;
                    for (game, options) in games {
                        snapshot = jobs::request(Command::Enqueue {
                            game,
                            operation,
                            options,
                        })
                        .map_err(|e| e.to_string())?;
                    }
                    Ok(snapshot)
                }),
                |r| Message::Snapshot(r.and_then(|r| r)),
            );
        }
        Message::OptimizeLibrary => {
            let games: Vec<_> = state
                .games
                .iter()
                .filter(|row| {
                    row.supported
                        && row.game.state.is_idle()
                        && !state.compressed(&row.game)
                        && state.recommendation(&row.game).is_some_and(|choice| {
                            choice.mode != crate::recommendation::StorageMode::Skip
                        })
                })
                .map(|row| {
                    (
                        row.game.clone(),
                        state
                            .recommendation(&row.game)
                            .map(|choice| choice.mode)
                            .unwrap_or(crate::recommendation::StorageMode::Skip),
                        crate::backend::CompressOpts {
                            preset: state.preset_for(&row.game.id.to_string()),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            if games.is_empty() {
                state.show_status(Status::info(
                    "Analysis has not found a worthwhile native compression job yet.",
                ));
                return Task::none();
            }
            let _navigation = update(state, Message::GoTo(Page::Queue));
            return Task::perform(
                background(move || {
                    let mut snapshot =
                        jobs::request(Command::Snapshot).map_err(|e| e.to_string())?;
                    let mut libraries = std::collections::BTreeSet::new();
                    for (game, _, _) in &games {
                        if let Some(parent) = game.install_dir.parent() {
                            libraries.insert(parent.to_path_buf());
                        }
                    }
                    for path in libraries {
                        snapshot = jobs::request(Command::Library(Library {
                            path,
                            automatic: true,
                            custom: false,
                        }))
                        .map_err(|error| error.to_string())?;
                    }
                    for (game, mode, options) in games {
                        snapshot = match mode {
                            crate::recommendation::StorageMode::Native => {
                                jobs::request(Command::Enqueue {
                                    game,
                                    operation: Operation::Compress,
                                    options,
                                })
                                .map_err(|e| e.to_string())?
                            }
                            crate::recommendation::StorageMode::MaximumSpace => {
                                automatic_pack(game)?
                            }
                            crate::recommendation::StorageMode::Skip => snapshot,
                        };
                    }
                    Ok(snapshot)
                }),
                |result| Message::Snapshot(result.and_then(|snapshot| snapshot)),
            );
        }
        Message::Send(command) => {
            state.polling = false;
            let pack = match &command {
                Command::PackCompact { game_path } => Some((
                    "Compacting launcher updates. The game path remains readable while the new store is built.",
                    "Updates are compacted. Test the game before reclaiming the previous version.",
                    game_path,
                )),
                Command::PackPrune { game_path } => Some((
                    "Reclaiming the previous compacted version.",
                    "The previous compacted version was reclaimed.",
                    game_path,
                )),
                Command::PackRollback { game_path } => Some((
                    "Restoring ordinary files at the launcher path.",
                    "Ordinary game files were restored with launcher updates intact.",
                    game_path,
                )),
                Command::PackReclaim { game_path } => Some((
                    "Reclaiming the retained original files.",
                    "The retained original files were reclaimed.",
                    game_path,
                )),
                _ => None,
            };
            if let Some((working, completed, game_path)) = pack {
                let Some(id) = state
                    .games
                    .iter()
                    .find(|row| row.game.install_dir == *game_path)
                    .map(|row| row.game.id.to_string())
                else {
                    state.show_status(Status::error("The selected game is no longer installed."));
                    return Task::none();
                };
                if state.pending.contains(&id) {
                    return Task::none();
                }
                state.pending.insert(id.clone());
                state.show_status(Status::info(working));
                return send_pack(command, completed, id);
            }
            return send(command);
        }
        Message::Folder(folder) => {
            state.folder = folder;
            state.folder_error = None;
        }
        Message::AddFolder => {
            let folder = PathBuf::from(state.folder.trim());
            if !folder.is_dir() || folder.parent().is_none() {
                state.folder_error = Some("Choose an existing game folder.".into());
                return Task::none();
            }
            state.folder_error = None;
            return send(Command::Library(Library {
                path: folder,
                automatic: false,
                custom: true,
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
                    let _navigation = update(state, Message::GoTo(Page::Games));
                    return iced::widget::operation::focus(iced::widget::Id::new("game-search"));
                }
                Key::Character(key) if modifiers.command() && key.as_str() == "r" => {
                    return update(state, Message::Refresh);
                }
                _ => {}
            }
        }
        Message::Keyboard(_) => {}
    }
    Task::none()
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
        testutil::{TestResult, check, check_eq},
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
        }
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
