//! Native Mac and Windows desktop workflows over their filesystem backends.

use anyhow::Result;
#[cfg(target_os = "macos")]
use iced::futures::SinkExt;
use iced::widget::{
    Space, button, column, container, image, pick_list, responsive, row, scrollable, text,
    text_input,
};
use iced::{Animation, Element, Length, Task, Theme, animation::Easing};
use std::time::{Duration, Instant};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::theme;
use crate::desktop::{Location, LocationKind, MotionChoice, Preferences, ThemeChoice};
#[cfg(target_os = "macos")]
use crate::macos as backend;
#[cfg(windows)]
use crate::windows as backend;
#[cfg(windows)]
use iced::widget::checkbox;
use std::collections::HashMap;
#[cfg(windows)]
const PLATFORM: &str = "Windows · WOF/LZX";
#[cfg(target_os = "macos")]
const PLATFORM: &str = "macOS · APFS";
#[cfg(windows)]
const MODE: &str = "LZX";
#[cfg(windows)]
const FOLDER_HINT: &str = "C:\\Games\\Your game";
#[cfg(target_os = "macos")]
const FOLDER_HINT: &str = "~/My Games/Your game";
#[cfg(target_os = "macos")]
const MODE: &str = "APFS";
#[cfg(windows)]
const RECOVERY_ACTION: &str = "Restore ordinary storage";
#[cfg(target_os = "macos")]
const RECOVERY_ACTION: &str = "Restore retained original";

/// The one banner line under the controls. `error` picks the banner style.
struct Status {
    error: bool,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Games,
    Settings,
}
impl Page {
    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Games => "Games",
            Self::Settings => "Settings",
        }
    }
    /// Position in the navigation column. Decides the direction of the page transition.
    fn rank(self) -> u8 {
        match self {
            Self::Overview => 0,
            Self::Games => 1,
            Self::Settings => 2,
        }
    }
}
/// Result of one background discovery pass, with artwork located per game id.
#[derive(Debug, Clone)]
struct Scan {
    /// Worker (epoch, revision) the games were read at. Compared with later snapshots.
    #[cfg(windows)]
    stamp: (u64, u64),
    games: Vec<crate::model::Game>,
    warnings: Vec<String>,
    /// Image for the 44 px list tile, by game id.
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
        let snapshot =
            crate::windows::coordinator::request(crate::windows::coordinator::Command::Snapshot)
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
    /// Install state is anything but idle.
    Attention,
}
impl std::fmt::Display for GameFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::All => "All games",
            Self::Updated => "Updated",
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
            Self::Title => "Title",
            Self::Launcher => "Launcher",
        })
    }
}
struct State {
    /// Last snapshot accepted from the worker.
    #[cfg(windows)]
    worker: crate::windows::coordinator::Snapshot,
    /// Set while polls fail. The previous snapshot stays on screen.
    #[cfg(windows)]
    worker_error: Option<String>,
    /// False once the user stops the worker or a snapshot reports it stopping.
    /// Polling stops while it is false.
    #[cfg(windows)]
    worker_enabled: bool,
    preferences: Preferences,
    /// Saving is refused until the load succeeds, so defaults never replace the file.
    preferences_loaded: bool,
    /// A save is in flight.
    saving_preferences: bool,
    /// Preferences changed during a save. Another save follows it.
    preferences_dirty: bool,
    /// Rescan when the pending save lands. Set when locations change.
    refresh_after_save: bool,
    location_kind: LocationKind,
    location_input: String,
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
    scroll_positions: std::collections::HashMap<&'static str, f32>,
    games: Vec<crate::model::Game>,
    /// Text of the selected-folder field. Every action targets this path.
    folder: String,
    status: Option<Status>,
    /// A space plan is being computed or, on macOS, a storage job is running.
    working: bool,
    scanning: bool,
    progress: Option<backend::Progress>,
    /// Stop flag shared with an in-process job thread. Only the macOS path sets it.
    cancel: Option<Arc<AtomicBool>>,
    system_theme: iced::theme::Mode,
    /// Folder, whether it is a compression, and its space plan, awaiting confirmation.
    planned: Option<(PathBuf, bool, crate::storage::SpacePlan)>,
    /// The scan in flight came from the timer, so its result posts no status line.
    refreshing: bool,
    recovery: Vec<backend::Recovery>,
    qualification: Option<crate::qualification::Wizard>,
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
            preferences: Preferences::default(),
            preferences_loaded: false,
            saving_preferences: false,
            preferences_dirty: false,
            refresh_after_save: false,
            location_kind: LocationKind::Game,
            location_input: String::new(),
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
            status: None,
            working: false,
            scanning: true,
            progress: None,
            cancel: None,
            system_theme: iced::theme::Mode::Dark,
            planned: None,
            refreshing: false,
            recovery: vec![],
            qualification: None,
        }
    }
}

