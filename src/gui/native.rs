//! Native Mac and Windows desktop workflows over their filesystem backends.

use anyhow::Result;
#[cfg(target_os = "macos")]
use iced::futures::SinkExt;
use iced::widget::{button, column, container, responsive, row, text};
use iced::{Alignment, Animation, Element, Length, Task, Theme, animation::Easing};
use std::time::{Duration, Instant};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::native_rules::{self as rules, Totals};
use super::shell::{self, Status, Toast};
use super::{surface, theme};
use crate::desktop::{LocationKind, MotionChoice, Preferences, ThemeChoice};
#[cfg(target_os = "macos")]
use crate::macos as backend;
#[cfg(windows)]
use crate::windows as backend;
#[cfg(windows)]
use crate::windows::coordinator;
#[cfg(windows)]
use iced::widget::checkbox;

#[cfg(windows)]
const PLATFORM: &str = "Windows · WOF/LZX";
#[cfg(target_os = "macos")]
const PLATFORM: &str = "macOS · APFS";
#[cfg(windows)]
const MODE: &str = "LZX";
#[cfg(target_os = "macos")]
const MODE: &str = "APFS";
#[cfg(windows)]
const FOLDER_HINT: &str = "C:\\Games\\Your game";
#[cfg(target_os = "macos")]
const FOLDER_HINT: &str = "~/My Games/Your game";
#[cfg(windows)]
const LIBRARY_HINT: &str = "C:\\Games or D:\\Games";
#[cfg(target_os = "macos")]
const LIBRARY_HINT: &str = "~/My Games";
#[cfg(windows)]
const RECOVERY_ACTION: &str = "Restore ordinary storage";
#[cfg(target_os = "macos")]
const RECOVERY_ACTION: &str = "Restore retained original";

/// What a finished background settings save hands back: the worker's new
/// snapshot on Windows, nothing on macOS.
#[cfg(windows)]
type Saved = coordinator::Snapshot;
#[cfg(target_os = "macos")]
type Saved = ();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Games,
    Jobs,
    Settings,
}
impl Page {
    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Games => "Games",
            Self::Jobs => "Jobs",
            Self::Settings => "Settings",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Self::Overview => shell::icons::OVERVIEW,
            Self::Games => shell::icons::GAMES,
            Self::Jobs => shell::icons::JOBS,
            Self::Settings => shell::icons::SETTINGS,
        }
    }
    /// Position in the navigation column. Decides the direction of the page transition.
    fn rank(self) -> u8 {
        match self {
            Self::Overview => 0,
            Self::Games => 1,
            Self::Jobs => 2,
            Self::Settings => 3,
        }
    }
}

/// Why `resolve_location` ran, which decides what a known path does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    /// The Add button: a listed path takes the chosen kind.
    Add,
    /// "Remember this folder": a listed path stays as it is.
    Remember,
}

/// Result of one background discovery pass, with artwork located per game id.
#[derive(Debug, Clone)]
struct Scan {
    /// Worker (epoch, revision) the games were read at. Compared with later snapshots.
    #[cfg(windows)]
    stamp: (u64, u64),
    games: Vec<crate::model::Game>,
    warnings: Vec<String>,
    /// Image for the list tile, by game id.
    artwork: HashMap<String, super::artwork::Source>,
    /// Image for the tall cover beside the controls, by game id.
    covers: HashMap<String, super::artwork::Source>,
}
/// Lists games and locates their artwork. Blocking, so it runs through `background`.
/// On Windows the list comes from the worker, which `request` starts if none is running.
fn scan() -> std::result::Result<Scan, String> {
    #[cfg(target_os = "macos")]
    let catalog = crate::native::discover_catalog().map_err(|error| error.to_string())?;
    #[cfg(windows)]
    let (catalog, stamp) = {
        let snapshot = coordinator::request(coordinator::Command::Snapshot)
            .map_err(|error| error.to_string())?;
        (
            crate::desktop_discovery::Catalog {
                games: snapshot.games,
                warnings: snapshot.warnings,
                artwork_roots: snapshot.artwork_roots,
            },
            (snapshot.epoch, snapshot.revision),
        )
    };
    let index =
        super::artwork::Index::new(catalog.artwork_roots).map_err(|error| error.to_string())?;
    let mut artwork = HashMap::new();
    let mut covers = HashMap::new();
    for game in &catalog.games {
        if let Some(source) = index.source(game) {
            artwork.insert(game.id.to_string(), source);
        }
        if let Some(source) = index.cover(game) {
            covers.insert(game.id.to_string(), source);
        }
    }
    Ok(Scan {
        #[cfg(windows)]
        stamp,
        games: catalog.games,
        warnings: catalog.warnings,
        artwork,
        covers,
    })
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum GameFilter {
    #[default]
    All,
    /// Build changed between two scans, or since the last completed compression (Windows).
    Updated,
    /// Not ready, or the latest job failed or was interrupted (Windows).
    Attention,
}
impl std::fmt::Display for GameFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::All => "All games",
            Self::Updated => "Updated games",
            Self::Attention => "Needs attention",
        })
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum GameSort {
    #[default]
    Title,
    Launcher,
}
impl std::fmt::Display for GameSort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Title => "Name",
            Self::Launcher => "Launcher",
        })
    }
}
struct State {
    /// Last snapshot accepted from the worker.
    #[cfg(windows)]
    worker: coordinator::Snapshot,
    /// Set while polls fail. The previous snapshot stays on screen.
    #[cfg(windows)]
    worker_error: Option<String>,
    /// False once the user stops the worker or a snapshot reports it stopping.
    /// Polling stops while it is false.
    #[cfg(windows)]
    worker_enabled: bool,
    /// "Stop background worker" was pressed while jobs were active.
    #[cfg(windows)]
    confirm_stop_worker: bool,
    preferences: Preferences,
    /// Saving is refused until the load succeeds, so defaults never replace the file.
    preferences_loaded: bool,
    /// Why the load failed. Shown until a load succeeds.
    preferences_error: Option<String>,
    /// The preferences as last read or written, restored when a save fails.
    saved_preferences: Preferences,
    /// The copy a save in flight is writing.
    in_flight: Option<Preferences>,
    /// Preferences changed during a save. Another save follows it.
    preferences_dirty: bool,
    /// Rescan when the pending save lands. Set when locations change.
    refresh_after_save: bool,
    location_kind: LocationKind,
    location_input: String,
    /// The reason the last Add failed, shown beside the field.
    location_error: Option<String>,
    /// A native picker dialog is open. Only one runs at a time.
    picker_busy: bool,
    warnings: Vec<String>,
    query: String,
    game_filter: GameFilter,
    game_sort: GameSort,
    /// Ids of games whose build changed between two scans in this session.
    updated: std::collections::HashSet<String>,
    artwork: HashMap<String, super::artwork::Source>,
    covers: HashMap<String, super::artwork::Source>,
    artwork_cache: super::artwork::Cache,
    page: Page,
    /// Progress of the page transition.
    reveal: Animation<bool>,
    /// Frames are requested until this instant so a programmatic scroll gets drawn.
    scroll_redraw_until: Option<Instant>,
    /// Sign of the transition offset: -1.0 towards an earlier page, 1.0 otherwise.
    direction: f32,
    /// Last scroll offset per page label, restored on navigation.
    scroll_positions: surface::Positions,
    games: Vec<crate::model::Game>,
    /// Text of the selected-folder field. Every action targets this path.
    folder: String,
    toast: Toast,
    /// The folder's less common actions are showing.
    advanced: bool,
    /// A space plan is being computed.
    planning: bool,
    /// A storage job runs on a thread of this process. Only macOS sets it.
    working: bool,
    scanning: bool,
    /// Counters of the in-process job, and the folder it works on.
    #[cfg(target_os = "macos")]
    progress: Option<backend::Progress>,
    #[cfg(target_os = "macos")]
    job_folder: Option<PathBuf>,
    /// Stop flag shared with an in-process job thread. Only the macOS path sets it.
    #[cfg(target_os = "macos")]
    cancel: Option<Arc<AtomicBool>>,
    system_theme: iced::theme::Mode,
    /// Folder, whether it is a compression, and the space plan that failed its check.
    planned: Option<(PathBuf, bool, crate::storage::SpacePlan)>,
    /// The scan in flight posts no "Found N games" line: it came from the timer,
    /// the worker or the first launch.
    refreshing: bool,
    recovery: Vec<backend::Recovery>,
    qualification: Option<crate::qualification::Wizard>,
    /// The wizard's hash is running.
    qualifying: bool,
    qualify_cancel: Arc<AtomicBool>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            #[cfg(windows)]
            worker: Default::default(),
            #[cfg(windows)]
            worker_error: None,
            #[cfg(windows)]
            worker_enabled: true,
            #[cfg(windows)]
            confirm_stop_worker: false,
            preferences: Preferences::default(),
            preferences_loaded: false,
            preferences_error: None,
            saved_preferences: Preferences::default(),
            in_flight: None,
            preferences_dirty: false,
            refresh_after_save: false,
            location_kind: LocationKind::Collection,
            location_input: String::new(),
            location_error: None,
            picker_busy: false,
            warnings: vec![],
            query: String::new(),
            game_filter: GameFilter::default(),
            game_sort: GameSort::default(),
            updated: Default::default(),
            artwork: HashMap::new(),
            covers: HashMap::new(),
            artwork_cache: super::artwork::Cache::default(),
            page: Page::Overview,
            scroll_redraw_until: None,
            reveal: Animation::new(true)
                .duration(Duration::from_millis(180))
                .easing(Easing::EaseOutCubic),
            direction: 1.0,
            scroll_positions: Default::default(),
            games: Vec::new(),
            folder: String::new(),
            toast: Toast::default(),
            advanced: false,
            planning: false,
            working: false,
            scanning: true,
            #[cfg(target_os = "macos")]
            progress: None,
            #[cfg(target_os = "macos")]
            job_folder: None,
            #[cfg(target_os = "macos")]
            cancel: None,
            system_theme: iced::theme::Mode::Dark,
            planned: None,
            refreshing: false,
            recovery: vec![],
            qualification: None,
            qualifying: false,
            qualify_cancel: Default::default(),
        }
    }
}

