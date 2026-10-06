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
const VERSION: u32 = 1;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Snapshot,
    Enqueue { game: Game, restore: bool },
    Pause { id: u64, paused: bool },
    Cancel(u64),
    Retry(u64),
    Refresh,
    Maintenance(bool),
    Settings(Preferences),
    Shutdown,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub stopping: bool,
    pub epoch: u64,
    pub revision: u64,
    pub jobs: Vec<Job>,
    pub games: Vec<Game>,
    pub warnings: Vec<String>,
    pub artwork_roots: Vec<PathBuf>,
    pub busy: Option<String>,
    pub discovering: bool,
    pub maintenance_paused: bool,
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
fn call_file(command: Command, mut file: std::fs::File) -> Result<Snapshot> {
    crate::windows_ipc::send(
        &mut file,
        &Request {
            version: VERSION,
            command,
        },
    )?;
    let response: Response = crate::windows_ipc::receive(&mut file)?;
    // The server retains its buffers until the client has read the complete reply.
    crate::windows_ipc::send(&mut file, &true)?;
    ensure!(
        response.version == VERSION,
        "Worker protocol changed. Restart Flummox."
    );
    if let Some(error) = response.error {
        anyhow::bail!(error);
    }
    Ok(response.snapshot)
}
pub fn poll() -> Result<Snapshot> {
    call(Command::Snapshot)
}
fn call(command: Command) -> Result<Snapshot> {
    call_file(command, crate::windows_ipc::connect()?)
}
pub fn request(command: Command) -> Result<Snapshot> {
    if let Ok(file) = crate::windows_ipc::connect() {
        return call_file(command, file);
    }
    if matches!(command, Command::Shutdown) {
        return call(command);
    }
    use std::os::windows::process::CommandExt;
    std::process::Command::new(std::env::current_exe()?)
        .arg("--native-coordinator")
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(file) = crate::windows_ipc::connect() {
            return call_file(command, file);
        }
    }
    call(command)
}
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
        if lock.try_lock().is_ok() {
            cleanup_startup(&args, &root)?;
            return Ok(true);
        }
        call(Command::Shutdown)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while lock.try_lock().is_err() {
            ensure!(
                Instant::now() < deadline,
                "The worker is finishing a file. Wait before upgrading or uninstalling."
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        cleanup_startup(&args, &root)?;
        return Ok(true);
    }
    Ok(false)
}
fn cleanup_startup(args: &[std::ffi::OsString], root: &std::path::Path) -> Result<()> {
    if args.iter().any(|arg| arg == "--remove-owned-startup") {
        crate::windows_launchers::startup(false)?;
        let mut preferences = Preferences::load(root)?;
        preferences.start_at_login = false;
        preferences.save(root)?;
    }
    Ok(())
}
struct Active {
    id: u64,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    events: mpsc::Receiver<Event>,
    thread: Option<std::thread::JoinHandle<()>>,
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
enum Event {
    Progress(Progress),
    Finished(std::result::Result<String, String>),
}
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
    let mut queue = Queue::load(&root)?;
    queue.save(&root)?;
    let mut pipe = crate::windows_ipc::listener()?;
    let (requests, receive) = mpsc::channel::<(Request, mpsc::Sender<Response>)>();
    let stopped = Arc::new(AtomicBool::new(false));
    let listener_stop = stopped.clone();
    let listener_failed = Arc::new(AtomicBool::new(false));
    let pipe_failed = listener_failed.clone();
    let listener_thread = std::thread::spawn(move || {
        while !listener_stop.load(Ordering::Relaxed) {
            match crate::windows_ipc::accept(&pipe) {
                Ok(true) => {
                    if let Ok(request) = crate::windows_ipc::receive::<Request>(&mut pipe) {
                        let (send, reply) = mpsc::channel();
                        if requests.send((request, send)).is_ok()
                            && let Ok(response) = reply.recv_timeout(Duration::from_secs(3))
                            && crate::windows_ipc::send(&mut pipe, &response).is_ok()
                        {
                            let _acknowledged = crate::windows_ipc::receive::<bool>(&mut pipe);
                        }
                    }
                    crate::windows_ipc::disconnect(&pipe);
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
    let mut preferences = Preferences::load(&root)?;
    let mut preferences_healthy = true;
    if preferences.start_at_login {
        crate::windows_launchers::startup(true)?;
    }
    let tray = tray()
        .map_err(|error| {
            tracing::warn!(%error, "Worker tray unavailable");
            error
        })
        .ok();
    let mut active: Option<Active> = None;
    let mut shutdown = false;
    let mut queue_dirty = false;
    let mut last_scan = Instant::now() - Duration::from_secs(60);
    let mut last_activity = Instant::now() - Duration::from_secs(60);
    let mut last_save = Instant::now();
    let mut last_preferences = Instant::now() - Duration::from_secs(60);
    let mut discovery: Option<
        mpsc::Receiver<std::result::Result<crate::desktop_discovery::Catalog, String>>,
    > = None;
    loop {
        ensure!(
            !listener_failed.load(Ordering::Relaxed),
            "The coordinator pipe failed; work stopped safely"
        );
        if last_preferences.elapsed() >= Duration::from_secs(1) {
            last_preferences = Instant::now();
            match Preferences::load(&root) {
                Ok(settings) => {
                    preferences = settings;
                    preferences_healthy = true;
                }
                Err(error) => {
                    preferences_healthy = false;
                    snapshot.busy = Some(format!("Preferences unavailable: {error}"));
                }
            }
        }
        if !shutdown
            && discovery.is_none()
            && last_scan.elapsed() >= Duration::from_secs(if active.is_some() { 3 } else { 30 })
        {
            last_scan = Instant::now();
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
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Discovery worker stopped unexpectedly".into()))
                }
            });
        if let Some(result) = discovered {
            discovery = None;
            snapshot.discovering = false;
            match result {
                Ok(catalog) => {
                    snapshot.games = catalog.games;
                    snapshot.artwork_roots = catalog.artwork_roots;
                    snapshot.warnings = catalog.warnings;
                    for game in
                        queue.observe(&snapshot.games, &preferences, snapshot.warnings.is_empty())
                    {
                        let observed = game.clone();
                        match queue.enqueue_automatic(game) {
                            Ok(id) => {
                                if queue
                                    .jobs
                                    .iter()
                                    .find(|job| job.id == id)
                                    .is_some_and(|job| job.game.build == observed.build)
                                {
                                    queue.acknowledge(&observed);
                                }
                            }
                            Err(error) => snapshot.warnings.push(error.to_string()),
                        }
                    }
                    queue.save(&root)?;
                }
                Err(error) => snapshot.warnings = vec![error],
            }
        }
        if last_activity.elapsed() >= Duration::from_secs(1) {
            last_activity = Instant::now();
            let mut games = snapshot.games.clone();
            games.extend(
                queue
                    .jobs
                    .iter()
                    .filter(|job| job.phase.active())
                    .map(|job| job.game.clone()),
            );
            snapshot.busy = Some("Checking running games".into());
            snapshot.busy = activity(&games).unwrap_or_else(|error| Some(error.to_string()));
            if !preferences_healthy {
                snapshot.busy = Some("Desktop preferences need attention".into());
            }
        }
        if let Some((_, actions)) = &tray {
            for action in actions.try_iter() {
                match action {
                    crate::windows_tray::Action::Open => {
                        let executable = std::env::current_exe()?.with_file_name("flummox-gui.exe");
                        if let Err(error) = std::process::Command::new(executable).spawn() {
                            tracing::warn!(%error, "Flummox window could not open");
                        }
                    }
                    crate::windows_tray::Action::Pause => {
                        preferences.maintenance_paused = true;
                        preferences.save(&root)?;
                    }
                    crate::windows_tray::Action::Resume => {
                        preferences.maintenance_paused = false;
                        preferences.save(&root)?;
                    }
                    crate::windows_tray::Action::Exit => {
                        shutdown = true;
                        if let Some(running) = &active {
                            running.cancel.store(true, Ordering::Relaxed);
                        }
                        for job in &mut queue.jobs {
                            if job.phase == Phase::Waiting {
                                job.phase = Phase::Cancelled;
                            }
                        }
                        queue.save(&root)?;
                    }
                }
            }
        }
        for (request, reply) in receive.try_iter().take(64) {
            let result = (|| -> Result<()> {
                ensure!(
                    request.version == VERSION,
                    "Worker protocol changed. Restart Flummox."
                );
                match request.command {
                    Command::Snapshot => {}
                    Command::Refresh => last_scan = Instant::now() - Duration::from_secs(60),
                    Command::Enqueue { mut game, restore } => {
                        game.install_dir = game.install_dir.canonicalize()?;
                        ensure!(
                            game.install_dir.is_dir() && game.install_dir.parent().is_some(),
                            "Choose an installed game folder"
                        );
                        ensure!(
                            restore
                                || !game
                                    .ids()
                                    .any(|id| preferences.excluded.contains(&id.to_string())),
                            "This game is excluded from jobs"
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
                            job.message = "Stopping after the current file".into();
                        } else if job.phase.active() {
                            job.phase = Phase::Cancelled;
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
                        settings.maintenance_paused = preferences.maintenance_paused;
                        if settings.start_at_login != preferences.start_at_login {
                            crate::windows_launchers::startup(settings.start_at_login)?;
                        }
                        settings.save(&root)?;
                        preferences = settings;
                        last_scan = Instant::now() - Duration::from_secs(60);
                    }
                    Command::Shutdown => {
                        shutdown = true;
                        if let Some(running) = &active {
                            running.cancel.store(true, Ordering::Relaxed);
                        }
                        for job in &mut queue.jobs {
                            if job.phase == Phase::Waiting {
                                job.phase = Phase::Cancelled;
                            }
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
                    job.message = "Excluded from background work".into();
                    queue_dirty = true;
                }
            }
        }
        let mut finished = false;
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
                "Background work paused".into()
            } else if !running.drive_online {
                "Reconnect the original drive".into()
            } else if let Some(reason) = &snapshot.busy {
                reason.clone()
            } else if unavailable {
                "Waiting for the launcher".into()
            } else if job.automatic && !snapshot.warnings.is_empty() {
                "Discovery needs attention".into()
            } else if running.cancel.load(Ordering::Relaxed) {
                "Stopping after the current file".into()
            } else {
                "Processing files".into()
            };
            queue_dirty |= before != (job.phase, job.message.clone());
            for _ in 0..256 {
                let event = match running.events.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if !finished {
                            finished = true;
                            job.phase = Phase::Failed;
                            job.message =
                                "Storage worker stopped unexpectedly. Review recovery.".into();
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
        if finished {
            active = None;
            queue.save(&root)?;
        }
        if active.is_none() && shutdown {
            stopped.store(true, Ordering::Relaxed);
            return Ok(());
        }
        let recovery_pending =
            if active.is_none() && queue.jobs.iter().any(|job| job.phase == Phase::Waiting) {
                !crate::windows::recovery()?.is_empty()
            } else {
                false
            };
        for job in queue
            .jobs
            .iter_mut()
            .filter(|job| job.phase == Phase::Waiting)
        {
            let message = if job.user_paused {
                "Paused by you".into()
            } else if preferences.maintenance_paused {
                "Background work paused".into()
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
                "Review interrupted storage before maintenance".into()
            } else if job.automatic && !snapshot.warnings.is_empty() {
                "Discovery needs attention".into()
            } else if snapshot.discovering {
                "Checking installed games".into()
            } else if active.is_some() {
                "Waiting for the current job".into()
            } else {
                "Waiting to start".into()
            };
            if job.message != message {
                job.message = message;
                queue_dirty = true;
            }
        }
        if !shutdown
            && active.is_none()
            && snapshot.busy.is_none()
            && !preferences.maintenance_paused
            && !snapshot.discovering
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
            job.phase = Phase::Running;
            job.message = "Preparing files".into();
            queue.save(&root)?;
            let job = queue
                .jobs
                .iter()
                .find(|job| job.phase == Phase::Running)
                .context("Starting job disappeared")?;
            active = Some(start(job));
        }
        if let Some((tray, _)) = &tray {
            tray.set_paused(preferences.maintenance_paused);
        }
        if queue_dirty && last_save.elapsed() >= Duration::from_secs(1) {
            last_save = Instant::now();
            queue.save(&root)?;
            queue_dirty = false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

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
    crate::windows_tray::Tray,
    mpsc::Receiver<crate::windows_tray::Action>,
)> {
    #[cfg(test)]
    if std::env::var_os("FLUMMOX_TEST_CATALOG").is_some() {
        anyhow::bail!("Tray disabled in the isolated coordinator fixture");
    }
    crate::windows_tray::spawn()
}

fn activity(games: &[Game]) -> Result<Option<String>> {
    #[cfg(test)]
    if std::env::var_os("FLUMMOX_TEST_CATALOG").is_some() {
        return Ok(None);
    }
    crate::windows_activity::busy(games)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _killed = self.0.kill();
            let _reaped = self.0.wait();
        }
    }
    #[test]
    #[ignore = "Subprocess entrypoint for the isolated coordinator test"]
    fn helper() -> TestResult {
        if std::env::var_os("FLUMMOX_TEST_CATALOG").is_none() {
            return Ok(());
        }
        run().map_err(|error| error.to_string())
    }
    fn launch(
        base: &std::path::Path,
        catalog: &std::path::Path,
        suffix: &str,
    ) -> std::result::Result<Child, String> {
        std::process::Command::new(std::env::current_exe().ctx("test executable")?)
            .args([
                "--exact",
                "windows_coordinator::tests::helper",
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
        call_file(command, crate::windows_ipc::test_connect(suffix)?)
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
        let mut pipe = crate::windows_ipc::test_connect(&suffix).ctx("version test")?;
        crate::windows_ipc::send(
            &mut pipe,
            &Request {
                version: VERSION + 1,
                command: Command::Cancel(id),
            },
        )
        .ctx("bad version request")?;
        let response: Response =
            crate::windows_ipc::receive(&mut pipe).ctx("bad version response")?;
        crate::windows_ipc::send(&mut pipe, &true).ctx("rejected request acknowledgement")?;
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
