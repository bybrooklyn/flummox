//! Single-user Windows coordinator for durable queued storage operations.
use crate::{
    desktop::Preferences,
    desktop_jobs::{Job, Phase, Progress, Queue},
    model::Game,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
/// Protocol version sent in every frame. Either side rejects a mismatch.
const VERSION: u32 = 1;
/// A request to the worker. Every command, whether it succeeds or not, is answered
/// with a full `Snapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    /// Changes nothing.
    Snapshot,
    /// Queue a compression, or a restore, of one game folder.
    Enqueue {
        game: Game,
        restore: bool,
    },
    /// Set or clear the user's pause on one job.
    Pause {
        id: u64,
        paused: bool,
    },
    /// The running job stops after its current file. A waiting one is cancelled at once.
    Cancel(u64),
    Retry(u64),
    /// Rediscover games on the next loop pass.
    Refresh,
    /// `true` pauses all background work, `false` resumes it.
    Maintenance(bool),
    /// Replace the saved preferences. `maintenance_paused` in the payload is ignored.
    Settings(Preferences),
    /// Cancel waiting jobs, stop the running one after its current file, then exit.
    Shutdown,
}
/// A request in the main loop's channel, with its reply slot and a flag the
/// listener raises when it stopped waiting for the reply.
type Pending = (Request, mpsc::Sender<Response>, Arc<AtomicBool>);
/// The worker's whole visible state at one moment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// A shutdown was requested and the worker exits once its job stops.
    pub stopping: bool,
    /// The worker's start time in nanoseconds since the Unix epoch. With `revision`
    /// it orders snapshots, including across a worker restart.
    pub epoch: u64,
    /// Incremented for every reply.
    pub revision: u64,
    pub jobs: Vec<Job>,
    pub games: Vec<Game>,
    pub warnings: Vec<String>,
    pub artwork_roots: Vec<PathBuf>,
    /// Why every job is held, if one is: a running game, a launcher at work, or
    /// preferences that cannot be read.
    pub busy: Option<String>,
    /// A discovery pass is running. No job starts until it ends.
    pub discovering: bool,
    pub maintenance_paused: bool,
    /// Why maintenance set a new starting point for a location, such as a different
    /// drive at the same path. The newest five are kept until the worker exits.
    #[serde(default)]
    pub notices: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Request {
    version: u32,
    command: Command,
}
#[derive(Serialize, Deserialize)]
struct Response {
    version: u32,
    snapshot: Snapshot,
    error: Option<String>,
}
/// One exchange on an open pipe: request, response, acknowledgement. A command the
/// worker refused comes back as an error.
fn call_file(command: Command, mut file: std::fs::File) -> Result<Snapshot> {
    crate::windows::ipc::send(
        &mut file,
        &Request {
            version: VERSION,
            command,
        },
    )?;
    let response: Response = crate::windows::ipc::receive(&mut file)?;
    // The server retains its buffers until the client has read the complete reply.
    crate::windows::ipc::send(&mut file, &true)?;
    ensure!(
        response.version == VERSION,
        "The background worker is from another version. Restart Flummox."
    );
    if let Some(error) = response.error {
        anyhow::bail!(error);
    }
    Ok(response.snapshot)
}
/// A snapshot from the running worker. Fails if none is running, and never starts one.
pub fn poll() -> Result<Snapshot> {
    call(Command::Snapshot)
}
fn call(command: Command) -> Result<Snapshot> {
    call_file(command, crate::windows::ipc::connect()?)
}
/// Sends a command, first starting a worker if none answers and waiting up to 5
/// seconds for its pipe. `Shutdown` never starts one.
pub fn request(command: Command) -> Result<Snapshot> {
    match crate::windows::ipc::connect() {
        Ok(file) => return call_file(command, file),
        // Only a missing pipe means no worker runs. Any other failure, such as a
        // refused connection, must not start a second one.
        Err(error) if !crate::windows::ipc::is_missing(&error) => return Err(error),
        Err(_) => {}
    }
    if matches!(command, Command::Shutdown) {
        return call(command);
    }
    // The worker is this same executable with a flag that `entrypoint` recognises.
    use std::os::windows::process::CommandExt;
    std::process::Command::new(std::env::current_exe()?)
        .arg("--native-coordinator")
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(file) = crate::windows::ipc::connect() {
            return call_file(command, file);
        }
    }
    call(command)
}
/// Handles the worker flags. Both binaries call this first in `main` and must
/// exit when it returns true. `--native-coordinator` runs the worker in this
/// process. `--background` makes sure one is running. `--native-worker-exit` stops
/// it and waits up to 30 seconds for it to finish its file.
pub fn entrypoint() -> Result<bool> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.iter().any(|arg| arg == "--native-coordinator") {
        run()?;
        return Ok(true);
    }
    if args.iter().any(|arg| arg == "--background") {
        request(Command::Snapshot)?;
        return Ok(true);
    }
    if args.iter().any(|arg| arg == "--native-worker-exit") {
        let root = crate::libraries::data_dir()?;
        crate::libraries::private_dir(&root)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("coordinator.lock"))?;
        // A worker holds this lock for its whole life. Getting it means none is running.
        let stopped = wait_for_worker(
            || lock.try_lock().is_ok(),
            || call(Command::Shutdown).map(|_| ()),
            Duration::from_secs(30),
            Duration::from_secs(1),
        )?;
        if stopped == Stopped::StillWorking {
            eprintln!(
                "The background worker is finishing a file. Wait before upgrading or uninstalling."
            );
            // The installer reads 3 as "still working" and any other failure as an
            // error. Returning an error from `main` can only give 1.
            #[allow(clippy::exit)]
            std::process::exit(EXIT_STILL_WORKING);
        }
        cleanup_startup(&args, &root)?;
        return Ok(true);
    }
    Ok(false)
}
/// Exit code of `--native-worker-exit` when the worker is still finishing a file.
const EXIT_STILL_WORKING: i32 = 3;
#[derive(Debug, PartialEq, Eq)]
enum Stopped {
    /// No worker holds the lock any more.
    Gone,
    /// A worker still holds it after the patience ran out.
    StillWorking,
}
/// Asks the worker to shut down until `released` says it has let go of its lock, or
/// `patience` runs out. `shutdown` is repeated every `resend`, because a worker
/// that is starting or exiting may not hear the first request. A failed request is
/// not an error while the lock is still held: the worker may be on its way out. At
/// the deadline the last failure is returned unless it only says the pipe is
/// missing. A worker that is gone, whether before the first request or during the
/// wait, is `Gone`.
fn wait_for_worker(
    mut released: impl FnMut() -> bool,
    mut shutdown: impl FnMut() -> Result<()>,
    patience: Duration,
    resend: Duration,
) -> Result<Stopped> {
    let deadline = Instant::now() + patience;
    let mut last_sent: Option<Instant> = None;
    let mut last_error: Option<anyhow::Error> = None;
    loop {
        if released() {
            return Ok(Stopped::Gone);
        }
        if Instant::now() >= deadline {
            return match last_error {
                Some(error) if !crate::windows::ipc::is_missing(&error) => Err(error),
                _ => Ok(Stopped::StillWorking),
            };
        }
        if due(last_sent, resend) {
            last_sent = Some(Instant::now());
            last_error = shutdown().err();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
/// With `--remove-owned-startup`, deletes this installation's login entry and
/// clears the matching preference.
fn cleanup_startup(args: &[std::ffi::OsString], root: &std::path::Path) -> Result<()> {
    // Neither step may fail the call: this runs before an uninstall, which a
    // startup entry from another installation or a damaged preferences file
    // would otherwise block for good.
    if args.iter().any(|arg| arg == "--remove-owned-startup") {
        if let Err(error) = crate::windows::launchers::startup(false) {
            tracing::warn!(%error, "Startup entry left in place");
        }
        match Preferences::load(root) {
            Ok(mut preferences) => {
                preferences.start_at_login = false;
                if let Err(error) = preferences.save(root) {
                    tracing::warn!(%error, "Preferences could not be updated");
                }
            }
            Err(error) => tracing::warn!(%error, "Preferences could not be read"),
        }
    }
    Ok(())
}
/// The running job's thread and its controls. Dropping it raises `cancel`, clears
/// `pause` and joins the thread, so the drop blocks until the current file is done.
struct Active {
    id: u64,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    events: mpsc::Receiver<Event>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// When the job's volume was last checked, and what the check found.
    drive_check: Instant,
    drive_online: bool,
}
impl Drop for Active {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.pause.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // Drain progress so a full bounded channel cannot prevent safe shutdown.
            while !thread.is_finished() {
                for _event in self.events.try_iter() {}
                std::thread::sleep(Duration::from_millis(5));
            }
            let _joined = thread.join();
        }
    }
}
/// Stops and joins the pipe listener thread when the coordinator returns, on
/// every path including an error.
struct ListenerStop {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for ListenerStop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _joined = thread.join();
        }
    }
}
/// What a job thread sends back. `Finished` is always its last event.
enum Event {
    Progress(Progress),
    Finished(std::result::Result<String, String>),
}
/// Starts the job's thread. The channel is bounded, so a job thread that gets 256
/// events ahead of the coordinator blocks until it catches up.
fn start(job: &Job) -> Active {
    let (send, events) = mpsc::sync_channel(256);
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    let paused = pause.clone();
    let game = job.game.clone();
    let restore = job.restore;
    let thread = std::thread::spawn(move || {
        let progress_send = send.clone();
        let result = crate::windows::folder_controlled(
            &game.install_dir,
            restore,
            &stop,
            &paused,
            move |progress| {
                let _sent = progress_send.send(Event::Progress(Progress {
                    files: progress.files,
                    changed: progress.changed,
                    skipped: progress.skipped,
                    bytes: progress.bytes,
                    allocation_before: progress.allocation_before,
                    allocation_after: progress.allocation_after,
                    failed: progress.failed,
                }));
            },
        )
        .map_err(|error| error.to_string());
        let _sent = send.send(Event::Finished(result));
    });
    Active {
        id: job.id,
        cancel,
        pause,
        events,
        thread: Some(thread),
        drive_check: Instant::now(),
        drive_online: true,
    }
}
/// Whether a periodic step is due. A step that has never run is due at once,
/// which an `Instant` cannot express near boot, where it has no earlier value.
fn due(last: Option<Instant>, every: Duration) -> bool {
    last.is_none_or(|last| last.elapsed() >= every)
}