impl State {
    fn reduced_motion(&self) -> bool {
        self.preferences.motion == MotionChoice::Reduced
    }
    /// How long the toast takes to appear, by the motion setting.
    fn fade(&self) -> Duration {
        Duration::from_millis(match self.preferences.motion {
            MotionChoice::Normal => 240,
            MotionChoice::Subtle => 150,
            MotionChoice::Reduced => 0,
        })
    }
    fn info(&mut self, text: impl Into<String>) {
        let fade = self.fade();
        self.toast.show(Status::info(text), fade);
    }
    fn error(&mut self, text: impl Into<String>) {
        let fade = self.fade();
        self.toast.show(Status::error(text), fade);
    }
    /// Shows the worker's refusal of something the user asked for.
    #[cfg(windows)]
    fn refuse(&mut self, text: String) {
        let fade = self.fade();
        self.toast.show_refusal(text, fade);
    }
    /// A plan is being computed or a job runs in this process.
    fn busy(&self) -> bool {
        self.planning || self.working
    }
    fn selected_path(&self) -> PathBuf {
        crate::native::folder_path(&self.folder)
    }
    /// The known game whose folder is in the field.
    fn selected_game(&self) -> Option<&crate::model::Game> {
        if self.folder.trim().is_empty() {
            return None;
        }
        rules::game_at(&self.games, &self.selected_path())
    }
    #[cfg(windows)]
    fn excluded(&self, game: &crate::model::Game) -> bool {
        let ids: Vec<String> = game.ids().map(|id| id.to_string()).collect();
        rules::is_excluded(&self.preferences.excluded, &ids)
    }
    /// The job on the selected folder that Stop and the progress box describe.
    #[cfg(windows)]
    fn selected_job(&self) -> Option<&crate::desktop_jobs::Job> {
        if self.folder.trim().is_empty() {
            return None;
        }
        rules::job_for_folder(&self.worker.jobs, &self.selected_path())
    }
    /// Whether the in-process job works on the selected folder.
    #[cfg(target_os = "macos")]
    fn job_selected(&self) -> bool {
        self.cancel.is_some()
            && self
                .job_folder
                .as_deref()
                .is_some_and(|folder| folder == self.selected_path())
    }
    /// Whether Stop has a job on the selected folder to act on.
    fn can_stop(&self) -> bool {
        #[cfg(windows)]
        {
            self.selected_job().is_some()
        }
        #[cfg(target_os = "macos")]
        {
            self.job_selected()
        }
    }
    /// Stops the wizard's hash and closes the form.
    fn stop_qualifying(&mut self) {
        self.qualify_cancel.store(true, Ordering::Relaxed);
        self.qualifying = false;
        self.qualification = None;
    }
}

#[derive(Debug, Clone)]
enum Message {
    /// A poll answered, or failed.
    #[cfg(windows)]
    Worker(std::result::Result<coordinator::Snapshot, String>),
    /// The worker answered a command the user gave, or refused it.
    #[cfg(windows)]
    Commanded(std::result::Result<coordinator::Snapshot, String>),
    /// The worker answered a request to queue the game with this title.
    #[cfg(windows)]
    Queued(String, std::result::Result<coordinator::Snapshot, String>),
    #[cfg(windows)]
    WorkerCommand(coordinator::Command),
    /// Stop the worker, asking first when jobs are active.
    #[cfg(windows)]
    StopWorker,
    #[cfg(windows)]
    KeepWorker,
    /// Turn maintenance on or off for the location at this path.
    #[cfg(windows)]
    Automatic(PathBuf, bool),
    #[cfg(windows)]
    StartAtLogin(bool),
    /// Every id of a game, its primary id, and whether it is now excluded.
    #[cfg(windows)]
    Exclude(Vec<String>, String, bool),
    GoTo(Page),
    /// Open Settings and scroll to the container with this id.
    Jump(&'static str),
    /// The offset `Jump` measured for its section.
    JumpOffset(f32),
    PreferencesLoaded(std::result::Result<Preferences, String>),
    PreferencesSaved(std::result::Result<Saved, String>),
    Theme(ThemeChoice),
    Motion(MotionChoice),
    Query(String),
    Filter(GameFilter),
    Sort(GameSort),
    ClearFilters,
    /// Show the games that need attention.
    ReviewAttention,
    LocationInput(String),
    LocationKind(LocationKind),
    AddLocation,
    LocationResolved(LocationKind, Intent, std::result::Result<PathBuf, String>),
    RemoveLocation(PathBuf),
    /// Open the folder picker. `true` fills the location field, `false` the selected folder.
    BrowseFolder(bool),
    /// The picker closed. `Ok(None)` means the user cancelled.
    FolderPicked(bool, std::result::Result<Option<PathBuf>, String>),
    /// A tile scrolled into view and wants its image decoded.
    ArtworkVisible(super::artwork::Source),
    ArtworkLoaded(super::artwork::Source, Option<iced::widget::image::Handle>),
    BrowseArtwork(String),
    ClearArtwork(String),
    ArtworkPicked(String, std::result::Result<Option<PathBuf>, String>),
    ArtworkSaved(std::result::Result<(), String>),
    /// A frame passed, or a scroll set from code finished. Drives the toast's
    /// deadline and removal.
    Tick,
    /// Hide the toast.
    Dismiss,
    ToggleAdvanced,
    /// The selected-folder field was edited.
    Folder(String),
    /// A game in the list was clicked.
    Select(PathBuf),
    Refresh,
    #[cfg(target_os = "macos")]
    Poll,
    Key(iced::keyboard::Event),
    Scanned(std::result::Result<Scan, String>),
    /// Plan a compression of the selected folder. `Restore` plans the reverse.
    Optimize,
    /// The space plan for a folder is ready. The bool is true for compression.
    Planned(
        PathBuf,
        bool,
        std::result::Result<crate::storage::SpacePlan, String>,
    ),
    /// Plan the stored folder again from the drive's current free space.
    StartPlanned,
    CancelPlanned,
    /// Add the selected folder to the locations as one game.
    Remember,
    Qualify,
    QualificationReady(std::result::Result<Box<crate::qualification::Wizard>, String>),
    QualificationField(crate::qualification::Field, String),
    QualificationCheck(crate::qualification::Check, bool),
    QualificationMode(crate::compatibility::StorageMode),
    MeasureQualification,
    QualificationMeasured(std::result::Result<crate::allocation::Allocation, String>),
    SaveQualification,
    OpenChangelog,
    CloseQualification,
    QualificationSaved(std::result::Result<String, String>),
    Recover(PathBuf),
    RecoveryScanned(std::result::Result<Vec<backend::Recovery>, String>),
    Restore,
    Stop,
    #[cfg(target_os = "macos")]
    Progress(backend::Progress),
    #[cfg(target_os = "macos")]
    Finished(std::result::Result<String, String>),
    SystemTheme(iced::theme::Mode),
}

/// Runs a blocking operation on its own thread and awaits the result, so the UI
/// thread never blocks on disk, a dialog or the worker pipe.
async fn background<T: Send + 'static>(
    operation: impl FnOnce() -> std::result::Result<T, String> + Send + 'static,
) -> std::result::Result<T, String> {
    shell::background(operation).await.and_then(|result| result)
}

/// Computes the space plan for `folder`. Nothing on disk changes until a plan
/// that fits arrives as `Planned`.
fn plan(state: &mut State, folder: PathBuf, optimize: bool) -> Task<Message> {
    if state.busy() {
        return Task::none();
    }
    state.planning = true;
    let planned_folder = folder.clone();
    Task::perform(
        background(move || {
            crate::storage::per_file_plan(&folder, !optimize).map_err(|error| error.to_string())
        }),
        move |result| Message::Planned(planned_folder.clone(), optimize, result),
    )
}