#[derive(Debug, Clone)]
enum Message {
    #[cfg(windows)]
    Worker(std::result::Result<crate::windows::coordinator::Snapshot, String>),
    #[cfg(windows)]
    WorkerCommand(crate::windows::coordinator::Command),
    /// Turn maintenance on or off for the location at this path.
    #[cfg(windows)]
    Automatic(PathBuf, bool),
    #[cfg(windows)]
    StartAtLogin(bool),
    /// A game id, and whether it is now excluded from background work.
    #[cfg(windows)]
    Exclude(String, bool),
    GoTo(Page),
    /// Open Settings and scroll to the container with this id.
    Jump(&'static str),
    /// The offset `Jump` measured for its section.
    JumpOffset(f32),
    PreferencesLoaded(std::result::Result<Preferences, String>),
    PreferencesSaved(std::result::Result<(), String>),
    Theme(ThemeChoice),
    Motion(MotionChoice),
    Query(String),
    Filter(GameFilter),
    Sort(GameSort),
    LocationInput(String),
    LocationKind(LocationKind),
    AddLocation,
    LocationResolved(LocationKind, std::result::Result<PathBuf, String>),
    RemoveLocation(PathBuf),
    /// Open the folder picker. `true` fills the location field, `false` the selected folder.
    BrowseFolder(bool),
    /// The picker closed. `Ok(None)` means the user cancelled.
    FolderPicked(bool, std::result::Result<Option<PathBuf>, String>),
    /// A tile scrolled into view and wants its image decoded.
    ArtworkVisible(super::artwork::Source),
    ArtworkLoaded(super::artwork::Source, Option<image::Handle>),
    BrowseArtwork(String),
    ArtworkPicked(String, std::result::Result<Option<PathBuf>, String>),
    ArtworkSaved(std::result::Result<(), String>),
    Scrolled(Page, f32),
    /// Carries nothing. Its arrival makes iced redraw.
    Tick,
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
    /// The user confirmed the plan on screen.
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
    let (send, receive) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _sent = send.send(operation());
    });
    receive
        .await
        .map_err(|_| "The background operation stopped unexpectedly.".to_owned())?
}