/// Writes the queue. A failure is logged and flagged so the periodic save tries
/// again, and the worker keeps serving.
fn save_queue(queue: &Queue, root: &std::path::Path, dirty: &mut bool) {
    if let Err(error) = queue.save(root) {
        tracing::warn!(%error, "Job queue could not be saved");
        *dirty = true;
    }
}

/// The worker: one loop, about every 20 ms, that owns the queue, runs at most one
/// job at a time and answers clients. Returns after a shutdown once no job is
/// running, or at once if another worker already holds the lock.
fn run() -> Result<()> {
    let root = crate::libraries::data_dir()?;
    crate::libraries::private_dir(&root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("coordinator.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    // `load` marks jobs the last worker left mid-run as interrupted. Save that now.
    let (mut queue, damaged) = Queue::load_or_quarantine(&root)?;
    if let Some(note) = damaged {
        tracing::warn!("{note}");
    }
    if let Err(error) = queue.save(&root) {
        tracing::warn!(%error, "Job queue could not be saved at start");
    }
    let mut pipe = crate::windows::ipc::listener()?;
    let (requests, receive) = mpsc::channel::<Pending>();
    let stopped = Arc::new(AtomicBool::new(false));
    let listener_stop = stopped.clone();
    let listener_failed = Arc::new(AtomicBool::new(false));
    let pipe_failed = listener_failed.clone();
    // The listener serves one client at a time: read a request, pass it to the main
    // loop, wait up to 3 seconds for the reply, write it, wait for the client's
    // acknowledgement, disconnect. Only the main loop touches the queue.
    let listener_thread = std::thread::spawn(move || {
        while !listener_stop.load(Ordering::Relaxed) {
            match crate::windows::ipc::accept(&pipe) {
                Ok(true) => {
                    if let Ok(request) = crate::windows::ipc::receive::<Request>(&mut pipe) {
                        let (send, reply) = mpsc::channel();
                        let abandoned = Arc::new(AtomicBool::new(false));
                        if requests.send((request, send, abandoned.clone())).is_ok() {
                            if let Ok(response) = reply.recv_timeout(Duration::from_secs(3))
                                && crate::windows::ipc::send(&mut pipe, &response).is_ok()
                            {
                                let _acknowledged = crate::windows::ipc::receive::<bool>(&mut pipe);
                            } else {
                                // The client was told it failed, so the loop must
                                // not carry the command out afterwards.
                                abandoned.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    crate::windows::ipc::disconnect(&pipe);
                }
                Ok(false) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => {
                    tracing::warn!(%error, "Worker pipe failed");
                    pipe_failed.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });
    let _listener_lifetime = ListenerStop {
        stop: stopped.clone(),
        thread: Some(listener_thread),
    };
    let mut snapshot = Snapshot {
        epoch: u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
        )?,
        ..Default::default()
    };
    let mut preferences = match Preferences::load(&root) {
        Ok(preferences) => preferences,
        Err(error) => {
            // Set the damaged file aside so the worker can start and the window
            // can save new settings.
            let aside = crate::desktop::quarantine(&root.join("desktop.json"))?;
            tracing::warn!(%error, kept = %aside.display(), "Preferences were unreadable");
            Preferences::default()
        }
    };
    let mut preferences_healthy = true;
    if preferences.start_at_login
        && let Err(error) = crate::windows::launchers::startup(true)
    {
        tracing::warn!(%error, "Startup entry could not be refreshed");
    }
    let tray = tray()
        .map_err(|error| {
            tracing::warn!(%error, "Worker tray unavailable");
            error
        })
        .ok();
    // A timer that is `None` has never run, so the first loop pass runs its step.
    let mut active: Option<Active> = None;
    let mut shutdown = false;
    let mut queue_dirty = false;
    let mut last_scan: Option<Instant> = None;
    let mut last_activity: Option<Instant> = None;
    let mut last_save = Instant::now();
    let mut last_preferences: Option<Instant> = None;
    // No job starts before one discovery has finished and one activity check has
    // run for the jobs waiting.
    let mut discovered_once = false;
    let mut activity_checked = false;
    // The preferences the running discovery started with. Its catalog may hold
    // games from locations added later, but never from locations missing here.
    let mut scan_preferences = preferences.clone();
    let mut last_start_failure: Option<Instant> = None;
    let mut discovery: Option<
        mpsc::Receiver<std::result::Result<crate::desktop_discovery::Catalog, String>>,
    > = None;
    loop {
        ensure!(
            !listener_failed.load(Ordering::Relaxed),
            "The background worker lost its connection and stopped safely. Restart Flummox."
        );
        // Reread preferences every second. While they cannot be read the old copy
        // stays in use and every job is held.
        if due(last_preferences, Duration::from_secs(1)) {
            last_preferences = Some(Instant::now());
            match Preferences::load(&root) {
                Ok(settings) => {
                    preferences = settings;
                    preferences_healthy = true;
                }
                Err(error) => {
                    preferences_healthy = false;
                    snapshot.busy = Some(format!("Your settings could not be read: {error}"));
                }
            }
        }
        // Discovery runs on its own thread, every 30 seconds, or every 3 while a job
        // is running. Only one pass runs at a time.
        if !shutdown
            && discovery.is_none()
            && due(
                last_scan,
                Duration::from_secs(if active.is_some() { 3 } else { 30 }),
            )
        {
            last_scan = Some(Instant::now());
            scan_preferences = preferences.clone();
            let (send, receive) = mpsc::channel();
            discovery = Some(receive);
            snapshot.discovering = true;
            std::thread::spawn(move || {
                let _sent = send.send(discover().map_err(|error| error.to_string()));
            });
        }
        let discovered = discovery
            .as_ref()
            .and_then(|receiver| match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(
                    "Finding games stopped unexpectedly. Refresh to try again.".into(),
                )),
            });
        if let Some(result) = discovered {
            discovery = None;
            snapshot.discovering = false;
            discovered_once = true;
            match result {
                Ok(catalog) => {
                    snapshot.games = catalog.games;
                    snapshot.artwork_roots = catalog.artwork_roots;
                    snapshot.warnings = catalog.warnings;
                    // Maintenance: queue what `observe` reports as due. A scan with
                    // warnings counts as unhealthy and queues nothing. The baseline
                    // moves when the job ends, in `Queue::settle`. The scan's own
                    // preferences are used, since its catalog was built from them.
                    for game in queue.observe(
                        &snapshot.games,
                        &scan_preferences,
                        snapshot.warnings.is_empty(),
                    ) {
                        if let Err(error) = queue.enqueue_automatic(game) {
                            snapshot.warnings.push(error.to_string());
                        }
                    }
                    for notice in std::mem::take(&mut queue.notices) {
                        if !snapshot.notices.contains(&notice) {
                            snapshot.notices.push(notice);
                        }
                    }
                    let extra = snapshot.notices.len().saturating_sub(5);
                    snapshot.notices.drain(..extra);
                    // Remember which drive each location is on, now that `observe`
                    // has compared against what was saved.
                    if preferences_healthy
                        && snapshot.warnings.is_empty()
                        && preferences.record_volumes(&crate::desktop::volume_identity)
                        && let Err(error) = preferences.save(&root)
                    {
                        snapshot.warnings.push(error.to_string());
                    }
                    save_queue(&queue, &root, &mut queue_dirty);
                }
                Err(error) => snapshot.warnings = vec![error],
            }
        }
        // Every second, look for a running game or launcher tool. Folders of active
        // jobs are included even if discovery no longer lists them. A failed check
        // holds work, with the error as the reason.
        let jobs_wait = queue.jobs.iter().any(|job| job.phase.active());
        if !jobs_wait {
            // Nothing to hold, so the check is skipped. A job queued later waits
            // for a fresh check before it can start.
            last_activity = None;
            activity_checked = false;
            if preferences_healthy {
                snapshot.busy = None;
            }
        } else if due(last_activity, Duration::from_secs(1)) {
            last_activity = Some(Instant::now());
            activity_checked = true;
            let mut games = snapshot.games.clone();
            games.extend(
                queue
                    .jobs
                    .iter()
                    .filter(|job| job.phase.active())
                    .map(|job| job.game.clone()),
            );
            snapshot.busy = Some("Checking running games…".into());
            snapshot.busy = activity(&games).unwrap_or_else(|error| Some(error.to_string()));
            if !preferences_healthy {
                snapshot.busy = Some(
                    "Your settings could not be read. Check that the settings file is not damaged."
                        .into(),
                );
            }
        }
        if let Some((_, actions)) = &tray {
            for action in actions.try_iter() {
                match action {
                    crate::windows::tray::Action::Open => {
                        let opened = std::env::current_exe()
                            .map_err(anyhow::Error::from)
                            .and_then(|path| {
                                std::process::Command::new(path.with_file_name("flummox-gui.exe"))
                                    .spawn()
                                    .map_err(anyhow::Error::from)
                            });
                        if let Err(error) = opened {
                            tracing::warn!(%error, "Flummox window could not open");
                        }
                    }
                    crate::windows::tray::Action::Pause | crate::windows::tray::Action::Resume => {
                        preferences.maintenance_paused =
                            matches!(action, crate::windows::tray::Action::Pause);
                        if let Err(error) = preferences.save(&root) {
                            tracing::warn!(%error, "Pause setting could not be saved");
                        }
                    }
                    crate::windows::tray::Action::Exit => {
                        // Waiting jobs stay waiting, and the next worker picks them up.
                        shutdown = true;
                        if let Some(running) = &active {
                            running.cancel.store(true, Ordering::Relaxed);
                        }
                        save_queue(&queue, &root, &mut queue_dirty);
                    }
                }
            }
        }
        // Answer clients. Each command runs in a closure so its error goes back to
        // that client and does not end the loop.
        for (request, reply, abandoned) in receive.try_iter().take(64) {
            if abandoned.load(Ordering::Relaxed) {
                continue;
            }
            let result = (|| -> Result<()> {
                ensure!(
                    request.version == VERSION,
                    "The background worker is from another version. Restart Flummox."
                );
                match request.command {
                    Command::Snapshot => {}
                    // Backdating the timer makes the discovery step above fire next pass.
                    Command::Refresh => last_scan = None,
                    Command::Enqueue { mut game, restore } => {
                        game.install_dir = game.install_dir.canonicalize()?;
                        ensure!(game.install_dir.is_dir(), "Choose an installed game folder");
                        crate::desktop::ProtectedFolders::from_environment()
                            .check(&game.install_dir)?;
                        ensure!(
                            restore
                                || !game
                                    .ids()
                                    .any(|id| preferences.excluded.contains(&id.to_string())),
                            "This game is excluded. Include it before running a job."
                        );
                        queue.enqueue(game, restore)?;
                        queue.save(&root)?;
                    }
                    Command::Pause { id, paused } => {
                        queue
                            .jobs
                            .iter_mut()
                            .find(|job| job.id == id)
                            .context("Job no longer exists")?
                            .user_paused = paused;
                        queue.save(&root)?;
                    }
                    Command::Cancel(id) => {
                        let job = queue
                            .jobs
                            .iter_mut()
                            .find(|job| job.id == id)
                            .context("Job no longer exists")?;
                        if let Some(running) = &active
                            && running.id == id
                        {
                            running.cancel.store(true, Ordering::Relaxed);
                            job.message = "Stopping after the current file…".into();
                        } else if job.phase.active() {
                            job.phase = Phase::Cancelled;
                            // The user chose this, so maintenance leaves the build alone.
                            queue.settle(id, false);
                        }
                        queue.save(&root)?;
                    }
                    Command::Retry(id) => {
                        queue.retry(id)?;
                        queue.save(&root)?;
                    }
                    Command::Maintenance(paused) => {
                        preferences.maintenance_paused = paused;
                        preferences.save(&root)?;
                    }
                    Command::Settings(mut settings) => {
                        // The payload's pause flag is discarded in favour of the
                        // worker's current one.
                        settings.maintenance_paused = preferences.maintenance_paused;
                        if settings.start_at_login != preferences.start_at_login {
                            crate::windows::launchers::startup(settings.start_at_login)?;
                        }
                        settings.save(&root)?;
                        preferences = settings;
                        last_scan = None;
                    }
                    Command::Shutdown => {
                        // Waiting jobs stay waiting, and the next worker picks them up.
                        shutdown = true;
                        if let Some(running) = &active {
                            running.cancel.store(true, Ordering::Relaxed);
                        }
                        queue.save(&root)?;
                    }
                }
                Ok(())
            })();
            snapshot.stopping = shutdown;
            snapshot.jobs = queue.jobs.clone();
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.maintenance_paused = preferences.maintenance_paused;
            let _sent = reply.send(Response {
                version: VERSION,
                snapshot: snapshot.clone(),
                error: result.err().map(|error| error.to_string()),
            });
        }
        // Preferences may have changed under queued work. Cancel compression of a
        // game that is now excluded, and automatic jobs whose location opted out.
        for job in &mut queue.jobs {
            let excluded = !job.restore
                && job
                    .game
                    .ids()
                    .any(|id| preferences.excluded.contains(&id.to_string()));
            let opted_out = job.automatic
                && !preferences.locations.iter().any(|location| {
                    location.automatic && job.game.install_dir.starts_with(&location.path)
                });
            if excluded || opted_out {
                if let Some(running) = &active
                    && running.id == job.id
                {
                    running.cancel.store(true, Ordering::Relaxed);
                } else if job.phase.active() {
                    job.phase = Phase::Cancelled;
                    job.message = "This game is excluded, so the job was stopped.".into();
                    queue_dirty = true;
                }
            }
        }
        // The running job: decide whether it should be paused right now, set its
        // message, then take the events its thread has sent.
        let mut finished = false;
        let mut finished_id = None;
        if let Some(running) = &mut active
            && let Some(job) = queue.jobs.iter_mut().find(|job| job.id == running.id)
        {
            if running.drive_check.elapsed() >= Duration::from_secs(1) {
                running.drive_check = Instant::now();
                running.drive_online = job.game.install_dir.is_dir()
                    && job.volume.as_ref().is_some_and(|original| {
                        crate::storage::volume(&job.game.install_dir)
                            .is_ok_and(|current| current.identity == original.identity)
                    });
            }
            let before = (job.phase, job.message.clone());
            // Discovery now reports this game as busy, updating or broken.
            let unavailable = snapshot
                .games
                .iter()
                .find(|game| game.install_dir == job.game.install_dir)
                .is_some_and(|game| !game.state.is_idle());
            let pause = !shutdown
                && (job.user_paused
                    || preferences.maintenance_paused
                    || snapshot.busy.is_some()
                    || unavailable
                    || !running.drive_online
                    || (job.automatic && !snapshot.warnings.is_empty()));
            running.pause.store(pause, Ordering::Relaxed);
            job.phase = if pause { Phase::Paused } else { Phase::Running };
            job.message = if job.user_paused {
                "Paused by you".into()
            } else if preferences.maintenance_paused {
                "Background jobs are paused".into()
            } else if !running.drive_online {
                "Reconnect the original drive".into()
            } else if let Some(reason) = &snapshot.busy {
                reason.clone()
            } else if unavailable {
                "Waiting for the launcher…".into()
            } else if job.automatic && !snapshot.warnings.is_empty() {
                "Finding games needs attention. Refresh to try again.".into()
            } else if running.cancel.load(Ordering::Relaxed) {
                "Stopping after the current file…".into()
            } else {
                "Processing files…".into()
            };
            queue_dirty |= before != (job.phase, job.message.clone());
            // At most 256 events per pass, so a fast job cannot starve the loop.
            for _ in 0..256 {
                let event = match running.events.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    // The thread ended without sending `Finished`.
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if !finished {
                            finished = true;
                            job.phase = Phase::Failed;
                            job.message =
                                "The job stopped unexpectedly. Review it under Recovery before trying again.".into();
                        }
                        break;
                    }
                };
                match event {
                    Event::Progress(progress) => {
                        job.progress = progress;
                        queue_dirty = true;
                    }
                    Event::Finished(result) => {
                        finished = true;
                        finished_id = Some(job.id);
                        job.phase = if running.cancel.load(Ordering::Relaxed) {
                            Phase::Cancelled
                        } else if result.is_ok() {
                            Phase::Completed
                        } else {
                            Phase::Failed
                        };
                        job.message = match result {
                            Ok(message) => message,
                            Err(error) => error,
                        };
                    }
                }
            }
        }
        // Dropping `Active` joins the thread, which has already sent its last event.
        if finished {
            active = None;
            if let Some(id) = finished_id {
                queue.settle(id, shutdown);
            }
            save_queue(&queue, &root, &mut queue_dirty);
        }
        if active.is_none() && shutdown {
            stopped.store(true, Ordering::Relaxed);
            return Ok(());
        }
        // An interrupted pass left its journal. Automatic jobs wait for the user to
        // deal with it. The journal is read only when a job could start.
        let recovery_pending =
            if active.is_none() && queue.jobs.iter().any(|job| job.phase == Phase::Waiting) {
                // An unreadable journal holds work back like a present one.
                crate::windows::recovery().map_or_else(
                    |error| {
                        tracing::warn!(%error, "Job journal could not be read");
                        true
                    },
                    |records| !records.is_empty(),
                )
            } else {
                false
            };
        // Give every waiting job a message naming the first thing it waits for.
        for job in queue
            .jobs
            .iter_mut()
            .filter(|job| job.phase == Phase::Waiting)
        {
            let message = if job.user_paused {
                "Paused by you".into()
            } else if preferences.maintenance_paused {
                "Background jobs are paused".into()
            } else if let Some(reason) = &snapshot.busy {
                reason.clone()
            } else if !job.game.install_dir.is_dir() {
                "Reconnect the original drive".into()
            } else if let Some(game) = snapshot
                .games
                .iter()
                .find(|game| game.install_dir == job.game.install_dir && !game.state.is_idle())
            {
                format!("Waiting: {}", game.state)
            } else if job.automatic && recovery_pending {
                "Review the interrupted job before background jobs run".into()
            } else if job.automatic && !snapshot.warnings.is_empty() {
                "Finding games needs attention. Refresh to try again.".into()
            } else if snapshot.discovering {
                "Checking installed games…".into()
            } else if active.is_some() {
                "Waiting for the current job…".into()
            } else {
                "Waiting to start…".into()
            };
            if job.message != message {
                job.message = message;
                queue_dirty = true;
            }
        }
        // Start the first waiting job that nothing holds: its folder exists on the
        // volume it was queued on, and discovery does not report the game as busy.
        if !shutdown
            && active.is_none()
            && snapshot.busy.is_none()
            && discovered_once
            && activity_checked
            && !preferences.maintenance_paused
            && !snapshot.discovering
            && last_start_failure.is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
            && let Some(job) = queue.jobs.iter_mut().find(|job| {
                job.phase == Phase::Waiting
                    && (!job.automatic || (snapshot.warnings.is_empty() && !recovery_pending))
                    && !job.user_paused
                    && job.game.install_dir.is_dir()
                    && job.volume.as_ref().is_some_and(|original| {
                        crate::storage::volume(&job.game.install_dir)
                            .is_ok_and(|current| current.identity == original.identity)
                    })
                    && snapshot
                        .games
                        .iter()
                        .find(|game| game.install_dir == job.game.install_dir)
                        .is_none_or(|game| game.state.is_idle())
            })
        {
            // Saved as running before the thread starts, so a crash from here on is
            // loaded as an interrupted job.
            let id = job.id;
            job.phase = Phase::Running;
            job.message = "Preparing files…".into();
            if let Err(error) = queue.save(&root) {
                // Without the record a crash would lose track of the job, so it
                // does not start until the queue can be written.
                tracing::warn!(%error, "Job not started: the queue could not be saved");
                for job in &mut queue.jobs {
                    if job.id == id {
                        job.phase = Phase::Waiting;
                        job.message = "Waiting. The job list could not be saved.".into();
                    }
                }
                last_start_failure = Some(Instant::now());
            } else {
                let job = queue
                    .jobs
                    .iter()
                    .find(|job| job.id == id)
                    .context("Starting job disappeared")?;
                active = Some(start(job));
            }
        }
        if let Some((tray, _)) = &tray {
            tray.set_paused(preferences.maintenance_paused);
        }
        // Changes flagged through `queue_dirty` are written at most once a second.
        if queue_dirty && last_save.elapsed() >= Duration::from_secs(1) {
            last_save = Instant::now();
            queue_dirty = false;
            save_queue(&queue, &root, &mut queue_dirty);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Discovery, with a seam for tests: a test build reads the catalog from the file
/// named by FLUMMOX_TEST_CATALOG. The same variable disables the tray and the
/// process check in the two functions below.
fn discover() -> Result<crate::desktop_discovery::Catalog> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("FLUMMOX_TEST_CATALOG") {
        return Ok(serde_json::from_slice(&crate::desktop::read_bounded(
            &PathBuf::from(path),
            16 * 1024 * 1024,
        )?)?);
    }
    crate::native::discover_catalog()
}
fn tray() -> Result<(
    crate::windows::tray::Tray,
    mpsc::Receiver<crate::windows::tray::Action>,
)> {
    #[cfg(test)]
    if std::env::var_os("FLUMMOX_TEST_CATALOG").is_some() {
        anyhow::bail!("Tray disabled in the isolated coordinator fixture");
    }
    crate::windows::tray::spawn()
}

fn activity(games: &[Game]) -> Result<Option<String>> {
    #[cfg(test)]
    if std::env::var_os("FLUMMOX_TEST_CATALOG").is_some() {
        return Ok(None);
    }
    crate::windows::activity::busy(games)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    fn missing_pipe() -> anyhow::Error {
        std::io::Error::from(std::io::ErrorKind::NotFound).into()
    }
    #[test]
    fn a_worker_that_is_gone_or_leaves_during_the_wait_is_not_an_error() -> TestResult {
        let patience = Duration::from_secs(5);
        let resend = Duration::from_millis(10);
        let mut asked = 0;
        check_eq(
            wait_for_worker(|| true, || Err(missing_pipe()), patience, resend)
                .ctx("lock already free")?,
            Stopped::Gone,
            "no worker needs no request",
        )?;
        let mut polls = 0;
        check_eq(
            wait_for_worker(
                || {
                    polls += 1;
                    polls > 3
                },
                || {
                    asked += 1;
                    Err(missing_pipe())
                },
                patience,
                resend,
            )
            .ctx("worker exits before hearing the request")?,
            Stopped::Gone,
            "a missing pipe while the lock frees is a worker that left",
        )?;
        check(asked >= 1, "the request was tried")
    }
    #[test]
    fn a_worker_that_keeps_its_lock_is_still_working_or_an_error() -> TestResult {
        let patience = Duration::from_millis(100);
        let resend = Duration::from_millis(10);
        check_eq(
            wait_for_worker(|| false, || Ok(()), patience, resend).ctx("busy worker")?,
            Stopped::StillWorking,
            "control: a worker that does not let go blocks the upgrade",
        )?;
        check_eq(
            wait_for_worker(|| false, || Err(missing_pipe()), patience, resend)
                .ctx("lock held with no pipe")?,
            Stopped::StillWorking,
            "a held lock is never reported as gone",
        )?;
        check(
            wait_for_worker(
                || false,
                || Err(anyhow::anyhow!("Another account is using the pipe")),
                patience,
                resend,
            )
            .is_err(),
            "a real failure to reach the worker is reported",
        )
    }
    #[test]
    fn a_step_that_never_ran_is_due_and_a_recent_one_is_not() -> TestResult {
        check(due(None, Duration::from_secs(30)), "never run is due")?;
        check(
            !due(Some(Instant::now()), Duration::from_secs(30)),
            "a step that just ran waits its interval",
        )?;
        check(
            due(Some(Instant::now()), Duration::ZERO),
            "a zero interval is always due",
        )
    }
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _killed = self.0.kill();
            let _reaped = self.0.wait();
        }
    }
    // Not a test of its own. `launch` runs the test binary filtered to this
    // function, which becomes a real coordinator in a child process.
    #[test]
    #[ignore = "Subprocess entrypoint for the isolated coordinator test"]
    fn helper() -> TestResult {
        if std::env::var_os("FLUMMOX_TEST_CATALOG").is_none() {
            return Ok(());
        }
        run().map_err(|error| error.to_string())
    }
    // LOCALAPPDATA moves the worker's state into the fixture, and the pipe suffix
    // keeps it apart from any real worker on this machine.
    fn launch(
        base: &std::path::Path,
        catalog: &std::path::Path,
        suffix: &str,
    ) -> std::result::Result<Child, String> {
        std::process::Command::new(std::env::current_exe().ctx("test executable")?)
            .args([
                "--exact",
                "windows::coordinator::tests::helper",
                "--ignored",
                "--nocapture",
            ])
            .env("LOCALAPPDATA", base)
            .env("FLUMMOX_TEST_CATALOG", catalog)
            .env("FLUMMOX_IPC_TEST_NAME", suffix)
            .spawn()
            .map(Child)
            .ctx("spawn isolated coordinator")
    }
    fn fixture_call(suffix: &str, command: Command) -> Result<Snapshot> {
        call_file(command, crate::windows::ipc::test_connect(suffix)?)
    }
    fn wait_exit(child: &mut Child) -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.0.try_wait().ctx("worker exit")? {
                return check(status.success(), "fixture coordinator exits cleanly");
            }
            check(Instant::now() < deadline, "fixture worker stops")?;
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn clients_can_close_jobs_complete_and_restart_preserves_history() -> TestResult {
        let temp = tempfile::tempdir().ctx("coordinator fixture")?;
        let game_path = temp.path().join("Fixture Game");
        std::fs::create_dir(&game_path).ctx("game")?;
        let original = vec![b'x'; 128 * 1024];
        let payload = game_path.join("payload.dat");
        std::fs::write(&payload, &original).ctx("game payload")?;
        let game = crate::desktop::manual_game("Fixture Game".into(), game_path);
        let catalog_path = temp.path().join("catalog.json");
        std::fs::write(
            &catalog_path,
            serde_json::to_vec(&crate::desktop_discovery::Catalog {
                games: vec![game.clone()],
                ..Default::default()
            })
            .ctx("fixture catalog")?,
        )
        .ctx("save catalog")?;
        // Seed the queue file with one user-paused job before the worker starts.
        let state = temp.path().join("flummox");
        let mut queue = Queue::default();
        let id = queue.enqueue(game.clone(), false).ctx("fixture job")?;
        queue.jobs.first_mut().ctx("job")?.user_paused = true;
        queue.save(&state).ctx("initial queue")?;
        let suffix = blake3::hash(temp.path().as_os_str().as_encoded_bytes())
            .to_hex()
            .to_string();
        let mut child = launch(temp.path(), &catalog_path, &suffix)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let snapshot = loop {
            match fixture_call(&suffix, Command::Snapshot) {
                Ok(snapshot) => break snapshot,
                Err(error) => {
                    check(
                        Instant::now() < deadline,
                        format!("isolated pipe starts: {error:#}"),
                    )?;
                    check(
                        child.0.try_wait().ctx("fixture worker status")?.is_none(),
                        "fixture worker stays alive during startup",
                    )?;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        check(
            snapshot.jobs.first().ctx("waiting job")?.user_paused,
            "paused queue survives startup",
        )?;
        // A bad protocol cannot mutate the queue.
        let mut pipe = crate::windows::ipc::test_connect(&suffix).ctx("version test")?;
        crate::windows::ipc::send(
            &mut pipe,
            &Request {
                version: VERSION + 1,
                command: Command::Cancel(id),
            },
        )
        .ctx("bad version request")?;
        let response: Response =
            crate::windows::ipc::receive(&mut pipe).ctx("bad version response")?;
        crate::windows::ipc::send(&mut pipe, &true).ctx("rejected request acknowledgement")?;
        check(
            response.error.is_some(),
            "old or unknown protocol is rejected",
        )?;
        drop(pipe);
        fixture_call(&suffix, Command::Pause { id, paused: false }).ctx("resume")?;
        // Each poll closes its pipe. The worker owns the job independently.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let snapshot = fixture_call(&suffix, Command::Snapshot).ctx("poll")?;
            let job = snapshot.jobs.first().ctx("job")?;
            check(
                !matches!(job.phase, Phase::Failed | Phase::Interrupted),
                format!("fixture job failed: {}", job.message),
            )?;
            if job.phase == Phase::Completed {
                check(
                    job.progress.changed > 0,
                    "compressible fixture really uses WOF",
                )?;
                break;
            }
            check(Instant::now() < deadline, "fixture compression finishes")?;
            std::thread::sleep(Duration::from_millis(50));
        }
        check_eq(
            std::fs::read(&payload).ctx("logical bytes")?,
            original.clone(),
            "WOF preserves game bytes",
        )?;
        let snapshot = fixture_call(
            &suffix,
            Command::Enqueue {
                game: game.clone(),
                restore: false,
            },
        )
        .ctx("incremental pass")?;
        let second = snapshot.jobs.last().ctx("second job")?.id;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let snapshot = fixture_call(&suffix, Command::Snapshot).ctx("incremental snapshot")?;
            let job = snapshot
                .jobs
                .iter()
                .find(|job| job.id == second)
                .ctx("incremental job")?;
            check(
                !matches!(job.phase, Phase::Failed | Phase::Interrupted),
                format!("incremental pass failed: {}", job.message),
            )?;
            if job.phase == Phase::Completed {
                check_eq(
                    job.progress.changed,
                    0,
                    "unchanged WOF files are not reopened for write",
                )?;
                check(
                    job.progress.skipped > 0,
                    "unchanged compression is reported as skipped",
                )?;
                break;
            }
            check(Instant::now() < deadline, "incremental pass finishes")?;
            std::thread::sleep(Duration::from_millis(50));
        }
        fixture_call(&suffix, Command::Shutdown).ctx("shutdown")?;
        wait_exit(&mut child)?;
        let mut restarted = launch(temp.path(), &catalog_path, &suffix)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let snapshot = loop {
            if let Ok(snapshot) = fixture_call(&suffix, Command::Snapshot) {
                break snapshot;
            }
            check(Instant::now() < deadline, "restart pipe starts")?;
            std::thread::sleep(Duration::from_millis(20));
        };
        check_eq(
            snapshot.jobs.first().ctx("retained job")?.phase,
            Phase::Completed,
            "completed history survives restart",
        )?;
        fixture_call(&suffix, Command::Shutdown).ctx("restart shutdown")?;
        wait_exit(&mut restarted)?;
        check_eq(
            std::fs::read(payload).ctx("final bytes")?,
            original,
            "restart leaves logical bytes unchanged",
        )
    }
}