// Runs the job on a thread inside this process and streams its progress back as
// messages. macOS has no worker process.
#[cfg(target_os = "macos")]
fn start(state: &mut State, folder: PathBuf, optimize: bool) -> Task<Message> {
    if state.working {
        return Task::none();
    }
    if folder.as_os_str().is_empty() {
        state.error("Choose an installed game folder first.");
        return Task::none();
    }
    if !folder.is_dir() {
        state.error("Choose an existing installed game folder.");
        return Task::none();
    }
    state.working = true;
    state.progress = None;
    state.job_folder = Some(folder.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    state.cancel = Some(cancel.clone());
    state.info(if optimize {
        format!("Compressing worthwhile files with {MODE}…")
    } else {
        "Restoring ordinary storage…".to_owned()
    });
    let stream = iced::stream::channel(32, async move |sender| {
        std::thread::spawn(move || {
            // try_send drops a progress update when the 32-slot channel is full. Each
            // update is a running total, so the next one replaces it.
            let mut progress_sender = sender.clone();
            let result = if optimize {
                backend::optimize_folder_with(&folder, &cancel, move |progress| {
                    let _sent = progress_sender.try_send(Message::Progress(progress));
                })
            } else {
                backend::restore_folder_with(&folder, &cancel, move |progress| {
                    let _sent = progress_sender.try_send(Message::Progress(progress));
                })
            }
            .map_err(|error| error.to_string());
            // The result uses a blocking send, which waits for room in the channel.
            let mut finished_sender = sender;
            let _sent =
                iced::futures::executor::block_on(finished_sender.send(Message::Finished(result)));
        });
    });
    Task::run(stream, |message| message)
}

/// Queues a job with the worker. A folder that is not a known game is sent as a
/// manual game named after the folder. The reply is `Message::Queued`.
#[cfg(windows)]
fn start(state: &mut State, folder: PathBuf, optimize: bool) -> Task<Message> {
    let game = rules::game_at(&state.games, &folder)
        .cloned()
        .unwrap_or_else(|| {
            crate::desktop::manual_game(
                folder
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Custom game".into()),
                folder,
            )
        });
    let title = game.title.clone();
    Task::perform(
        background(move || {
            coordinator::request(coordinator::Command::Enqueue {
                game,
                restore: !optimize,
            })
            .map_err(|error| error.to_string())
        }),
        move |result| Message::Queued(title.clone(), result),
    )
}

/// Sends one command the user gave to the worker, starting it if needed. The
/// reply is `Message::Commanded`, which shows a refusal as a toast.
#[cfg(windows)]
fn worker_command(command: coordinator::Command) -> Task<Message> {
    Task::perform(
        background(move || coordinator::request(command).map_err(|error| error.to_string())),
        Message::Commanded,
    )
}

/// Takes in a snapshot from the worker: the jobs, the games and the flags.
#[cfg(windows)]
fn apply_snapshot(state: &mut State, snapshot: coordinator::Snapshot) -> Task<Message> {
    // Replies can arrive out of order. Drop any that is not newer than the
    // one on screen: epoch is the worker's start time, revision counts its
    // replies.
    if state.worker.epoch > 0
        && (snapshot.epoch < state.worker.epoch
            || (snapshot.epoch == state.worker.epoch && snapshot.revision <= state.worker.revision))
    {
        return Task::none();
    }
    let changed = state.worker.games != snapshot.games;
    // A job that was active in the previous snapshot has ended.
    let finished = snapshot.jobs.iter().any(|job| {
        !job.phase.active()
            && state
                .worker
                .jobs
                .iter()
                .any(|old| old.id == job.id && old.phase.active())
    });
    state.preferences.maintenance_paused = snapshot.maintenance_paused;
    state.worker_enabled = !snapshot.stopping;
    if !state.worker_enabled {
        state.confirm_stop_worker = false;
    }
    state.worker_error = None;
    state.worker = snapshot;
    replace_games(state, state.worker.games.clone());
    state.warnings = state.worker.warnings.clone();
    let mut tasks = vec![];
    // A finished job may have left or cleared a recovery record.
    if finished {
        tasks.push(Task::perform(
            background(|| backend::recovery().map_err(|error| error.to_string())),
            Message::RecoveryScanned,
        ));
    }
    // The artwork maps were built for the old game list. Rebuild them.
    if changed && !state.scanning && state.worker_enabled {
        state.scanning = true;
        state.refreshing = true;
        tasks.push(Task::perform(background(scan), Message::Scanned));
    }
    Task::batch(tasks)
}

/// Applies one message, then starts the timer for the toast's deadline.
fn update(state: &mut State, message: Message) -> Task<Message> {
    let task = apply(state, message);
    Task::batch([task, state.toast.timer(Message::Tick)])
}

/// Applies one message. Arms that start their own task return early. The rest fall
/// through to `artwork_tasks`, which starts any image decodes the view asked for.
fn apply(state: &mut State, message: Message) -> Task<Message> {
    match message {
        #[cfg(windows)]
        Message::Exclude(ids, primary, excluded) => {
            rules::set_excluded(&mut state.preferences.excluded, &ids, &primary, excluded);
            return save_preferences(state);
        }
        #[cfg(windows)]
        Message::StartAtLogin(enabled) => {
            state.preferences.start_at_login = enabled;
            return save_preferences(state);
        }
        #[cfg(windows)]
        Message::Automatic(path, enabled) => {
            if let Some(location) = state
                .preferences
                .locations
                .iter_mut()
                .find(|location| location.path == path)
            {
                location.automatic = enabled;
            }
            return save_preferences(state);
        }
        #[cfg(windows)]
        Message::WorkerCommand(command) => {
            // Any command but Shutdown turns polling back on. `request` starts a
            // worker if none is running, which is how "Start background worker" works.
            state.worker_enabled = !matches!(command, coordinator::Command::Shutdown);
            state.confirm_stop_worker = false;
            return worker_command(command);
        }
        #[cfg(windows)]
        Message::StopWorker => {
            if rules::active_jobs(&state.worker.jobs) > 0 && !state.confirm_stop_worker {
                state.confirm_stop_worker = true;
            } else {
                return update(
                    state,
                    Message::WorkerCommand(coordinator::Command::Shutdown),
                );
            }
        }
        #[cfg(windows)]
        Message::KeepWorker => state.confirm_stop_worker = false,
        #[cfg(windows)]
        Message::Worker(result) => match result {
            Ok(snapshot) => return apply_snapshot(state, snapshot),
            Err(error) => state.worker_error = Some(error),
        },
        #[cfg(windows)]
        Message::Commanded(result) => match result {
            Ok(snapshot) => return apply_snapshot(state, snapshot),
            Err(error) => state.refuse(error),
        },
        #[cfg(windows)]
        Message::Queued(title, result) => match result {
            Ok(snapshot) => {
                state.info(format!("Added {title} to the jobs."));
                return apply_snapshot(state, snapshot);
            }
            Err(error) => state.refuse(error),
        },
        Message::PreferencesLoaded(result) => {
            state.preferences_loaded = result.is_ok();
            match result {
                Ok(preferences) => {
                    state.preferences_error = None;
                    state.saved_preferences = preferences.clone();
                    state.preferences = preferences;
                }
                Err(text) => {
                    state.preferences_error = Some(text.clone());
                    state.error(text);
                }
            }
        }
        Message::PreferencesSaved(result) => {
            let written = state.in_flight.take();
            match result {
                Err(text) => {
                    // The file still holds the last good copy, so the screen goes
                    // back to it and a queued save is dropped.
                    state.preferences = state.saved_preferences.clone();
                    state.preferences_dirty = false;
                    state.refresh_after_save = false;
                    state.error(text);
                }
                Ok(saved) => {
                    if let Some(written) = written {
                        state.saved_preferences = written;
                    }
                    let mut tasks = vec![];
                    #[cfg(windows)]
                    tasks.push(apply_snapshot(state, saved));
                    #[cfg(target_os = "macos")]
                    let () = saved;
                    if state.preferences_dirty {
                        // Preferences changed while this save ran. Write the newer copy first.
                        tasks.push(save_preferences(state));
                    } else if state.refresh_after_save {
                        state.refresh_after_save = false;
                        tasks.push(update(state, Message::Refresh));
                    }
                    return Task::batch(tasks);
                }
            }
        }
        Message::Theme(choice) => {
            state.preferences.theme = choice;
            return save_preferences(state);
        }
        Message::Motion(choice) => {
            state.preferences.motion = choice;
            return save_preferences(state);
        }
        Message::Query(query) => state.query = query,
        Message::Filter(filter) => state.game_filter = filter,
        Message::Sort(sort) => state.game_sort = sort,
        Message::ClearFilters => {
            state.query.clear();
            state.game_filter = GameFilter::All;
        }
        Message::ReviewAttention => {
            state.game_filter = GameFilter::Attention;
            state.query.clear();
            return update(state, Message::GoTo(Page::Games));
        }
        Message::LocationInput(input) => {
            state.location_input = input;
            state.location_error = None;
        }
        Message::LocationKind(kind) => state.location_kind = kind,
        Message::AddLocation => {
            if state.location_input.trim().is_empty() || !state.preferences_loaded {
                return Task::none();
            }
            state.location_error = None;
            let path = crate::native::folder_path(&state.location_input);
            return resolve_location(path, state.location_kind, Intent::Add);
        }
        Message::LocationResolved(kind, intent, result) => match result {
            Ok(path) => {
                let change = rules::apply_location(
                    &mut state.preferences.locations,
                    path,
                    kind,
                    intent == Intent::Add,
                );
                if change == rules::LocationChange::Unchanged {
                    state.info("This folder is already in your locations.");
                    return Task::none();
                }
                if intent == Intent::Add {
                    state.location_input.clear();
                }
                state.refresh_after_save = true;
                return save_preferences(state);
            }
            Err(text) => match intent {
                Intent::Add => state.location_error = Some(text),
                Intent::Remember => state.error(text),
            },
        },
        Message::RemoveLocation(path) => {
            state.preferences.remove(&path);
            state.refresh_after_save = true;
            return save_preferences(state);
        }
        Message::BrowseFolder(location) => {
            if state.picker_busy {
                return Task::none();
            }
            state.picker_busy = true;
            return Task::perform(
                background(|| crate::native::pick(false).map_err(|error| error.to_string())),
                move |result| Message::FolderPicked(location, result),
            );
        }
        Message::FolderPicked(location, result) => {
            state.picker_busy = false;
            match result {
                Ok(Some(path)) => {
                    if let Some(text) = path.to_str() {
                        if location {
                            state.location_input = text.into();
                            state.location_error = None;
                        } else {
                            state.folder = text.into();
                            state.planned = None;
                        }
                    } else {
                        state.error("This folder cannot be displayed in the path field.");
                    }
                }
                Ok(None) => {}
                Err(text) => state.error(text),
            }
        }
        Message::ArtworkVisible(source) => state.artwork_cache.request(source),
        Message::ArtworkLoaded(source, handle) => state.artwork_cache.loaded(source, handle),
        Message::BrowseArtwork(id) => {
            if state.picker_busy {
                return Task::none();
            }
            state.picker_busy = true;
            return Task::perform(
                background(|| crate::native::pick(true).map_err(|error| error.to_string())),
                move |result| Message::ArtworkPicked(id.clone(), result),
            );
        }
        Message::ClearArtwork(id) => {
            return Task::perform(
                background(move || {
                    super::artwork::clear_override(&id).map_err(|error| error.to_string())
                }),
                Message::ArtworkSaved,
            );
        }
        Message::ArtworkPicked(id, result) => {
            state.picker_busy = false;
            match result {
                Ok(Some(path)) => {
                    return Task::perform(
                        background(move || {
                            super::artwork::save_override(id, path)
                                .map_err(|error| error.to_string())
                        }),
                        Message::ArtworkSaved,
                    );
                }
                Ok(None) => {}
                Err(text) => state.error(text),
            }
        }
        Message::ArtworkSaved(result) => match result {
            Ok(()) => {
                state.refreshing = true;
                return update(state, Message::Refresh);
            }
            Err(text) => state.error(text),
        },
        Message::Jump(section) => {
            // `GoTo` can return artwork decodes it has already marked as
            // pending. Dropping that task left them pending for good, and
            // with two decodes allowed at once artwork loading could stall.
            let navigation = update(state, Message::GoTo(Page::Settings));
            return navigation.chain(surface::jump("Settings", section, Message::JumpOffset));
        }
        Message::JumpOffset(offset) => {
            return surface::scroll_to_offset(
                &mut state.scroll_redraw_until,
                "Settings",
                offset,
                Message::Tick,
            );
        }
        Message::GoTo(page) => {
            #[cfg(windows)]
            {
                state.confirm_stop_worker = false;
            }
            if state.page != page {
                // Restart the transition, then put the page back at its last offset.
                state.direction = if page.rank() < state.page.rank() {
                    -1.0
                } else {
                    1.0
                };
                state.page = page;
                state.reveal = Animation::new(false)
                    .duration(state.preferences.motion.duration())
                    .easing(Easing::EaseOutCubic)
                    .go(true, Instant::now());
                return surface::restore_offset(
                    page.label(),
                    surface::recorded(&state.scroll_positions, page.label()),
                    Message::Tick,
                );
            }
        }
        Message::Tick => {
            let instant = state.reduced_motion();
            state.toast.tick(instant);
        }
        Message::Dismiss => {
            let instant = state.reduced_motion();
            state.toast.dismiss(instant);
        }
        Message::ToggleAdvanced => state.advanced = !state.advanced,

        // The compatibility wizard. Its state lives in `state.qualification`, and
        // these arms forward to it while it is open.
        Message::Qualify => {
            // The hash reads every file, so a second press while it runs
            // would start a second read of the install.
            if state.qualifying {
                return Task::none();
            }
            let game = state.selected_game().cloned();
            if let Some(reason) = rules::qualify_block(game.as_ref()) {
                state.error(reason);
                return Task::none();
            }
            if let Some(game) = game {
                state.qualifying = true;
                state.qualify_cancel = Default::default();
                let cancel = state.qualify_cancel.clone();
                return Task::perform(
                    background(move || {
                        crate::qualification::Wizard::start_cancellable(
                            game,
                            &cancel,
                            &crate::qualification::NoObserver,
                        )
                        .map(Box::new)
                        .map_err(|error| error.to_string())
                    }),
                    Message::QualificationReady,
                );
            }
        }
        Message::QualificationReady(result) => {
            // A result that arrives after the user cancelled is dropped.
            if std::mem::take(&mut state.qualifying) {
                match result {
                    Ok(wizard) => state.qualification = Some(*wizard),
                    Err(error) => state.error(error),
                }
            }
        }
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
        Message::CloseQualification => state.stop_qualifying(),
        Message::OpenChangelog => {
            if let Err(error) = super::open_changelog() {
                state.error(format!(
                    "Could not open a browser ({error}). The changelog is at {}",
                    super::CHANGELOG_URL
                ));
            }
        }
        Message::MeasureQualification => {
            if let Some(wizard) = &state.qualification {
                let roots = vec![wizard.game.install_dir.clone()];
                return Task::perform(
                    background(move || {
                        crate::allocation::measure(&roots).map_err(|error| format!("{error:#}"))
                    }),
                    Message::QualificationMeasured,
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
                                    .map(|path| format!("Report saved to {}", path.display()))
                                    .map_err(|error| error.to_string())
                            }),
                            Message::QualificationSaved,
                        );
                    }
                    Err(error) => state.error(error.to_string()),
                }
            }
        }
        Message::QualificationSaved(result) => {
            state.stop_qualifying();
            match result {
                Ok(text) => state.info(text),
                Err(text) => state.error(text),
            }
        }
        Message::Folder(folder) => {
            state.folder = folder;
            state.planned = None;
        }
        Message::Select(folder) => {
            let navigation = update(state, Message::GoTo(Page::Games));
            state.folder = folder.display().to_string();
            state.planned = None;
            return navigation;
        }
        // Tab and Shift+Tab move focus. Escape closes the plan. The command key with
        // F searches and with R scans again.
        Message::Key(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            if let Some(focus) = shell::tab_focus(&key, modifiers) {
                return focus;
            }
            match shell::shortcut(&key, modifiers) {
                Some(shell::Shortcut::Search) => {
                    let navigation = update(state, Message::GoTo(Page::Games));
                    return Task::batch([
                        navigation,
                        iced::widget::operation::focus(iced::widget::Id::new("game-search")),
                    ]);
                }
                Some(shell::Shortcut::Rescan) => return update(state, Message::Refresh),
                None => {}
            }
            if key == iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape) {
                state.planned = None;
            }
        }
        Message::Key(_) => {}
        #[cfg(target_os = "macos")]
        Message::Poll => {
            // The timer rescan waits while a job, a scan, a pending plan or the
            // wizard is using the current game list.
            if !state.busy()
                && !state.scanning
                && state.planned.is_none()
                && state.qualification.is_none()
            {
                state.refreshing = true;
                return update(state, Message::Refresh);
            }
        }
        Message::Refresh => {
            if state.scanning || state.busy() {
                return Task::none();
            }
            state.scanning = true;
            let scan = Task::perform(background(scan), Message::Scanned);
            // The worker keeps its own game list, so ask it to rediscover as well.
            #[cfg(windows)]
            return Task::batch([scan, worker_command(coordinator::Command::Refresh)]);
            #[cfg(target_os = "macos")]
            return scan;
        }
        Message::Scanned(games) => {
            state.scanning = false;
            let quiet = std::mem::take(&mut state.refreshing);
            match games {
                Ok(scan) => {
                    // This scan read an older snapshot than the one on screen and
                    // disagrees with it. Applying it would roll the list back, so
                    // scan again.
                    #[cfg(windows)]
                    if scan.stamp < (state.worker.epoch, state.worker.revision)
                        && scan.games != state.worker.games
                    {
                        if state.worker_enabled {
                            state.scanning = true;
                            state.refreshing = quiet;
                            return Task::perform(background(self::scan), Message::Scanned);
                        }
                        return Task::none();
                    }
                    replace_games(state, scan.games);
                    state.warnings = scan.warnings;
                    state.artwork = scan.artwork;
                    state.covers = scan.covers;
                }
                Err(error) => {
                    state.error(error);
                    return Task::none();
                }
            }
            let error_showing = state
                .toast
                .status
                .as_ref()
                .is_some_and(|status| status.is_error);
            if let Some(text) = rules::scan_announcement(state.games.len(), quiet, error_showing) {
                state.info(text);
            }
        }
        Message::Optimize => {
            let folder = state.selected_path();
            return plan(state, folder, true);
        }
        Message::Restore => {
            let folder = state.selected_path();
            return plan(state, folder, false);
        }
        Message::Planned(folder, optimize, result) => {
            state.planning = false;
            match result {
                // A plan with room to spare starts at once. The review appears
                // only when the plan fails its own check, to say what is short.
                Ok(plan) if plan.check().is_ok() => {
                    state.planned = None;
                    // Free space and the mounted volume may have changed since the
                    // plan was computed.
                    match plan.recheck() {
                        Ok(()) => return start(state, folder, optimize),
                        Err(error) => state.error(error.to_string()),
                    }
                }
                Ok(plan) => state.planned = Some((folder, optimize, plan)),
                Err(error) => state.error(error),
            }
        }
        Message::StartPlanned => {
            // The stored numbers are the ones that failed, so checking them again
            // could not change the answer. The folder is planned afresh.
            if let Some((folder, optimize, _)) = state.planned.clone() {
                return plan(state, folder, optimize);
            }
        }
        Message::CancelPlanned => state.planned = None,
        Message::Remember => {
            if state.folder.trim().is_empty() || !state.preferences_loaded {
                return Task::none();
            }
            return resolve_location(state.selected_path(), LocationKind::Game, Intent::Remember);
        }
        // On Windows recovery is an ordinary restore job queued with the worker, so it
        // goes through the space plan like Decompress.
        #[cfg(windows)]
        Message::Recover(folder) => return plan(state, folder, false),
        #[cfg(target_os = "macos")]
        Message::Recover(folder) => {
            state.working = true;
            state.job_folder = None;
            state.info("Recovering the retained original…");
            return Task::perform(
                background(move || {
                    backend::recover_folder(&folder)
                        .map(|()| "Recovery finished.".into())
                        .map_err(|error| error.to_string())
                }),
                Message::Finished,
            );
        }
        Message::RecoveryScanned(result) => match result {
            Ok(records) => state.recovery = records,
            Err(error) => state.error(error),
        },
        Message::Stop => {
            // Windows cancels the selected folder's job. macOS raises the flag its
            // job thread checks before each file.
            #[cfg(windows)]
            if let Some(job) = state.selected_job() {
                return worker_command(coordinator::Command::Cancel(job.id));
            }
            #[cfg(target_os = "macos")]
            if let Some(cancel) = state.cancel.clone() {
                cancel.store(true, Ordering::Relaxed);
                state.info("Stopping after the current file…");
            }
        }
        #[cfg(target_os = "macos")]
        Message::Progress(progress) => state.progress = Some(progress),
        #[cfg(target_os = "macos")]
        Message::Finished(result) => {
            // A stopped job returns an error. Report it as a stop, then reread the
            // recovery journal either way.
            let stopped = state
                .cancel
                .take()
                .is_some_and(|cancel| cancel.load(Ordering::Relaxed));
            state.working = false;
            state.progress = None;
            state.job_folder = None;
            match (stopped, result) {
                (true, _) => state.info("Stopped. Files already processed remain valid."),
                (false, Ok(text)) => state.info(text),
                (false, Err(text)) => state.error(text),
            }
            return Task::perform(
                background(|| backend::recovery().map_err(|error| error.to_string())),
                Message::RecoveryScanned,
            );
        }
        Message::SystemTheme(theme) => state.system_theme = theme,
    }
    shell::artwork_tasks(&mut state.artwork_cache, Message::ArtworkLoaded)
}