/// Computes the space plan for the selected folder. Nothing on disk changes until
/// the user confirms it with `StartPlanned`.
fn plan(state: &mut State, optimize: bool) -> Task<Message> {
    if state.working {
        return Task::none();
    }
    let folder = crate::native::folder_path(&state.folder);
    state.working = true;
    state.planned = None;
    let planned_folder = folder.clone();
    Task::perform(
        background(move || {
            crate::storage::native_plan(&folder, !optimize).map_err(|error| error.to_string())
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
        state.status = Some(Status {
            error: true,
            text: "Choose an installed game folder first.".into(),
        });
        return Task::none();
    }
    if !folder.is_dir() {
        state.status = Some(Status {
            error: true,
            text: "Choose an existing installed game folder.".into(),
        });
        return Task::none();
    }
    state.working = true;
    state.progress = None;
    let cancel = Arc::new(AtomicBool::new(false));
    state.cancel = Some(cancel.clone());
    state.status = Some(Status {
        error: false,
        text: if optimize {
            format!("Compressing worthwhile files with {MODE}…")
        } else {
            "Restoring ordinary storage…".into()
        },
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

/// Applies one message. Arms that start their own task return early. The rest fall
/// through to `artwork_tasks`, which starts any image decodes the view asked for.
fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        #[cfg(windows)]
        Message::Exclude(id, excluded) => {
            // Remove first so an id is never listed twice.
            state.preferences.excluded.retain(|old| old != &id);
            if excluded {
                state.preferences.excluded.push(id);
            }
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
            state.worker_enabled =
                !matches!(command, crate::windows::coordinator::Command::Shutdown);
            return worker_send(command);
        }
        #[cfg(windows)]
        Message::Worker(result) => match result {
            Ok(snapshot) => {
                // Replies can arrive out of order. Drop any that is not newer than the
                // one on screen: epoch is the worker's start time, revision counts its
                // replies.
                if state.worker.epoch > 0
                    && (snapshot.epoch < state.worker.epoch
                        || (snapshot.epoch == state.worker.epoch
                            && snapshot.revision <= state.worker.revision))
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
                // Mirror the running or paused job's counters into the progress panel.
                if let Some(job) = snapshot.jobs.iter().find(|job| {
                    job.phase.active() && job.phase != crate::desktop_jobs::Phase::Waiting
                }) {
                    state.progress = Some(backend::Progress {
                        files: job.progress.files,
                        changed: job.progress.changed,
                        skipped: job.progress.skipped,
                        bytes: job.progress.bytes,
                        allocation_before: job.progress.allocation_before,
                        allocation_after: job.progress.allocation_after,
                    });
                }
                state.preferences.maintenance_paused = snapshot.maintenance_paused;
                state.worker_enabled = !snapshot.stopping;
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
                    tasks.push(Task::perform(background(scan), Message::Scanned));
                }
                return Task::batch(tasks);
            }
            Err(error) => state.worker_error = Some(error),
        },
        Message::PreferencesLoaded(result) => {
            state.preferences_loaded = result.is_ok();
            match result {
                Ok(preferences) => state.preferences = preferences,
                Err(text) => state.status = Some(Status { error: true, text }),
            }
        }
        Message::PreferencesSaved(result) => {
            state.saving_preferences = false;
            if let Err(text) = result {
                state.status = Some(Status { error: true, text });
            } else if state.preferences_dirty {
                // Preferences changed while this save ran. Write the newer copy first.
                return save_preferences(state);
            } else if state.refresh_after_save {
                state.refresh_after_save = false;
                return update(state, Message::Refresh);
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
        Message::LocationInput(input) => state.location_input = input,
        Message::LocationKind(kind) => state.location_kind = kind,
        Message::AddLocation => {
            let path = crate::native::folder_path(&state.location_input);
            let kind = state.location_kind;
            return resolve_location(path, kind);
        }
        Message::LocationResolved(kind, result) => match result {
            Ok(path) => {
                // A path that is already listed only has its kind changed.
                if let Some(old) = state
                    .preferences
                    .locations
                    .iter_mut()
                    .find(|location| location.path == path)
                {
                    old.kind = kind;
                } else {
                    state.preferences.locations.push(Location {
                        path,
                        kind,
                        automatic: false,
                    });
                }
                state.refresh_after_save = true;
                return save_preferences(state);
            }
            Err(text) => state.status = Some(Status { error: true, text }),
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
                        } else {
                            state.folder = text.into();
                            state.planned = None;
                        }
                    } else {
                        state.status = Some(Status {
                            error: true,
                            text: "This folder cannot be displayed in the path field.".into(),
                        });
                    }
                }
                Ok(None) => {}
                Err(text) => state.status = Some(Status { error: true, text }),
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
                Err(text) => state.status = Some(Status { error: true, text }),
            }
        }
        Message::ArtworkSaved(result) => match result {
            Ok(()) => return update(state, Message::Refresh),
            Err(text) => state.status = Some(Status { error: true, text }),
        },
        Message::Jump(section) => {
            // GoTo's own task restores the page's old scroll offset. It is dropped
            // because the jump scrolls to the section.
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
        Message::GoTo(page) => {
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
                return iced::widget::operation::scroll_to(
                    page.label(),
                    iced::widget::operation::AbsoluteOffset {
                        x: None,
                        y: Some(
                            state
                                .scroll_positions
                                .get(page.label())
                                .copied()
                                .unwrap_or_default(),
                        ),
                    },
                )
                .chain(Task::done(Message::Tick));
            }
        }
        Message::Scrolled(page, offset) => {
            state.scroll_positions.insert(page.label(), offset);
        }
        Message::Tick => {}

        // The compatibility wizard. Its state lives in `state.qualification`, and
        // these arms forward to it while it is open.
        Message::Qualify => {
            let path = crate::native::folder_path(&state.folder);
            if let Some(game) = state
                .games
                .iter()
                .find(|game| game.install_dir == path)
                .cloned()
            {
                return Task::perform(
                    background(move || {
                        crate::qualification::Wizard::start(game)
                            .map(Box::new)
                            .map_err(|error| error.to_string())
                    }),
                    Message::QualificationReady,
                );
            }
            state.status = Some(Status {
                error: true,
                text: "Remember or select the game folder first.".into(),
            });
        }
        Message::QualificationReady(result) => match result {
            Ok(wizard) => state.qualification = Some(*wizard),
            Err(error) => {
                state.status = Some(Status {
                    error: true,
                    text: error,
                })
            }
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
        Message::CloseQualification => state.qualification = None,
        Message::OpenChangelog => {
            if let Err(error) = super::open_changelog() {
                state.status = Some(Status {
                    error: true,
                    text: format!(
                        "Could not open a browser ({error}). The changelog is at {}",
                        super::CHANGELOG_URL
                    ),
                });
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
                    Err(error) => {
                        state.status = Some(Status {
                            error: true,
                            text: error.to_string(),
                        })
                    }
                }
            }
        }
        Message::QualificationSaved(result) => {
            state.qualification = None;
            state.status = Some(match result {
                Ok(text) => Status { error: false, text },
                Err(text) => Status { error: true, text },
            });
        }
        Message::Folder(folder) => {
            state.folder = folder;
            state.planned = None;
        }
        Message::Select(folder) => {
            let navigation = update(state, Message::GoTo(Page::Games));
            state.folder = folder.display().to_string();
            state.planned = None;
            state.status = None;
            return navigation;
        }
        // Tab and Shift+Tab move focus. Escape closes the plan and the wizard.
        Message::Key(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            if key == iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab) {
                return if modifiers.shift() {
                    iced::widget::operation::focus_previous()
                } else {
                    iced::widget::operation::focus_next()
                };
            }
            if key == iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape) {
                state.planned = None;
                state.qualification = None;
            }
        }
        Message::Key(_) => {}
        #[cfg(target_os = "macos")]
        Message::Poll => {
            // The timer rescan waits while a job, a scan, a pending plan or the
            // wizard is using the current game list.
            if !state.working
                && !state.scanning
                && state.planned.is_none()
                && state.qualification.is_none()
            {
                state.refreshing = true;
                return update(state, Message::Refresh);
            }
        }
        Message::Refresh => {
            if state.scanning || state.working {
                return Task::none();
            }
            state.scanning = true;
            let scan = Task::perform(background(scan), Message::Scanned);
            // The worker keeps its own game list, so ask it to rediscover as well.
            #[cfg(windows)]
            return Task::batch([
                scan,
                worker_send(crate::windows::coordinator::Command::Refresh),
            ]);
            #[cfg(target_os = "macos")]
            return scan;
        }
        Message::Scanned(games) => {
            state.scanning = false;
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
                    state.status = Some(Status {
                        error: true,
                        text: error,
                    });
                    return Task::none();
                }
            }
            if !state.refreshing {
                state.status = Some(Status {
                    error: false,
                    text: format!("Found {} remembered games.", state.games.len()),
                });
            }
            state.refreshing = false;
        }
        Message::Optimize => return plan(state, true),
        Message::Restore => return plan(state, false),
        Message::Planned(folder, optimize, result) => {
            state.working = false;
            match result {
                Ok(plan) => state.planned = Some((folder, optimize, plan)),
                Err(error) => {
                    state.status = Some(Status {
                        error: true,
                        text: error,
                    })
                }
            }
        }
        Message::StartPlanned => {
            if let Some((folder, optimize, plan)) = state.planned.take() {
                // Free space and the mounted volume may have changed since the plan
                // was shown.
                if let Err(error) = plan.recheck() {
                    state.status = Some(Status {
                        error: true,
                        text: error.to_string(),
                    });
                } else {
                    return start(state, folder, optimize);
                }
            }
        }
        Message::CancelPlanned => state.planned = None,
        Message::Remember => {
            return resolve_location(
                crate::native::folder_path(&state.folder),
                LocationKind::Game,
            );
        }
        // On Windows recovery is an ordinary restore job queued with the worker.
        #[cfg(windows)]
        Message::Recover(folder) => return start(state, folder, false),
        #[cfg(target_os = "macos")]
        Message::Recover(folder) => {
            state.working = true;
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
            Err(error) => {
                state.status = Some(Status {
                    error: true,
                    text: error,
                })
            }
        },
        Message::Stop => {
            // Windows cancels the worker's running or paused job. macOS raises the
            // flag its job thread checks before each file.
            #[cfg(windows)]
            if let Some(job) =
                state.worker.jobs.iter().find(|job| {
                    job.phase.active() && job.phase != crate::desktop_jobs::Phase::Waiting
                })
            {
                return worker_send(crate::windows::coordinator::Command::Cancel(job.id));
            }
            if let Some(cancel) = &state.cancel {
                cancel.store(true, Ordering::Relaxed);
                state.status = Some(Status {
                    error: false,
                    text: "Stopping after the current file…".into(),
                });
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
            state.status = Some(match (stopped, result) {
                (true, _) => Status {
                    error: false,
                    text: "Stopped. Files already processed remain valid.".into(),
                },
                (false, Ok(text)) => Status { error: false, text },
                (false, Err(text)) => Status { error: true, text },
            });
            return Task::perform(
                background(|| backend::recovery().map_err(|error| error.to_string())),
                Message::RecoveryScanned,
            );
        }
        Message::SystemTheme(theme) => state.system_theme = theme,
    }
    artwork_tasks(state)
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
                    GameFilter::Attention => !game.state.is_idle(),
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
/// Lays the window out, compact under 760 px wide. The second argument keeps frames
/// coming while a page transition or a programmatic scroll is in progress.
fn view(state: &State) -> Element<'_, Message> {
    super::surface::animate(
        responsive(move |size| layout(state, size.width < 760.0)),
        (state.preferences.motion != MotionChoice::Reduced
            && state.reveal.is_animating(Instant::now()))
            || state
                .scroll_redraw_until
                .is_some_and(|until| Instant::now() < until),
    )
}