/// Swaps in a new game list. A game whose build differs from the old list is marked
/// updated. On Windows the mark clears once the latest completed compression job for
/// the game covers its current build.
fn replace_games(state: &mut State, games: Vec<crate::model::Game>) {
    for game in &games {
        if state
            .games
            .iter()
            .any(|old| old.id == game.id && old.build != game.build)
        {
            state.updated.insert(game.id.to_string());
        }
        #[cfg(windows)]
        if state
            .worker
            .jobs
            .iter()
            .rev()
            .find(|job| {
                job.phase == crate::desktop_jobs::Phase::Completed
                    && !job.restore
                    && job.game.id == game.id
            })
            .is_some_and(|job| job.game.build == game.build)
        {
            state.updated.remove(&game.id.to_string());
        }
    }
    state.games = games;
}

/// The phase of the newest job for `game`. macOS keeps no job history.
fn latest_phase(state: &State, game: &crate::model::Game) -> Option<crate::desktop_jobs::Phase> {
    #[cfg(windows)]
    {
        rules::latest_phase(&state.worker.jobs, game)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (state, game);
        None
    }
}

/// Games matching the search box and filter, in the chosen order. The search is a
/// case-insensitive substring match on the title.
fn filtered_games(state: &State) -> Vec<&crate::model::Game> {
    let query = state.query.to_lowercase();
    let mut games: Vec<_> = state
        .games
        .iter()
        .filter(|game| {
            let updated = state.updated.contains(&game.id.to_string());
            // The job history survives a restart of the window, so it also marks a
            // game whose latest completed compression was for another build.
            #[cfg(windows)]
            let updated = updated
                || state
                    .worker
                    .jobs
                    .iter()
                    .rev()
                    .find(|job| {
                        job.phase == crate::desktop_jobs::Phase::Completed
                            && !job.restore
                            && job.game.id == game.id
                    })
                    .is_some_and(|job| job.game.build != game.build);
            game.title.to_lowercase().contains(&query)
                && match state.game_filter {
                    GameFilter::All => true,
                    GameFilter::Updated => updated,
                    GameFilter::Attention => {
                        rules::needs_attention(game, latest_phase(state, game))
                    }
                }
        })
        .collect();
    // Both orders use one key shape. Title order leaves the launcher slot empty, and
    // the id breaks ties so the order is stable between scans.
    games.sort_by_key(|game| match state.game_sort {
        GameSort::Title => (
            String::new(),
            game.title.to_lowercase(),
            game.id.to_string(),
        ),
        GameSort::Launcher => (
            game.id.launcher.label().into(),
            game.title.to_lowercase(),
            game.id.to_string(),
        ),
    });
    games
}

/// Whether a page transition, the toast or a programmatic scroll still needs frames.
fn animating(state: &State) -> bool {
    (!state.reduced_motion()
        && (state.reveal.is_animating(Instant::now()) || state.toast.animating()))
        || state
            .scroll_redraw_until
            .is_some_and(|until| Instant::now() < until)
}

/// Lays the window out, compact under `shell::COMPACT_BELOW` wide. The second
/// argument keeps frames coming while something moves.
fn view(state: &State) -> Element<'_, Message> {
    surface::animate(
        responsive(move |size| layout(state, size.width < shell::COMPACT_BELOW)),
        animating(state),
    )
}

/// The refresh button every page header carries.
fn refresh_button(state: &State) -> Element<'_, Message> {
    theme::secondary_maybe(
        if state.scanning {
            "Refreshing…"
        } else {
            "Refresh"
        },
        (!state.scanning && !state.busy()).then_some(Message::Refresh),
    )
}

/// The Overview page: what the window will do, the totals, anything that needs a
/// look, and the work in progress.
fn overview(state: &State) -> Element<'_, Message> {
    let finding = state.scanning && state.games.is_empty();
    let attention = state
        .games
        .iter()
        .filter(|game| rules::needs_attention(game, latest_phase(state, game)))
        .count();
    let mut content = column![
        theme::page_header("Overview", None, Some(refresh_button(state))),
        theme::hero_card(
            column![
                text(if finding {
                    "Finding your games…".to_owned()
                } else {
                    format!("Save space with {MODE}")
                })
                .size(27),
                theme::muted("Games stay in place and launch normally"),
                row![
                    theme::stat(
                        if finding {
                            "…".to_owned()
                        } else {
                            state.games.len().to_string()
                        },
                        "Games"
                    ),
                    theme::stat(MODE.into(), "Mode")
                ]
                .spacing(32),
                row![
                    theme::action("Choose games", Message::GoTo(Page::Games)),
                    theme::secondary("View jobs", Message::GoTo(Page::Jobs)),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            ]
            .spacing(14)
        ),
    ]
    .spacing(theme::PAGE_GAP);
    if attention > 0 || !state.warnings.is_empty() {
        content = content.push(shell::attention_notes(
            attention,
            &state.warnings,
            Some(Message::ReviewAttention),
        ));
    }
    content = content.push(theme::section_title("Running now"));
    content = content.push(theme::panel_card(text(current_work(state))));
    content.into()
}

/// The search box, the filter and the sort order, in one row when wide.
fn search_bar(state: &State, compact: bool) -> Element<'_, Message> {
    let search = shell::input("Search your games", &state.query)
        .id("game-search")
        .on_input(Message::Query)
        .width(Length::Fill);
    let filter = shell::picker(
        [GameFilter::All, GameFilter::Updated, GameFilter::Attention],
        Some(state.game_filter),
        Message::Filter,
    );
    let sort = shell::picker(
        [GameSort::Title, GameSort::Launcher],
        Some(state.game_sort),
        Message::Sort,
    );
    if compact {
        column![
            search,
            row![filter, sort].spacing(8).align_y(Alignment::Center)
        ]
        .spacing(8)
        .into()
    } else {
        row![search, filter, sort]
            .spacing(8)
            .align_y(Alignment::Center)
            .into()
    }
}

/// What the Games list says when it has no rows, and the way out of it.
fn empty_games(state: &State) -> (&'static str, &'static str, Option<(&'static str, Message)>) {
    if state.games.is_empty() && state.scanning {
        (
            "Finding your games…",
            "This can take a moment on a large library",
            None,
        )
    } else if state.games.is_empty() {
        (
            "No games found",
            "Add a folder that holds your games",
            Some(("Add a folder", Message::Jump("settings-locations"))),
        )
    } else if !state.query.trim().is_empty() {
        (
            "No games match your search",
            "Try fewer letters or clear the search",
            Some(("Clear search", Message::Query(String::new()))),
        )
    } else {
        (
            "No games match these filters",
            "The filters hide every game",
            Some(("Show all games", Message::ClearFilters)),
        )
    }
}

/// One game in the list. The row for the folder in the field is drawn selected.
fn game_row<'a>(state: &'a State, game: &'a crate::model::Game) -> Element<'a, Message> {
    let selected = !state.folder.trim().is_empty() && game.install_dir == state.selected_path();
    let mut status = rules::game_status(game, latest_phase(state, game));
    if let Some(bytes) = game.size_hint {
        status = format!("{status} · {}", shell::size(bytes));
    }
    button(
        row![
            shell::artwork_tile(
                &state.artwork_cache,
                state.artwork.get(&game.id.to_string()),
                &game.title,
                shell::Tile::Row(44),
                Message::ArtworkVisible,
            ),
            column![text(&game.title).size(16), theme::muted(status)]
                .spacing(4)
                .width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding(theme::BUTTON_PADDING)
    .style(if selected {
        theme::selected_button
    } else {
        button::text
    })
    .on_press(Message::Select(game.install_dir.clone()))
    .into()
}

/// The list of games with its heading, or the panel that says why it is empty.
fn library(state: &State) -> Element<'_, Message> {
    let filtered = filtered_games(state);
    let mut list = column![theme::section_text(format!(
        "Library · {}",
        shell::games_count(state.games.len())
    ))]
    .spacing(10);
    if filtered.is_empty() {
        let (title, hint, way_out) = empty_games(state);
        return list.push(shell::empty_panel(title, hint, way_out)).into();
    }
    let rows: Vec<_> = filtered
        .into_iter()
        .map(|game| {
            (
                *blake3::hash(game.id.to_string().as_bytes()).as_bytes(),
                game_row(state, game),
            )
        })
        .collect();
    list = list.push(iced::widget::keyed_column(rows).spacing(8));
    list.into()
}

/// The totals box for the job on the selected folder, with a title naming it.
fn progress_box<'a>(title: String, totals: Totals, note: &'a str) -> Element<'a, Message> {
    let mut content = column![
        theme::section_text(title),
        text(format!(
            "{} files processed · {} changed · {} skipped",
            totals.files, totals.changed, totals.skipped
        ))
        .size(13),
        theme::muted(format!(
            "{} scanned · {} freed so far",
            shell::size(totals.bytes),
            shell::size(totals.freed())
        )),
    ]
    .spacing(4);
    if !note.is_empty() {
        content = content.push(theme::muted(note));
    }
    theme::hero_card(content)
}