/// Builds all three pages' parts and shows the current one beside the navigation
/// column. `compact` stacks the Games page's two panels and narrows the navigation.
fn layout(state: &State, compact: bool) -> Element<'_, Message> {
    let hero = container(
        column![
            text(format!("Save space with {MODE}")).size(28),
            theme::muted("Games stay in place and launch normally"),
            row![
                theme::stat(state.games.len().to_string(), "Games"),
                theme::stat(MODE.into(), "Mode")
            ]
            .spacing(32),
        ]
        .spacing(14),
    )
    .padding(22)
    .width(Length::Fill)
    .style(theme::hero);

    // Games page, first panel: refresh, search, filter, sort, then one button per game.
    let mut game_list = column![
        row![
            text(format!("Library · {} games", state.games.len())).size(16),
            button(if state.scanning {
                "Refreshing…"
            } else {
                "Refresh"
            })
            .padding([6, 10])
            .on_press_maybe((!state.working && !state.scanning).then_some(Message::Refresh)),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center)
    ]
    .spacing(8);
    game_list = game_list.push(
        text_input("Search your games", &state.query)
            .on_input(Message::Query)
            .padding(10),
    );
    game_list = game_list.push(
        row![
            pick_list(
                [GameFilter::All, GameFilter::Updated, GameFilter::Attention],
                Some(state.game_filter),
                Message::Filter
            ),
            pick_list(
                [GameSort::Title, GameSort::Launcher],
                Some(state.game_sort),
                Message::Sort
            ),
        ]
        .spacing(8),
    );
    let filtered = filtered_games(state);
    if filtered.is_empty() {
        game_list = game_list.push(theme::muted(if state.games.is_empty() {
            "No games found. Add a game or games library in Settings"
        } else {
            "No games match this search and filter"
        }));
    }
    for game in filtered {
        game_list = game_list.push(
            button(
                row![
                    artwork_tile(state, game, false),
                    column![
                        text(&game.title).size(14),
                        text(game.install_dir.display().to_string()).size(11),
                        text(game.state.to_string()).size(11),
                    ]
                    .spacing(2)
                ]
                .spacing(10)
                .align_y(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .padding([9, 11])
            // A game that is busy, updating or unavailable cannot be selected.
            .on_press_maybe(
                (!state.working && !state.scanning && game.state.is_idle())
                    .then(|| Message::Select(game.install_dir.clone())),
            ),
        );
    }
    let library = container(game_list)
        .padding(16)
        .width(if compact {
            Length::Fill
        } else {
            Length::FillPortion(5)
        })
        .style(theme::panel);

    // Games page, second panel: the selected folder and everything that acts on it.
    let controls = row![
        button("Optimize")
            .padding([11, 18])
            .style(theme::action_button)
            .on_press_maybe((!state.working).then_some(Message::Optimize)),
        button("Restore")
            .padding([11, 18])
            .style(theme::secondary_button)
            .on_press_maybe((!state.working).then_some(Message::Restore)),
        button("Stop")
            .padding([11, 18])
            .style(theme::secondary_button)
            .on_press_maybe(can_stop(state).then_some(Message::Stop)),
    ]
    .spacing(10);
    let mut action = column![
        theme::section_title("Selected folder"),
        text_input(FOLDER_HINT, &state.folder)
            .on_input_maybe((!state.working).then_some(Message::Folder))
            .padding(12)
            .width(Length::Fill),
        button("Browse…").on_press_maybe(
            (!state.picker_busy && !state.working).then_some(Message::BrowseFolder(false))
        ),
        theme::muted("Optimize skips files the filesystem cannot shrink"),
        controls,
        button("Qualify compatibility")
            .on_press_maybe((!state.working).then_some(Message::Qualify)),
        button("Remember this folder").on_press_maybe(
            (!state.working && state.preferences_loaded).then_some(Message::Remember)
        ),
    ]
    .spacing(14);
    // Exclusion and artwork controls need a game id, so they appear only when the
    // folder text resolves to a known game.
    if let Some(game) = state
        .games
        .iter()
        .find(|game| game.install_dir == crate::native::folder_path(&state.folder))
    {
        #[cfg(windows)]
        {
            let excluded = game
                .ids()
                .any(|id| state.preferences.excluded.contains(&id.to_string()));
            action = action.push(
                button(if excluded {
                    "Include in background work"
                } else {
                    "Exclude from background work"
                })
                .on_press_maybe(
                    state
                        .preferences_loaded
                        .then(|| Message::Exclude(game.id.to_string(), !excluded)),
                ),
            );
        }
        action = action.push(artwork_tile(state, game, true));
        action = action.push(button("Choose local artwork…").on_press_maybe(
            (!state.picker_busy).then(|| Message::BrowseArtwork(game.id.to_string())),
        ));
    }
    if let Some(wizard) = &state.qualification {
        action = action.push(crate::qualification::view(
            wizard,
            Message::QualificationField,
            Message::QualificationCheck,
            Message::QualificationMode,
            Message::MeasureQualification,
            Message::SaveQualification,
            Message::CloseQualification,
        ));
    }
    // The confirmation step: one line per volume, and Start only if the plan fits.
    if let Some((_, _, plan)) = &state.planned {
        action = action.push(theme::section_title("Storage plan"));
        for row in &plan.requirements {
            action = action.push(
                text(format!(
                    "{}: {} needed including headroom; {} available",
                    row.volume.path.display(),
                    humansize::format_size(
                        row.additional.saturating_add(row.headroom),
                        humansize::DECIMAL
                    ),
                    humansize::format_size(row.volume.available, humansize::DECIMAL)
                ))
                .size(13),
            );
        }
        if let Err(error) = plan.check() {
            action = action.push(text(error.to_string()).size(13));
        }
        action = action.push(
            row![
                button("Start job")
                    .on_press_maybe(plan.check().is_ok().then_some(Message::StartPlanned)),
                button("Cancel").on_press(Message::CancelPlanned)
            ]
            .spacing(10),
        );
    }
    // Allocation is summed over the files visited so far, so `freed` grows with the job.
    if let Some(progress) = &state.progress {
        let freed = progress
            .allocation_before
            .saturating_sub(progress.allocation_after);
        action = action.push(
            container(
                column![
                    text(format!(
                        "{} files processed · {} changed · {} skipped",
                        progress.files, progress.changed, progress.skipped
                    ))
                    .size(13),
                    theme::muted(format!(
                        "{} scanned · {} freed so far",
                        humansize::format_size(progress.bytes, humansize::DECIMAL),
                        humansize::format_size(freed, humansize::DECIMAL)
                    )),
                ]
                .spacing(4),
            )
            .padding(12)
            .width(Length::Fill)
            .style(theme::hero),
        );
    }
    if let Some(status) = &state.status {
        action = action.push(
            container(text(&status.text).size(13))
                .padding(12)
                .width(Length::Fill)
                .style(theme::banner(status.error)),
        );
    }
    let page = state.page;
    let content: Element<'_, Message> = match page {
        Page::Overview => column![
            theme::page_title("Overview"),
            hero,
            theme::section_title("Running now"),
            theme::muted(current_work(state)),
            button("View jobs and recovery").on_press(Message::GoTo(Page::Settings)),
            button("Choose games").on_press(Message::GoTo(Page::Games)),
        ]
        .spacing(16)
        .into(),
        Page::Games => {
            if compact {
                column![theme::page_title("Games"), library, action]
                    .spacing(16)
                    .into()
            } else {
                column![
                    theme::page_title("Games"),
                    row![library, action].spacing(16)
                ]
                .spacing(16)
                .into()
            }
        }
        Page::Settings => settings_page(state),
    };
    // The page starts offset by the motion distance and settles to zero as `reveal`
    // runs from 0 to 1. Reduced motion pins it at 1.
    let reveal = if state.preferences.motion == MotionChoice::Reduced {
        1.0
    } else {
        state.reveal.interpolate(0.0, 1.0, Instant::now())
    };
    let body = super::surface::surface(
        scrollable(container(content).padding(24).width(Length::Fill))
            // The scrollable's id is the page label. `scroll_to` in `update` targets it.
            .id(page.label())
            .on_scroll(move |viewport| Message::Scrolled(page, viewport.absolute_offset().y))
            .height(Length::Fill),
        state.direction * state.preferences.motion.distance() * (1.0 - reveal),
        state.preferences.motion != MotionChoice::Reduced,
        page.label(),
    );
    let mut navigation = column![text("Flummox").size(23), Space::new().height(16)]
        .spacing(10)
        .padding(16);
    for destination in [Page::Overview, Page::Games, Page::Settings] {
        navigation = navigation.push(
            button(destination.label())
                .style(if destination == page {
                    theme::action_button
                } else {
                    theme::secondary_button
                })
                .width(Length::Fill)
                .padding(12)
                .on_press(Message::GoTo(destination)),
        );
    }
    container(
        row![
            container(navigation).width(if compact { 130 } else { 190 }),
            body
        ]
        .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::app_background)
    .into()
}