/// The progress box of the job on the selected folder, if there is one.
fn selected_progress(state: &State) -> Option<Element<'_, Message>> {
    #[cfg(windows)]
    {
        let job = state.selected_job()?;
        let totals = Totals {
            files: job.progress.files,
            changed: job.progress.changed,
            skipped: job.progress.skipped,
            bytes: job.progress.bytes,
            allocation_before: job.progress.allocation_before,
            allocation_after: job.progress.allocation_after,
        };
        Some(progress_box(
            format!(
                "{} · {} · {}",
                rules::operation_words(!job.restore),
                job.game.title,
                job.phase
            ),
            totals,
            &job.message,
        ))
    }
    #[cfg(target_os = "macos")]
    {
        if !state.job_selected() {
            return None;
        }
        let progress = state.progress.clone().unwrap_or_default();
        let totals = Totals {
            files: progress.files,
            changed: progress.changed,
            skipped: progress.skipped,
            bytes: progress.bytes,
            allocation_before: progress.allocation_before,
            allocation_after: progress.allocation_after,
        };
        Some(progress_box(
            format!("Working on {}", state.folder.trim()),
            totals,
            "",
        ))
    }
}

/// The selected folder and everything that acts on it.
fn controls(state: &State) -> Element<'_, Message> {
    let folder_empty = state.folder.trim().is_empty();
    let game = state.selected_game();
    let idle = !state.busy();
    #[cfg(windows)]
    let excluded = game.is_some_and(|game| state.excluded(game));
    #[cfg(target_os = "macos")]
    let excluded = false;
    let compress_block = rules::compress_block(folder_empty, game, excluded);
    let decompress_block = rules::decompress_block(folder_empty);
    let mut panel = column![
        theme::section_title("Selected folder"),
        row![
            shell::input(FOLDER_HINT, &state.folder)
                .on_input_maybe(idle.then_some(Message::Folder))
                .width(Length::Fill),
            theme::secondary_maybe(
                "Browse…",
                (!state.picker_busy && idle).then_some(Message::BrowseFolder(false))
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        theme::muted("Compress skips files the filesystem cannot shrink"),
        row![
            theme::action_maybe(
                "Compress",
                (idle && compress_block.is_none()).then_some(Message::Optimize)
            ),
            theme::secondary_maybe("Stop", state.can_stop().then_some(Message::Stop)),
            theme::secondary_maybe(
                "Remember this folder",
                (idle && !folder_empty && state.preferences_loaded).then_some(Message::Remember)
            ),
        ]
        .spacing(8)
        .wrap(),
    ]
    .spacing(12);
    if let Some(reason) = compress_block {
        panel = panel.push(theme::muted(reason));
    }
    if state.planning {
        panel = panel.push(theme::muted("Checking free space…"));
    }
    if let Some(note) =
        rules::preferences_note(state.preferences_loaded, state.preferences_error.as_deref())
        && state.preferences_error.is_some()
    {
        panel = panel.push(theme::warning_text(note));
    }
    if let Some(progress) = selected_progress(state) {
        panel = panel.push(progress);
    }
    panel = panel.push(theme::disclosure(
        if state.advanced {
            "Hide advanced"
        } else {
            "Advanced"
        },
        state.advanced,
        Message::ToggleAdvanced,
    ));
    if state.advanced {
        let mut tools = row![
            theme::secondary_maybe(
                "Decompress",
                (idle && decompress_block.is_none()).then_some(Message::Restore)
            ),
            theme::secondary_maybe(
                if state.qualifying {
                    "Measuring…"
                } else {
                    "Qualify compatibility"
                },
                (!state.qualifying && rules::qualify_block(game).is_none())
                    .then_some(Message::Qualify)
            ),
        ]
        .spacing(8);
        if state.qualifying {
            tools = tools.push(theme::secondary("Cancel", Message::CloseQualification));
        }
        panel = panel.push(tools.wrap());
        if let Some(reason) = decompress_block {
            panel = panel.push(theme::muted(reason));
        }
        if let Some(reason) = rules::qualify_block(game) {
            panel = panel.push(theme::muted(reason));
        }
        // Exclusion and artwork controls need a game id, so they appear only when the
        // folder text resolves to a known game.
        if let Some(game) = game {
            #[cfg(windows)]
            {
                let ids: Vec<String> = game.ids().map(|id| id.to_string()).collect();
                let primary = game.id.to_string();
                panel = panel.push(theme::secondary_maybe(
                    if excluded {
                        "Include this game"
                    } else {
                        "Exclude this game"
                    },
                    state
                        .preferences_loaded
                        .then(|| Message::Exclude(ids, primary, !excluded)),
                ));
            }
            panel = panel.push(shell::artwork_tile(
                &state.artwork_cache,
                state.covers.get(&game.id.to_string()),
                &game.title,
                shell::Tile::Cover,
                Message::ArtworkVisible,
            ));
            panel = panel.push(
                row![
                    theme::secondary_maybe(
                        "Choose local artwork…",
                        (!state.picker_busy).then(|| Message::BrowseArtwork(game.id.to_string()))
                    ),
                    theme::secondary(
                        "Use default artwork",
                        Message::ClearArtwork(game.id.to_string())
                    ),
                ]
                .spacing(8)
                .wrap(),
            );
        }
    }
    if let Some(wizard) = &state.qualification {
        panel = panel.push(crate::qualification::view(
            wizard,
            Message::QualificationField,
            Message::QualificationCheck,
            Message::QualificationMode,
            Message::MeasureQualification,
            Message::SaveQualification,
            Message::CloseQualification,
        ));
    }
    theme::panel_card(panel)
}

/// The Games page: search, then the list beside the selected folder's controls,
/// stacked when compact.
fn games_page(state: &State, compact: bool) -> Element<'_, Message> {
    let library = library(state);
    let header = theme::page_header("Games", None, Some(refresh_button(state)));
    let search = search_bar(state, compact);
    if compact {
        column![header, search, theme::panel_card(library), controls(state)]
            .spacing(theme::PAGE_GAP)
            .into()
    } else {
        let list = container(library)
            .padding(theme::CARD_PADDING)
            .width(Length::FillPortion(5))
            .style(theme::panel);
        let side = container(controls(state)).width(Length::FillPortion(6));
        column![
            header,
            search,
            row![list, side]
                .spacing(theme::PAGE_GAP)
                .align_y(Alignment::Start)
        ]
        .spacing(theme::PAGE_GAP)
        .into()
    }
}

/// The sidebar, the page and the toast, with the plan review above the page.
fn layout(state: &State, compact: bool) -> Element<'_, Message> {
    let page = state.page;
    let content: Element<'_, Message> = match page {
        Page::Overview => overview(state),
        Page::Games => games_page(state, compact),
        Page::Jobs => jobs_page(state),
        Page::Settings => settings_page(state),
    };
    // The plan is reviewed above whichever page started it, since Recovery can
    // start one from Settings.
    let review: Element<'_, Message> = match &state.planned {
        Some((folder, optimize, plan)) => container(shell::plan_review(
            Some(format!(
                "{} · {}",
                rules::operation_words(*optimize),
                folder_title(state, folder)
            )),
            plan,
            Message::StartPlanned,
            Message::CancelPlanned,
        ))
        .padding(iced::Padding::default().bottom(theme::PAGE_GAP))
        .into(),
        None => iced::widget::Space::new().into(),
    };
    // The page slides in over the motion distance as `reveal` runs from 0 to 1.
    // Reduced motion pins it at 1.
    let reveal = if state.reduced_motion() {
        1.0
    } else {
        state.reveal.interpolate(0.0, 1.0, Instant::now())
    };
    let body = shell::page_surface(
        column![review, content].into(),
        page.label(),
        compact,
        state.direction * state.preferences.motion.distance() * (1.0 - reveal),
        !state.reduced_motion(),
        state.scroll_positions.clone(),
    );
    let entry = |destination: Page| {
        shell::nav_entry(
            destination.label(),
            destination.icon(),
            if destination == page { 1.0 } else { 0.0 },
            compact,
            Message::GoTo(destination),
        )
    };
    let sidebar = shell::sidebar(
        compact,
        [Page::Overview, Page::Games, Page::Jobs]
            .into_iter()
            .map(entry)
            .collect(),
        entry(Page::Settings),
    );
    shell::with_toast(
        shell::frame(sidebar, body),
        &state.toast,
        state.reduced_motion(),
        Message::Dismiss,
    )
}