fn theme(state: &State) -> Theme {
    theme::theme(match state.preferences.theme {
        ThemeChoice::System => state.system_theme != iced::theme::Mode::Light,
        ThemeChoice::Dark => true,
        ThemeChoice::Light => false,
    })
}

/// Starts the first scan, the preference load and the recovery check side by side.
fn boot() -> (State, Task<Message>) {
    let state = State::default();
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
                if (state.preferences.motion != MotionChoice::Reduced
                    && state.reveal.is_animating(Instant::now()))
                    || state
                        .scroll_redraw_until
                        .is_some_and(|until| Instant::now() < until)
                {
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
        .window_size((900.0, 620.0))
        .run()?;
    Ok(())
}

/// Canonicalises a typed path off the UI thread. Rejects anything that is not an
/// existing directory with a parent, which rules out a drive or filesystem root.
fn resolve_location(path: PathBuf, kind: LocationKind) -> Task<Message> {
    Task::perform(
        background(move || {
            let path = path.canonicalize().map_err(|error| error.to_string())?;
            if !path.is_dir() || path.parent().is_none() {
                return Err("Choose an existing game or games library".into());
            }
            Ok(path)
        }),
        move |result| Message::LocationResolved(kind, result),
    )
}
/// Writes the preferences, one save at a time. On Windows the worker writes the file
/// and applies the change in the same step. On macOS this process writes it.
fn save_preferences(state: &mut State) -> Task<Message> {
    if !state.preferences_loaded {
        return Task::none();
    }
    // A save is running. `PreferencesSaved` starts the next one when it returns.
    if state.saving_preferences {
        state.preferences_dirty = true;
        return Task::none();
    }
    state.saving_preferences = true;
    state.preferences_dirty = false;
    let preferences = state.preferences.clone();
    Task::perform(
        background(move || {
            #[cfg(target_os = "macos")]
            let result = crate::libraries::data_dir().and_then(|root| preferences.save(&root));
            #[cfg(windows)]
            let result = crate::windows::coordinator::request(
                crate::windows::coordinator::Command::Settings(preferences),
            )
            .map(|_| ());
            result.map_err(|error| error.to_string())
        }),
        Message::PreferencesSaved,
    )
}
/// Starts a decode for every image the cache has queued. A failed decode reports
/// `None`, and the tile keeps its letter.
fn artwork_tasks(state: &mut State) -> Task<Message> {
    let mut tasks = vec![];
    while let Some(source) = state.artwork_cache.next() {
        let key = source.clone();
        tasks.push(Task::perform(
            background(move || Ok(source.decode().ok())),
            move |result| Message::ArtworkLoaded(key.clone(), result.ok().flatten()),
        ));
    }
    Task::batch(tasks)
}
/// A game's image, or the first letter of its title until one is loaded. `cover`
/// picks the tall 128x192 cover over the 44 px list tile. The sensor asks for the
/// decode only when the tile is shown.
fn artwork_tile<'a>(
    state: &'a State,
    game: &'a crate::model::Game,
    cover: bool,
) -> Element<'a, Message> {
    let (width, height) = if cover { (128, 192) } else { (44, 44) };
    let source = if cover {
        state.covers.get(&game.id.to_string())
    } else {
        state.artwork.get(&game.id.to_string())
    };
    let tile: Element<'a, Message> = source
        .and_then(|source| state.artwork_cache.get(source))
        .map(|handle| {
            Element::from(
                image(handle.clone())
                    .width(width)
                    .height(height)
                    .content_fit(iced::ContentFit::Contain),
            )
        })
        .unwrap_or_else(|| {
            container(text(game.title.chars().next().unwrap_or('F').to_string()).size(20))
                .center_x(width)
                .center_y(height)
                .into()
        });
    match source {
        Some(source) => {
            let source = source.clone();
            iced::widget::sensor(tile)
                .key(source.clone())
                .on_show(move |_| Message::ArtworkVisible(source.clone()))
                .into()
        }
        None => tile,
    }
}
/// The Settings page: jobs, locations, recovery, maintenance, appearance, reports
/// and about, then any discovery warnings.
fn settings_page(state: &State) -> Element<'_, Message> {
    // Each id here must match the `.id(...)` of a container in `content` below.
    let jumps = iced::widget::Row::with_children(
        [
            ("Jobs", "settings-jobs"),
            ("Locations", "settings-locations"),
            ("Recovery", "settings-recovery"),
            ("Maintenance", "settings-maintenance"),
            ("Appearance", "settings-appearance"),
            ("Reports", "settings-reports"),
            ("About", "settings-about"),
        ]
        .into_iter()
        .map(|(label, section)| {
            button(label)
                .style(theme::secondary_button)
                .on_press(Message::Jump(section))
                .into()
        }),
    )
    .spacing(8)
    .wrap();
    // macOS shows the one in-process job. Windows lists the worker's queue.
    #[cfg(target_os = "macos")]
    let jobs = {
        let mut jobs = column![
            theme::section_title("Jobs"),
            theme::muted(if state.working {
                "A storage operation is running"
            } else {
                "No jobs running"
            })
        ]
        .spacing(12);
        if let Some(progress) = &state.progress {
            jobs = jobs.push(text(format!(
                "{} files processed · {} changed · {} skipped",
                progress.files, progress.changed, progress.skipped
            )));
        }
        if state.cancel.is_some() {
            jobs = jobs.push(button("Cancel job").on_press(Message::Stop));
        }
        if let Some(status) = &state.status {
            jobs = jobs.push(
                container(text(&status.text))
                    .padding(12)
                    .style(theme::banner(status.error)),
            );
        }
        jobs
    };
    #[cfg(windows)]
    let jobs = worker_jobs(state);
    let mut locations = column![
        theme::section_title("Locations"),
        theme::muted("Add one game, or a library whose immediate subfolders are games"),
        pick_list(
            [LocationKind::Game, LocationKind::Collection],
            Some(state.location_kind),
            Message::LocationKind
        ),
        text_input(FOLDER_HINT, &state.location_input)
            .on_input(Message::LocationInput)
            .padding(12),
        row![
            button("Browse…")
                .on_press_maybe((!state.picker_busy).then_some(Message::BrowseFolder(true))),
            button("Add location")
                .on_press_maybe(state.preferences_loaded.then_some(Message::AddLocation))
        ]
        .spacing(10)
    ]
    .spacing(12);
    for location in &state.preferences.locations {
        // Maintenance is opt-in per location and needs the worker, so Windows only.
        #[cfg(windows)]
        {
            let path = location.path.clone();
            locations = locations.push(
                checkbox(location.automatic)
                    .label(format!(
                        "Maintain new installs and updates in {}",
                        location.path.display()
                    ))
                    .on_toggle(move |enabled| Message::Automatic(path.clone(), enabled)),
            );
        }
        locations = locations.push(
            container(
                row![
                    column![
                        text(location.path.display().to_string()),
                        theme::muted(location.kind.to_string())
                    ]
                    .spacing(4)
                    .width(Length::Fill),
                    button("Remove location")
                        .on_press(Message::RemoveLocation(location.path.clone()))
                ]
                .spacing(12),
            )
            .padding(12)
            .style(theme::panel),
        );
    }
    let mut recovery = column![theme::section_title("Recovery")].spacing(12);
    if state.recovery.is_empty() {
        recovery = recovery.push(theme::muted("No interrupted jobs need recovery"));
    }
    for record in &state.recovery {
        recovery = recovery.push(text(record.root.display().to_string())).push(
            button(RECOVERY_ACTION)
                .on_press_maybe((!state.working).then(|| Message::Recover(record.root.clone()))),
        );
    }
    #[cfg(target_os = "macos")]
    let maintenance = column![
        theme::section_title("Maintenance"),
        theme::muted("Keep Flummox open until storage jobs finish.")
    ]
    .spacing(8);
    #[cfg(windows)]
    let maintenance = column![theme::section_title("Maintenance"), theme::muted("Enable maintenance separately for each location. Existing games establish a baseline; new installs and changed builds can be queued. Closing the window keeps the worker running."), button(if state.worker.maintenance_paused { "Resume background work" } else { "Pause background work" }).on_press(Message::WorkerCommand(crate::windows::coordinator::Command::Maintenance(!state.worker.maintenance_paused))), button(if state.worker_enabled { "Stop background worker" } else { "Start background worker" }).on_press(Message::WorkerCommand(if state.worker_enabled { crate::windows::coordinator::Command::Shutdown } else { crate::windows::coordinator::Command::Snapshot }))].spacing(12);
    #[cfg(windows)]
    let maintenance = maintenance.push(
        checkbox(state.preferences.start_at_login)
            .label("Start background maintenance when I sign in")
            .on_toggle_maybe(state.preferences_loaded.then_some(Message::StartAtLogin)),
    );
    let appearance = column![
        theme::section_title("Appearance"),
        row![
            text("Theme").width(Length::Fill),
            pick_list(
                [ThemeChoice::System, ThemeChoice::Dark, ThemeChoice::Light],
                Some(state.preferences.theme),
                Message::Theme
            )
        ]
        .spacing(12),
        row![
            text("Motion").width(Length::Fill),
            pick_list(
                [
                    MotionChoice::Normal,
                    MotionChoice::Subtle,
                    MotionChoice::Reduced
                ],
                Some(state.preferences.motion),
                Message::Motion
            )
        ]
        .spacing(12)
    ]
    .spacing(12);
    let reports = column![theme::section_title("Compatibility reports"), theme::muted("Create a local report from a selected game after testing launch, gameplay, updates and restoration"), button("Choose a game").on_press(Message::GoTo(Page::Games))].spacing(8);
    let mut content = column![
        theme::page_title("Settings"),
        jumps,
        container(jobs).id("settings-jobs"),
        container(locations).id("settings-locations"),
        container(recovery).id("settings-recovery"),
        container(maintenance).id("settings-maintenance"),
        container(appearance).id("settings-appearance"),
        container(reports).id("settings-reports"),
        container(
            column![
                theme::section_title("About"),
                theme::muted(format!("{PLATFORM} · {}", env!("CARGO_PKG_VERSION"))),
                button("What changed")
                    .on_press(Message::OpenChangelog)
                    .style(theme::secondary_button),
                theme::muted(super::CHANGELOG_URL)
            ]
            .spacing(8)
        )
        .id("settings-about")
    ]
    .spacing(28);
    for warning in &state.warnings {
        content = content.push(theme::muted(warning));
    }
    content.into()
}