/// The title a job on `folder` goes by: the game's, else the folder's name.
fn folder_title(state: &State, folder: &Path) -> String {
    rules::game_at(&state.games, folder)
        .map(|game| game.title.clone())
        .or_else(|| {
            folder
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| folder.display().to_string())
}

fn theme(state: &State) -> Theme {
    shell::window_theme(
        state.system_theme,
        match state.preferences.theme {
            ThemeChoice::System => None,
            ThemeChoice::Dark => Some(true),
            ThemeChoice::Light => Some(false),
        },
    )
}

/// Starts the first scan, the preference load and the recovery check side by side.
fn boot() -> (State, Task<Message>) {
    // The first scan announces nothing; the list filling in is the news.
    let state = State {
        refreshing: true,
        ..State::default()
    };
    let task = Task::batch([
        Task::perform(background(scan), Message::Scanned),
        iced::system::theme().map(Message::SystemTheme),
        Task::perform(
            background(|| {
                crate::libraries::data_dir()
                    .and_then(|root| Preferences::load(&root))
                    .map_err(|error| error.to_string())
            }),
            Message::PreferencesLoaded,
        ),
        Task::perform(
            background(|| backend::recovery().map_err(|error| error.to_string())),
            Message::RecoveryScanned,
        ),
    ]);
    (state, task)
}

// Emits Poll every 30 seconds. The sleep runs on a helper thread through
// `background`, so it does not block the executor.
#[cfg(target_os = "macos")]
fn polls() -> impl iced::futures::Stream<Item = Message> {
    iced::futures::stream::unfold((), |()| async {
        let _waited = background(|| {
            std::thread::sleep(std::time::Duration::from_secs(30));
            Ok(())
        })
        .await;
        Some((Message::Poll, ()))
    })
}

/// Runs the native desktop app.
pub fn run() -> Result<()> {
    iced::application(boot, update, view)
        .title("Flummox")
        .theme(theme)
        .subscription(|state: &State| {
            iced::Subscription::batch([
                // Frame ticks only while something is moving. An idle window does
                // not redraw.
                if animating(state) {
                    iced::window::frames().map(|_| Message::Tick)
                } else {
                    iced::Subscription::none()
                },
                iced::system::theme_changes().map(Message::SystemTheme),
                poll_subscription(state),
                iced::keyboard::listen().map(Message::Key),
            ])
        })
        .default_font(theme::BODY_FONT)
        .window(iced::window::Settings {
            size: iced::Size::new(shell::WINDOW_SIZE.0, shell::WINDOW_SIZE.1),
            min_size: Some(iced::Size::new(shell::MIN_WINDOW.0, shell::MIN_WINDOW.1)),
            ..iced::window::Settings::default()
        })
        .run()?;
    Ok(())
}

/// Canonicalises a typed path off the UI thread. Rejects anything that is not an
/// existing directory with a parent, which rules out a drive or filesystem root.
fn resolve_location(path: PathBuf, kind: LocationKind, intent: Intent) -> Task<Message> {
    Task::perform(
        background(move || {
            let path = path.canonicalize().map_err(|error| error.to_string())?;
            if !path.is_dir() || path.parent().is_none() {
                return Err("Choose an existing game or games library".into());
            }
            Ok(path)
        }),
        move |result| Message::LocationResolved(kind, intent, result),
    )
}
/// Writes the preferences, one save at a time. On Windows the worker writes the file
/// and applies the change in the same step. On macOS this process writes it.
fn save_preferences(state: &mut State) -> Task<Message> {
    if !state.preferences_loaded {
        return Task::none();
    }
    // A save is running. `PreferencesSaved` starts the next one when it returns.
    if state.in_flight.is_some() {
        state.preferences_dirty = true;
        return Task::none();
    }
    state.preferences_dirty = false;
    let preferences = state.preferences.clone();
    state.in_flight = Some(preferences.clone());
    Task::perform(
        background(move || {
            #[cfg(target_os = "macos")]
            let result = crate::libraries::data_dir().and_then(|root| preferences.save(&root));
            #[cfg(windows)]
            let result = coordinator::request(coordinator::Command::Settings(preferences));
            result.map_err(|error| error.to_string())
        }),
        Message::PreferencesSaved,
    )
}

/// A titled card, so a Settings section has one look.
fn section<'a>(
    title: &'a str,
    body: impl Into<Element<'a, Message>>,
) -> iced::widget::Column<'a, Message> {
    column![theme::section_title(title), body.into()].spacing(10)
}

/// The Settings page: locations, recovery, maintenance, appearance, reports and
/// about, each in a card.
fn settings_page(state: &State) -> Element<'_, Message> {
    // Each id here must match the `.id(...)` of a container in `content` below.
    let jumps = iced::widget::Row::with_children(
        [
            ("Drives & libraries", "settings-locations"),
            ("Recovery", "settings-recovery"),
            ("Maintenance", "settings-maintenance"),
            ("Appearance", "settings-appearance"),
            ("Reports", "settings-reports"),
            ("About Flummox", "settings-about"),
        ]
        .into_iter()
        .map(|(label, section)| theme::secondary(label, Message::Jump(section))),
    )
    .spacing(8)
    .wrap();
    let mut content = column![theme::page_header::<Message>("Settings", None, None), jumps]
        .spacing(theme::PAGE_GAP);
    if let Some(note) =
        rules::preferences_note(state.preferences_loaded, state.preferences_error.as_deref())
    {
        content = content.push(theme::attention_card(
            theme::warning_text(note),
            state.preferences_error.is_some(),
        ));
    }
    content = content
        .push(container(locations_card(state)).id("settings-locations"))
        .push(container(recovery_card(state)).id("settings-recovery"))
        .push(container(maintenance_card(state)).id("settings-maintenance"))
        .push(
            container(theme::panel_card(
                section(
                    "Appearance",
                    column![
                        shell::setting_row(
                            "Theme",
                            "Use the desktop theme",
                            shell::picker(
                                [ThemeChoice::System, ThemeChoice::Dark, ThemeChoice::Light],
                                Some(state.preferences.theme),
                                Message::Theme
                            )
                        ),
                        shell::setting_row(
                            "Motion",
                            match state.preferences.motion {
                                MotionChoice::Normal => "Smooth transitions",
                                MotionChoice::Subtle => "Short transitions",
                                MotionChoice::Reduced => "No transitions",
                            },
                            shell::picker(
                                [
                                    MotionChoice::Normal,
                                    MotionChoice::Subtle,
                                    MotionChoice::Reduced
                                ],
                                Some(state.preferences.motion),
                                Message::Motion
                            )
                        ),
                    ]
                    .spacing(18),
                ),
            ))
            .id("settings-appearance"),
        )
        .push(
            container(theme::panel_card(section(
                "Compatibility reports",
                column![
                    theme::muted("Create a local report from a selected game after testing launch, gameplay, updates and restoration"),
                    theme::secondary("Choose a game", Message::GoTo(Page::Games)),
                ]
                .spacing(8),
            )))
            .id("settings-reports"),
        )
        .push(
            container(shell::about_card(
                format!("Version {} · {PLATFORM}", env!("CARGO_PKG_VERSION")),
                Message::OpenChangelog,
            ))
            .id("settings-about"),
        );
    content.into()
}

/// The card that adds a location and lists the ones added.
fn locations_card(state: &State) -> Element<'_, Message> {
    let can_add = state.preferences_loaded && !state.location_input.trim().is_empty();
    let add = if can_add {
        Some(Message::AddLocation)
    } else {
        None
    };
    let mut field = shell::input(
        match state.location_kind {
            LocationKind::Collection => LIBRARY_HINT,
            LocationKind::Game => FOLDER_HINT,
        },
        &state.location_input,
    )
    .on_input(Message::LocationInput)
    .width(Length::Fill);
    if let Some(message) = add.clone() {
        field = field.on_submit(message);
    }
    let mut form = column![
        theme::section_title("Add a location"),
        shell::picker(
            [LocationKind::Collection, LocationKind::Game],
            Some(state.location_kind),
            Message::LocationKind
        ),
        theme::muted(match state.location_kind {
            LocationKind::Collection =>
                "Each immediate subfolder appears as a game. New subfolders appear when you refresh.",
            LocationKind::Game => "Show this entire folder as one game.",
        }),
        row![
            field,
            theme::secondary_maybe(
                "Browse…",
                (!state.picker_busy).then_some(Message::BrowseFolder(true))
            ),
            theme::action_maybe("Add location", add),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    ]
    .spacing(10);
    if let Some(error) = &state.location_error {
        form = form.push(theme::danger_text(error));
    }
    let mut content = column![
        column![
            theme::section_title("Drives & libraries"),
            theme::muted("Add games from any location and choose which libraries to maintain"),
        ]
        .spacing(4),
        theme::panel_card(form),
    ]
    .spacing(theme::PAGE_GAP);
    for location in &state.preferences.locations {
        let card = column![
            row![
                column![
                    text(location.path.display().to_string()),
                    theme::muted(location.kind.to_string())
                ]
                .spacing(4)
                .width(Length::Fill),
                theme::secondary(
                    "Remove location",
                    Message::RemoveLocation(location.path.clone())
                )
            ]
            .spacing(10)
            .align_y(Alignment::Center)
        ]
        .spacing(10);
        // Maintenance is opt-in per location and needs the worker, so Windows only.
        #[cfg(windows)]
        let card = {
            let path = location.path.clone();
            card.push(
                checkbox(location.automatic)
                    .label("Maintain new installs and updates")
                    .on_toggle(move |enabled| Message::Automatic(path.clone(), enabled)),
            )
        };
        content = content.push(theme::panel_card(card));
    }
    #[cfg(windows)]
    for id in &state.preferences.excluded {
        let title = state
            .games
            .iter()
            .find(|game| game.ids().any(|other| &other.to_string() == id))
            .map(|game| {
                (
                    game.title.clone(),
                    game.ids().map(|id| id.to_string()).collect(),
                )
            })
            .unwrap_or_else(|| (id.clone(), vec![id.clone()]));
        content = content.push(theme::panel_card(
            row![
                theme::muted(format!("Excluded: {}", title.0)).width(Length::Fill),
                theme::secondary("Restore", Message::Exclude(title.1, id.clone(), false))
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        ));
    }
    content.into()
}

/// Interrupted jobs and retained originals, with the button that restores each.
fn recovery_card(state: &State) -> Element<'_, Message> {
    let mut content = column![
        column![
            theme::section_title("Recovery"),
            theme::muted(
                "Review interrupted jobs and retained storage before retrying or restoring."
            ),
        ]
        .spacing(4),
    ]
    .spacing(theme::PAGE_GAP);
    // A record whose folder has a job in progress describes that job.
    let records: Vec<_> = state
        .recovery
        .iter()
        .filter(|record| {
            #[cfg(windows)]
            {
                !rules::recovery_in_use(&state.worker.jobs, &record.root)
            }
            #[cfg(target_os = "macos")]
            {
                let _ = record;
                true
            }
        })
        .collect();
    if records.is_empty() {
        content = content.push(theme::panel_card(theme::muted(
            "No interrupted jobs need recovery",
        )));
    }
    for record in records {
        content = content.push(theme::attention_card(
            column![
                text(record.root.display().to_string()),
                theme::secondary_maybe(
                    RECOVERY_ACTION,
                    (!state.busy()).then(|| Message::Recover(record.root.clone()))
                ),
            ]
            .spacing(8),
            false,
        ));
    }
    content.into()
}