/// Queues a job with the worker. A folder that is not a known game is sent as a
/// manual game named after the folder.
#[cfg(windows)]
fn start(state: &mut State, folder: PathBuf, optimize: bool) -> Task<Message> {
    let game = state
        .games
        .iter()
        .find(|game| game.install_dir == folder)
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
    worker_send(crate::windows::coordinator::Command::Enqueue {
        game,
        restore: !optimize,
    })
}
/// Sends one command to the worker, starting it if needed, and feeds the returned
/// snapshot back as `Message::Worker`.
#[cfg(windows)]
fn worker_send(command: crate::windows::coordinator::Command) -> Task<Message> {
    Task::perform(
        background(move || {
            crate::windows::coordinator::request(command).map_err(|error| error.to_string())
        }),
        Message::Worker,
    )
}
/// Asks the worker for a snapshot once a second. `poll` never starts a worker, so
/// a stopped one stays stopped.
#[cfg(windows)]
fn polls() -> impl iced::futures::Stream<Item = Message> {
    iced::futures::stream::unfold((), |_| async {
        let result = background(|| {
            std::thread::sleep(Duration::from_secs(1));
            crate::windows::coordinator::poll().map_err(|error| error.to_string())
        })
        .await;
        Some((Message::Worker(result), ()))
    })
}

/// The worker's jobs in four groups, newest first. History shows its latest 20.
#[cfg(windows)]
fn worker_jobs(state: &State) -> iced::widget::Column<'_, Message> {
    use crate::desktop_jobs::Phase;
    let mut jobs = column![theme::section_title("Jobs")].spacing(12);
    if let Some(error) = &state.worker_error {
        jobs = jobs.push(theme::muted(format!(
            "Worker connection interrupted. Showing the previous jobs. {error}"
        )));
    }
    if let Some(busy) = &state.worker.busy {
        jobs = jobs.push(theme::muted(format!("Paused: {busy}")));
    }
    for (title, phases) in [
        ("Running", vec![Phase::Running, Phase::Paused]),
        ("Waiting", vec![Phase::Waiting]),
        ("Needs attention", vec![Phase::Failed, Phase::Interrupted]),
        ("History", vec![Phase::Completed, Phase::Cancelled]),
    ] {
        let items: Vec<_> = state
            .worker
            .jobs
            .iter()
            .filter(|job| phases.contains(&job.phase))
            .collect();
        jobs = jobs.push(text(format!("{title} · {}", items.len())).size(17));
        if items.is_empty() {
            jobs = jobs.push(theme::muted("No jobs in this section"));
        }
        let mut rows = vec![];
        let limit = if title == "History" { 20 } else { items.len() };
        if items.len() > limit {
            jobs = jobs.push(theme::muted(format!("Showing the latest {limit} jobs")));
        }
        for job in items.iter().rev().take(limit) {
            let mut controls = row![].spacing(8);
            // Active jobs can be paused or cancelled. Failed, interrupted and
            // stopped ones can be retried. Completed ones have no controls.
            if job.phase.active() {
                controls = controls
                    .push(
                        button(if job.user_paused { "Resume" } else { "Pause" }).on_press(
                            Message::WorkerCommand(crate::windows::coordinator::Command::Pause {
                                id: job.id,
                                paused: !job.user_paused,
                            }),
                        ),
                    )
                    .push(button("Cancel").on_press(Message::WorkerCommand(
                        crate::windows::coordinator::Command::Cancel(job.id),
                    )));
            } else if job.phase != Phase::Completed {
                controls = controls.push(button("Retry").on_press(Message::WorkerCommand(
                    crate::windows::coordinator::Command::Retry(job.id),
                )));
            }
            let row: Element<'_, Message> = container(
                column![
                    row![
                        text(&job.game.title).width(Length::Fill),
                        theme::muted(format!(
                            "{} · {}",
                            if job.restore {
                                "Restore"
                            } else {
                                "Compression"
                            },
                            job.phase
                        ))
                    ],
                    theme::muted(&job.message),
                    theme::muted(format!(
                        "{} files · {} changed · {} skipped",
                        job.progress.files, job.progress.changed, job.progress.skipped
                    )),
                    controls
                ]
                .spacing(8),
            )
            .padding(12)
            .style(theme::panel)
            .into();
            rows.push((job.id, row));
        }
        jobs = jobs.push(iced::widget::keyed_column(rows).spacing(10));
    }
    jobs
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

/// Whether Stop has a job to act on. A job still waiting in the queue does not count.
fn can_stop(state: &State) -> bool {
    #[cfg(windows)]
    {
        state
            .worker
            .jobs
            .iter()
            .any(|job| job.phase.active() && job.phase != crate::desktop_jobs::Phase::Waiting)
    }
    #[cfg(target_os = "macos")]
    {
        state.cancel.is_some()
    }
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
        } else {
            "No jobs running".into()
        }
    }
}

#[cfg(test)]
#[path = "native_preview.rs"]
mod preview;