/// What keeps jobs going: the worker on Windows, the open window on macOS.
fn maintenance_card(state: &State) -> Element<'_, Message> {
    #[cfg(target_os = "macos")]
    {
        let _ = state;
        theme::panel_card(section(
            "Maintenance",
            theme::muted("Keep Flummox open until storage jobs finish."),
        ))
    }
    #[cfg(windows)]
    {
        let active = rules::active_jobs(&state.worker.jobs);
        let mut card = column![
            theme::section_title("Maintenance"),
            theme::muted("Enable maintenance separately for each location. Existing games establish a baseline; new installs and changed builds can be queued. Closing the window keeps the worker running."),
            theme::secondary_maybe(
                if state.worker.maintenance_paused { "Resume background work" } else { "Pause background work" },
                state.worker_enabled.then_some(Message::WorkerCommand(
                    coordinator::Command::Maintenance(!state.worker.maintenance_paused)
                )),
            ),
        ]
        .spacing(10);
        if !state.worker_enabled {
            card = card.push(theme::muted(
                "The worker is stopped, so there is nothing to pause.",
            ));
        }
        if state.confirm_stop_worker {
            card = card.push(theme::muted(format!(
                "Stopping the worker cancels {active} job{} and exits after the current file.",
                if active == 1 { "" } else { "s" }
            )));
            card = card.push(
                row![
                    theme::action("Stop worker", Message::StopWorker),
                    theme::secondary("Keep running", Message::KeepWorker),
                ]
                .spacing(8),
            );
        } else if state.worker_enabled {
            card = card.push(theme::secondary(
                "Stop background worker",
                Message::StopWorker,
            ));
        } else {
            card = card.push(theme::secondary(
                "Start background worker",
                Message::WorkerCommand(coordinator::Command::Snapshot),
            ));
        }
        card = card.push(
            checkbox(state.preferences.start_at_login)
                .label("Start background maintenance when I sign in")
                .on_toggle_maybe(state.preferences_loaded.then_some(Message::StartAtLogin)),
        );
        theme::panel_card(card)
    }
}

/// The Jobs page. Windows lists the worker's queue; macOS shows its one job.
fn jobs_page(state: &State) -> Element<'_, Message> {
    let mut content = column![theme::page_header::<Message>(
        "Jobs",
        Some("Track running work, waiting games, and recent results"),
        None
    )]
    .spacing(theme::PAGE_GAP);
    #[cfg(windows)]
    {
        use crate::desktop_jobs::Phase;
        if let Some(error) = &state.worker_error {
            content = content.push(theme::attention_card(
                column![
                    theme::section_text("Worker connection interrupted"),
                    theme::danger_text(error),
                    theme::muted("Showing the last received jobs. Reconnecting…")
                ]
                .spacing(6),
                true,
            ));
        }
        if let Some(busy) = &state.worker.busy {
            content = content.push(theme::panel_card(text(format!("Paused: {busy}"))));
        }
        let groups: [&[Phase]; 4] = [
            &[Phase::Running, Phase::Paused],
            &[Phase::Waiting],
            &[Phase::Failed, Phase::Interrupted],
            &[Phase::Completed, Phase::Cancelled],
        ];
        for (index, ((title, empty), phases)) in
            shell::JOB_GROUPS.into_iter().zip(groups).enumerate()
        {
            let items: Vec<_> = state
                .worker
                .jobs
                .iter()
                .filter(|job| phases.contains(&job.phase))
                .collect();
            content = content.push(theme::section_text(format!("{title} · {}", items.len())));
            if items.is_empty() {
                content = content.push(theme::muted(empty));
            }
            let history = index == 3;
            if history && items.len() > shell::HISTORY_LIMIT {
                content = content.push(theme::muted(format!(
                    "Showing the latest {} jobs",
                    shell::HISTORY_LIMIT
                )));
            }
            let limit = if history {
                shell::HISTORY_LIMIT
            } else {
                items.len()
            };
            let rows: Vec<_> = items
                .iter()
                .rev()
                .take(limit)
                .map(|job| {
                    (
                        job.id,
                        if history {
                            completed_row(job)
                        } else {
                            job_card(state, job)
                        },
                    )
                })
                .collect();
            content = content.push(iced::widget::keyed_column(rows).spacing(12));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut card = column![theme::section_text(if state.working {
            "A storage operation is running"
        } else if state.planning {
            "Checking free space…"
        } else {
            "No jobs running"
        })]
        .spacing(10);
        if let Some(progress) = &state.progress {
            card = card.push(theme::muted(format!(
                "{} files processed · {} changed · {} skipped",
                progress.files, progress.changed, progress.skipped
            )));
        }
        if state.cancel.is_some() {
            card = card.push(theme::secondary("Cancel job", Message::Stop));
        }
        content = content.push(theme::panel_card(card));
    }
    content.into()
}

/// A card for a job that is running, waiting or needs attention, with its controls.
#[cfg(windows)]
fn job_card<'a>(state: &State, job: &'a crate::desktop_jobs::Job) -> Element<'a, Message> {
    let tone = rules::job_tone(job.phase);
    let mut controls = iced::widget::Row::new().spacing(8);
    // Active jobs can be paused or cancelled. Failed, interrupted and stopped ones can
    // be retried. Completed ones have no controls.
    if job.phase.active() {
        controls = controls
            .push(theme::secondary_maybe(
                if job.user_paused { "Resume" } else { "Pause" },
                state.worker_enabled.then_some(Message::WorkerCommand(
                    coordinator::Command::Pause {
                        id: job.id,
                        paused: !job.user_paused,
                    },
                )),
            ))
            .push(theme::secondary(
                "Cancel",
                Message::WorkerCommand(coordinator::Command::Cancel(job.id)),
            ));
    } else if job.phase != crate::desktop_jobs::Phase::Completed {
        controls = controls.push(theme::secondary(
            "Retry",
            Message::WorkerCommand(coordinator::Command::Retry(job.id)),
        ));
    }
    let failed = tone == theme::Tone::Failed;
    let content = column![
        row![
            theme::section_text(&job.game.title).width(Length::Fill),
            theme::muted(format!(
                "{} · {}",
                rules::operation_words(!job.restore),
                job.phase
            ))
            .style(theme::tone_text(tone))
        ]
        .spacing(12)
        .align_y(Alignment::Center),
        if failed {
            Element::from(theme::danger_text(&job.message))
        } else {
            Element::from(theme::muted(&job.message))
        },
        row![
            theme::muted(format!(
                "{} files · {} changed · {} skipped",
                job.progress.files, job.progress.changed, job.progress.skipped
            ))
            .width(Length::Fill),
            controls.wrap()
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    ]
    .spacing(10);
    if failed {
        theme::attention_card(content, true)
    } else {
        theme::panel_card(content)
    }
}

/// A one-line card for a finished or stopped job. A stopped job can be retried.
#[cfg(windows)]
fn completed_row(job: &crate::desktop_jobs::Job) -> Element<'_, Message> {
    use crate::desktop_jobs::Phase;
    theme::panel_card(
        row![
            if job.phase == Phase::Completed {
                Element::from(theme::accent_text("✓").size(18))
            } else {
                Element::from(text("○").size(18))
            },
            column![
                text(&job.game.title).size(15),
                theme::muted(format!(
                    "{} · {} files · {} changed · {} skipped",
                    rules::operation_words(!job.restore),
                    job.progress.files,
                    job.progress.changed,
                    job.progress.skipped
                ))
            ]
            .spacing(3)
            .width(Length::Fill),
            theme::muted(job.phase.to_string()),
            if job.phase == Phase::Cancelled {
                theme::secondary(
                    "Retry",
                    Message::WorkerCommand(coordinator::Command::Retry(job.id)),
                )
            } else {
                iced::widget::Space::new().width(0).into()
            }
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
}

/// Asks the worker for a snapshot once a second. `poll` never starts a worker, so
/// a stopped one stays stopped.
#[cfg(windows)]
fn polls() -> impl iced::futures::Stream<Item = Message> {
    iced::futures::stream::unfold((), |_| async {
        let result = background(|| {
            std::thread::sleep(Duration::from_secs(1));
            coordinator::poll().map_err(|error| error.to_string())
        })
        .await;
        Some((Message::Worker(result), ()))
    })
}

/// The platform's periodic refresh: worker snapshots on Windows, a rescan on macOS.
fn poll_subscription(state: &State) -> iced::Subscription<Message> {
    #[cfg(windows)]
    if !state.worker_enabled {
        return iced::Subscription::none();
    }
    #[cfg(target_os = "macos")]
    let _state = state;
    iced::Subscription::run(polls)
}

/// One line for the Overview page describing the first active job, if any.
fn current_work(state: &State) -> String {
    #[cfg(windows)]
    {
        state
            .worker
            .jobs
            .iter()
            .find(|job| job.phase.active())
            .map(|job| format!("{} · {} · {}", job.game.title, job.phase, job.message))
            .unwrap_or_else(|| "No jobs running".into())
    }
    #[cfg(target_os = "macos")]
    {
        if state.working {
            "A storage operation is running".into()
        } else if state.planning {
            "Checking free space…".into()
        } else {
            "No jobs running".into()
        }
    }
}

#[cfg(test)]
#[path = "native_preview.rs"]
mod preview;
