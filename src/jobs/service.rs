//! Local IPC, durable queue ownership, and worker supervision.

use super::client::*;
use super::packs::*;
use super::*;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::io::{BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The one running worker child process and its pipes.
struct Active {
    /// Id of the job the worker runs.
    id: i64,
    child: Child,
    input: ChildStdin,
    /// Events a reader thread parses from the child's stdout.
    events: mpsc::Receiver<WorkerEvent>,
    started: Instant,
    /// The pause state last sent to the worker.
    paused: bool,
}

/// Logs a failed per-pass step and carries on, so that one bad write does
/// not end the coordinator and unmount every game. The same message is
/// logged at most once every ten seconds, because a pass repeats every 50 ms.
fn survive<T>(what: &str, result: Result<T>) -> Option<T> {
    static LAST: std::sync::Mutex<Option<(String, Instant)>> = std::sync::Mutex::new(None);
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            let line = format!("{what}: {error:#}");
            if let Ok(mut last) = LAST.lock()
                && last.as_ref().is_none_or(|(text, at)| {
                    *text != line || at.elapsed() >= Duration::from_secs(10)
                })
            {
                eprintln!("{line}");
                *last = Some((line, Instant::now()));
            }
            None
        }
    }
}

/// Writes one control line to a worker's stdin and flushes it.
fn send_control(input: &mut impl Write, control: Control) -> Result<()> {
    serde_json::to_writer(&mut *input, &control)?;
    input.write_all(b"\n")?;
    input.flush()?;
    Ok(())
}

/// Whether any of the game's ids is on the exclusion list.
fn is_excluded(game: &Game, excluded: &[String]) -> bool {
    game.ids().any(|id| excluded.contains(&id.to_string()))
}

/// The latest discovered record for the folder `game` is installed in. A
/// manually added game missing from discovery stands for itself while its
/// folder exists. `None` means the game cannot be worked on now.
fn current_game<'a>(game: &'a Game, games: &'a [Game]) -> Option<&'a Game> {
    games
        .iter()
        .find(|current| current.install_dir == game.install_dir)
        .or_else(|| {
            (game.id.launcher == crate::model::Launcher::Manual && game.install_dir.is_dir())
                .then_some(game)
        })
}

/// What maintenance last saw of each game id: its build, whether it was idle,
/// and whether its library had maintenance on. Stored as settings row 2.
#[derive(Default, Serialize, Deserialize)]
struct Observations(std::collections::HashMap<String, (Option<String>, bool, bool)>);

impl Observations {
    /// Records the game's current state and returns whether to queue
    /// compression for it. That takes an idle game in an automatic library
    /// that is new, has a new build, or was not idle last time. A game last
    /// seen with maintenance off only gets a baseline, as does every game
    /// while `initialized` is false.
    fn observe(&mut self, game: &Game, libraries: &[Library], initialized: bool) -> bool {
        let enabled = libraries
            .iter()
            .any(|l| l.automatic && game.install_dir.starts_with(&l.path));
        let stamp = (game.build.clone(), game.state.is_idle(), enabled);
        let old = self.0.insert(game.id.to_string(), stamp.clone());
        enabled
            && stamp.1
            && !game.is_tool
            && initialized
            && old.is_none_or(|old| old.2 && (old.0 != stamp.0 || !old.1))
    }

    /// Records every game as seen now, so none of them queues work later
    /// for a change that had already happened.
    fn baseline(&mut self, games: &[Game], libraries: &[Library]) {
        for game in games {
            let _changed = self.observe(game, libraries, false);
        }
    }
}

/// What automatic upkeep remembers about one Maximum Space install, keyed by
/// its game path. Stored as settings row 7 and never sent to clients.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Upkeep {
    /// The game's build when updates were last folded into the store.
    build: Option<String>,
    /// The game has been seen running since then.
    played: bool,
}

/// Update-layer size that counts as a game update when no build number says
/// so: this many bytes, or a twentieth of the store if that is more. Games
/// that keep saves in their own folder stay well under it.
const UPKEEP_LAYER_BYTES: u64 = 256 * 1024 * 1024;

/// The upkeep a confirmed, mounted install is due, if any.
///
/// Nothing is due while the original from activation is kept, because the
/// user has not yet confirmed the game runs from its store. After that,
/// updates are folded in when the build changed or the update layer grew
/// large, and the previous version is deleted once the folded-in one has
/// been played. The first sight of an install only records its build.
fn upkeep_due(
    install: &crate::pack::Install,
    build: Option<&str>,
    layer_bytes: u64,
    record: &mut Upkeep,
) -> Option<PackTask> {
    if install.phase != crate::pack::InstallPhase::Mounted || install.backup_path.is_some() {
        return None;
    }
    if install.previous_store_path.is_some() {
        return record.played.then_some(PackTask::Prune);
    }
    let store = install
        .summary
        .as_ref()
        .map_or(0, |summary| summary.archive_bytes);
    let large = layer_bytes >= UPKEEP_LAYER_BYTES.max(store / 20);
    let rebuilt = match (&record.build, build) {
        (Some(recorded), Some(current)) => recorded != current && layer_bytes > 0,
        _ => false,
    };
    if record.build.is_none() {
        record.build = build.map(str::to_owned);
    }
    (large || rebuilt).then_some(PackTask::Compact)
}

/// Bytes of regular files in an update layer's upper tree. An unreadable
/// layer counts as empty, which only delays upkeep.
fn layer_bytes(writes: &Path) -> u64 {
    walkdir::WalkDir::new(writes.join("files"))
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok())
        .fold(0u64, |total, metadata| total.saturating_add(metadata.len()))
}

/// Queues the upkeep each install is due and records what was decided.
///
/// A task is not queued while its game is busy or has a job waiting, or
/// while the last attempt at the same task needs attention. That last rule
/// keeps a failing task from being queued again every scan.
fn run_upkeep(
    snapshot: &mut Snapshot,
    games: &[Game],
    records: &mut std::collections::HashMap<String, Upkeep>,
    db: &Connection,
) {
    let installs = snapshot.packs.clone();
    records.retain(|path, _| {
        installs
            .iter()
            .any(|install| install.game_path.to_string_lossy() == *path)
    });
    for install in &installs {
        let Some(game) = games
            .iter()
            .find(|game| game.install_dir == install.game_path)
        else {
            continue;
        };
        let record = records
            .entry(install.game_path.to_string_lossy().into_owned())
            .or_default();
        let Some(task) = upkeep_due(
            install,
            game.build.as_deref(),
            layer_bytes(&install.writes_path),
            record,
        ) else {
            continue;
        };
        let jobs = || {
            snapshot
                .jobs
                .iter()
                .rev()
                .filter(|job| job.game.install_dir == game.install_dir)
        };
        let stuck = jobs()
            .find(|job| job.pack.as_ref() == Some(&task))
            .is_some_and(|job| {
                matches!(
                    job.phase,
                    Phase::Failed | Phase::Partial | Phase::Interrupted
                )
            });
        if !game.state.is_idle() || stuck || jobs().any(|job| job.phase.active()) {
            continue;
        }
        let compacting = task == PackTask::Compact;
        match enqueue_job(
            snapshot,
            game.clone(),
            Operation::Pack,
            CompressOpts::default(),
            Some(task),
            db,
        ) {
            Ok(()) if compacting => {
                record.build.clone_from(&game.build);
                record.played = false;
            }
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(%error, game = %game.title, "store upkeep was not queued");
            }
        }
    }
}

/// Writes one job to the queue table, replacing its previous row.
fn save(db: &Connection, job: &Job) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO queue(id, data) VALUES(?1, ?2)",
        params![job.id, serde_json::to_string(job)?],
    )?;
    Ok(())
}

/// Writes the libraries and exclusions as settings row 1.
fn settings(db: &Connection, snapshot: &Snapshot) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO settings(id, data) VALUES(1, ?1)",
        [serde_json::to_string(&(
            snapshot.libraries.clone(),
            snapshot.excluded.clone(),
        ))?],
    )?;
    Ok(())
}

/// The directories of a Steam library that receive installs and downloads.
/// Only the ones that exist are returned.
fn steam_write_dirs(library: &Path) -> Vec<PathBuf> {
    let steamapps = library.join("steamapps");
    ["", "common", "downloading", "temp"]
        .iter()
        .map(|name| {
            if name.is_empty() {
                steamapps.clone()
            } else {
                steamapps.join(name)
            }
        })
        .filter(|path| path.is_dir())
        .collect()
}

/// Xattr set on a directory whose compression property Flummox turned on.
const LIVE_COMPRESSION_MARKER: &str = "user.flummox.live-compression";

/// Turns the btrfs compression property on `dir` on or off. A property that
/// was already set without the marker belongs to the user: it is neither
/// marked when enabling nor cleared when disabling.
fn set_live_compression(dir: &Path, enabled: bool) -> Result<()> {
    let owned = xattr::get(dir, LIVE_COMPRESSION_MARKER)?.is_some();
    if enabled {
        if crate::backend::btrfs::dir_property(dir)?.is_some() {
            return Ok(());
        }
        crate::backend::btrfs::set_dir_property(dir, true)?;
        if let Err(error) = xattr::set(dir, LIVE_COMPRESSION_MARKER, b"1") {
            let _cleared = crate::backend::btrfs::set_dir_property(dir, false);
            return Err(error.into());
        }
    } else if owned {
        crate::backend::btrfs::set_dir_property(dir, false)?;
        xattr::remove(dir, LIVE_COMPRESSION_MARKER)?;
    }
    Ok(())
}

/// Applies the library's `automatic` setting to its Steam write directories,
/// so new downloads land compressed. Does nothing when the library has no
/// `steamapps` directories or is not on btrfs.
fn update_live_compression(library: &Library) -> Result<()> {
    let dirs = steam_write_dirs(&library.path);
    if dirs.is_empty() {
        return Ok(());
    }
    let info = crate::fsprobe::probe(&library.path)?;
    if !matches!(
        crate::fsprobe::tier_for(&info),
        crate::fsprobe::Tier::Native(crate::fsprobe::BackendKind::Btrfs)
    ) {
        return Ok(());
    }
    for dir in dirs {
        set_live_compression(&dir, library.automatic)
            .with_context(|| format!("Updating live compression for {}", dir.display()))?;
    }
    Ok(())
}

/// Opens or creates the queue database and loads the state it holds. A job
/// that was running, pausing, paused or cancelling is marked `Interrupted`.
/// Queued jobs stay queued.
fn open_store(path: &Path) -> Result<(Connection, Snapshot)> {
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    // Settings rows: 1 libraries and exclusions, 2 maintenance observations,
    // 3 reduced motion, 4 pack installs, 5 theme, 6 motion, 7 store upkeep.
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
        CREATE TABLE IF NOT EXISTS queue(id INTEGER PRIMARY KEY, data TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS settings(id INTEGER PRIMARY KEY, data TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS receipts(game TEXT NOT NULL, path TEXT NOT NULL, policy TEXT NOT NULL, entry TEXT NOT NULL, PRIMARY KEY(game,path));")?;
    let mut snapshot = Snapshot::default();
    // Load the newest 500 jobs, then reverse them into oldest-first order.
    let mut stmt = db.prepare("SELECT data FROM queue ORDER BY id DESC LIMIT 500")?;
    for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
        let mut job: Job = serde_json::from_str(&row?)?;
        // A job the user paused before it ever started has no work to lose.
        // It stays paused and starts when resumed.
        let never_started = job.phase == Phase::Paused
            && job.user_paused
            && job.files_total == 0
            && job.elapsed == 0;
        if job.phase.active() && job.phase != Phase::Queued && !never_started {
            job.phase = Phase::Interrupted;
            job.message = "Work was interrupted. Resume to finish the remaining files.".into();
            save(&db, &job)?;
        }
        snapshot.jobs.push(job);
    }
    drop(stmt);
    snapshot.jobs.reverse();
    let mut stmt = db.prepare("SELECT data FROM settings WHERE id=1")?;
    if let Some(row) = stmt.query_map([], |r| r.get::<_, String>(0))?.next() {
        (snapshot.libraries, snapshot.excluded) = serde_json::from_str(&row?)?;
    }
    drop(stmt);
    snapshot.reduced_motion = db
        .query_row("SELECT data FROM settings WHERE id=3", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .is_some_and(|value| value == "true");
    snapshot.theme = db
        .query_row("SELECT data FROM settings WHERE id=5", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default();
    // Row 6 decides motion. A database that has only the older row 3 maps it
    // to Reduced or Expressive, and `reduced_motion` then follows `motion`.
    snapshot.motion = db
        .query_row("SELECT data FROM settings WHERE id=6", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or({
            if snapshot.reduced_motion {
                super::MotionPreference::Reduced
            } else {
                super::MotionPreference::Expressive
            }
        });
    snapshot.reduced_motion = snapshot.motion == super::MotionPreference::Reduced;
    snapshot.packs = db
        .query_row("SELECT data FROM settings WHERE id=4", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .map(|value| serde_json::from_str(&value))
        .transpose()?
        .unwrap_or_default();
    Ok((db, snapshot))
}

/// Queues an analysis, compression or decompression job.
fn enqueue(
    snapshot: &mut Snapshot,
    game: Game,
    operation: Operation,
    options: CompressOpts,
    db: &Connection,
) -> Result<()> {
    ensure!(
        operation != Operation::Pack,
        "Pack jobs require a storage task"
    );
    enqueue_job(snapshot, game, operation, options, None, db)
}

/// Validates a request and appends it to the queue and the database. Returns
/// `Ok` without queueing when an active job already covers the same folder,
/// operation and storage task. Refuses excluded games, a folder that contains
/// or lies inside another discovered game, and a queue of 200 active jobs.
fn enqueue_job(
    snapshot: &mut Snapshot,
    game: Game,
    operation: Operation,
    options: CompressOpts,
    pack: Option<PackTask>,
    db: &Connection,
) -> Result<()> {
    let path = validate_folder(&game.install_dir)?;
    ensure!(
        (1..=32).contains(&options.threads),
        "Choose between 1 and 32 worker threads."
    );
    ensure!(
        options
            .level
            .is_none_or(|level| (-15..=15).contains(&level)),
        "btrfs levels range from -15 to 15."
    );
    ensure!(
        !snapshot
            .excluded
            .iter()
            .any(|id| game.ids().any(|g| g.to_string() == *id)),
        "This game is excluded. Restore it in Drives first."
    );
    if let Some(existing) = snapshot
        .jobs
        .iter()
        .find(|j| j.phase.active() && j.game.install_dir == path && j.operation == operation)
    {
        // A storage job for the same folder with another task is a different
        // request, so it is refused rather than reported as queued.
        ensure!(
            existing.pack == pack,
            "{} is already waiting or running for this game. Wait for it to finish first.",
            existing.pack.as_ref().map_or("A storage task", PackTask::label)
        );
        return Ok(());
    }
    if let Some(other) = snapshot.discovered.iter().find(|other| {
        let theirs = other
            .install_dir
            .canonicalize()
            .unwrap_or_else(|_| other.install_dir.clone());
        theirs != path && (theirs.starts_with(&path) || path.starts_with(&theirs))
    }) {
        bail!(
            "This folder overlaps {}. Choose that game's own folder instead.",
            other.title
        );
    }
    ensure!(
        snapshot.jobs.iter().filter(|j| j.phase.active()).count() < 200,
        "The queue is full. Let some jobs finish first."
    );
    let mut game = game;
    game.install_dir = path;
    let id: i64 = db.query_row("SELECT COALESCE(MAX(id),0)+1 FROM queue", [], |r| r.get(0))?;
    let job = Job {
        id,
        game,
        operation,
        options,
        phase: Phase::Queued,
        files_done: 0,
        bytes_done: 0,
        files_total: 0,
        bytes_total: 0,
        estimate: None,
        message: "Queued".into(),
        errors: vec![],
        created: now(),
        elapsed: 0,
        drive_change: None,
        user_paused: false,
        pack,
        pack_interruptible: true,
        space_plan: None,
    };
    save(db, &job)?;
    snapshot.jobs.push(job);
    // Keep the newest 300 finished jobs and delete older ones from both the
    // snapshot and the database.
    let old: Vec<_> = snapshot
        .jobs
        .iter()
        .rev()
        .filter(|job| !job.phase.active())
        .skip(300)
        .map(|job| job.id)
        .collect();
    snapshot.jobs.retain(|job| !old.contains(&job.id));
    for id in old {
        db.execute("DELETE FROM queue WHERE id=?1", [id])?;
    }
    Ok(())
}

/// Cancels the running worker when its job is an analysis, so that work the
/// user requested does not wait behind it.
fn preempt_analysis(
    snapshot: &mut Snapshot,
    db: &Connection,
    active: &mut Option<Active>,
) -> Result<()> {
    if let Some(worker) = active.as_mut()
        && let Some(job) = snapshot
            .jobs
            .iter_mut()
            .find(|job| job.id == worker.id && job.operation == Operation::Analyze)
    {
        send_control(&mut worker.input, Control::Cancel)?;
        job.phase = Phase::Cancelling;
        job.message = "Making room for your requested job".into();
        save(db, job)?;
    }
    Ok(())
}

/// Applies one client command to the snapshot and the database.
/// `running_pack` is the id of the job on the storage thread, if any. An
/// error is sent to the client as the reply.
fn apply(
    command: Command,
    snapshot: &mut Snapshot,
    db: &Connection,
    active: &mut Option<Active>,
    mounts: &mut Vec<PackMount>,
    running_pack: Option<i64>,
) -> Result<()> {
    match command {
        // Rechecks the plan against the drives as they are now, applies the
        // inner command, then stores the plan on the newest active job for
        // that folder. The worker rechecks it again before it starts.
        Command::EnqueuePlanned { command, plan } => {
            ensure!(
                matches!(
                    &*command,
                    Command::Enqueue {
                        operation: Operation::Compress | Operation::Decompress,
                        ..
                    } | Command::EnqueuePack { .. }
                ),
                "Space plans apply only to storage jobs"
            );
            plan.recheck()?;
            let (path, operation) = match &*command {
                Command::Enqueue {
                    game, operation, ..
                } => (game.install_dir.clone(), *operation),
                Command::EnqueuePack { game, .. } => (game.install_dir.clone(), Operation::Pack),
                _ => bail!("Storage command is missing"),
            };
            apply(*command, snapshot, db, active, mounts, running_pack)?;
            // The enqueue may have joined an existing job. The plan belongs
            // to the job for this operation, not to whichever job for the
            // folder is newest.
            if let Some(job) = snapshot.jobs.iter_mut().rev().find(|job| {
                job.game.install_dir == path && job.operation == operation && job.phase.active()
            }) {
                job.space_plan = Some(plan);
                save(db, job)?;
            }
        }
        // `run` handles these, since they act on loop state this function
        // does not receive.
        Command::Snapshot
        | Command::Restart
        | Command::RefreshDiscovery
        | Command::CancelDiscovery => {}
        // The motion arms write rows 3 and 6 together, so the older boolean
        // and the newer preference always agree.
        Command::ReducedMotion(value) => {
            db.execute(
                "INSERT OR REPLACE INTO settings(id,data) VALUES(3,?1)",
                [value.to_string()],
            )?;
            snapshot.reduced_motion = value;
            snapshot.motion = if value {
                super::MotionPreference::Reduced
            } else {
                super::MotionPreference::Expressive
            };
            db.execute(
                "INSERT OR REPLACE INTO settings(id,data) VALUES(6,?1)",
                [serde_json::to_string(&snapshot.motion)?],
            )?;
        }
        Command::Theme(value) => {
            db.execute(
                "INSERT OR REPLACE INTO settings(id,data) VALUES(5,?1)",
                [serde_json::to_string(&value)?],
            )?;
            snapshot.theme = value;
        }
        Command::Motion(value) => {
            db.execute(
                "INSERT OR REPLACE INTO settings(id,data) VALUES(6,?1)",
                [serde_json::to_string(&value)?],
            )?;
            snapshot.motion = value;
            snapshot.reduced_motion = value == super::MotionPreference::Reduced;
            db.execute(
                "INSERT OR REPLACE INTO settings(id,data) VALUES(3,?1)",
                [snapshot.reduced_motion.to_string()],
            )?;
        }
        // Requested storage work cancels a running analysis. A queued
        // analysis never preempts anything.
        Command::Enqueue {
            game,
            operation,
            options,
        } => {
            enqueue(snapshot, game, operation, options, db)?;
            if operation != Operation::Analyze {
                preempt_analysis(snapshot, db, active)?;
            }
        }
        Command::EnqueuePack { game, task } => {
            ensure!(
                cfg!(feature = "pack-mount"),
                "Maximum Space requires a build with pack mounting"
            );
            enqueue_job(
                snapshot,
                game,
                Operation::Pack,
                CompressOpts::default(),
                Some(task),
                db,
            )?;
            preempt_analysis(snapshot, db, active)?;
        }
        // Sets `user_paused`. The phase changes here only for a job nothing
        // is running. For a running job the loop reads the flag and tells the
        // worker or storage thread, which reports the phase back.
        Command::Pause { id, paused } => {
            let job = snapshot
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .context("Job no longer exists")?;
            // The window redraws once a second, so a Pause can arrive for a
            // job that has just finished. That is not an error to report: the
            // reply carries the snapshot that shows the job as done.
            if !job.phase.active() {
                return Ok(());
            }
            job.user_paused = paused;
            if job.phase == Phase::Queued
                || (job.phase == Phase::Paused
                    && active.as_ref().is_none_or(|a| a.id != id)
                    && running_pack != Some(id))
            {
                job.phase = if paused { Phase::Paused } else { Phase::Queued };
            }
            save(db, job)?;
        }
        // A running job becomes `Cancelling` and reaches `Cancelled` when its
        // worker or storage thread stops. A job nothing is running is
        // cancelled at once. A finished job is left as it is.
        Command::Cancel(id) => {
            let job = snapshot
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .context("Job no longer exists")?;
            if let Some(a) = active.as_mut().filter(|a| a.id == id) {
                send_control(&mut a.input, Control::Cancel)?;
                job.phase = Phase::Cancelling;
            } else if running_pack == Some(id) {
                job.phase = Phase::Cancelling;
            } else if job.phase.active() {
                job.phase = Phase::Cancelled;
            }
            save(db, job)?;
        }
        // Queues a new job with a new id and the old job's parameters. The
        // old job stays in history. `enqueue_job` repeats every check, so an
        // excluded game cannot be retried.
        Command::Retry(id) => {
            let job = snapshot
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .context("Job no longer exists")?;
            ensure!(!job.phase.active(), "This job is already queued");
            let game = job.game.clone();
            let operation = job.operation;
            let options = job.options;
            let pack = job.pack.clone();
            enqueue_job(snapshot, game, operation, options, pack, db)?;
        }
        // Replaces the entry for this path. If live compression cannot be
        // updated, the command fails with nothing saved.
        Command::Library(mut library) => {
            library.path = validate_folder(&library.path)?;
            update_live_compression(&library)?;
            snapshot.libraries.retain(|l| l.path != library.path);
            snapshot.libraries.push(library);
            settings(db, snapshot)?;
        }
        // Only a custom location with no active jobs and no activated stores
        // is removed. Live compression is turned off for it first.
        Command::RemoveLibrary(path) => {
            let path = path.canonicalize().unwrap_or(path);
            let library = snapshot
                .libraries
                .iter()
                .find(|l| l.path == path && l.custom)
                .context("This custom location is no longer registered")?
                .clone();
            ensure!(
                !snapshot
                    .jobs
                    .iter()
                    .any(|job| job.phase.active() && job.game.install_dir.starts_with(&path)),
                "Finish or cancel jobs in this location before removing it."
            );
            ensure!(
                !snapshot
                    .packs
                    .iter()
                    .any(|install| install.game_path.starts_with(&path)),
                "Restore Maximum Space games in this location before removing it."
            );
            if path.is_dir() {
                update_live_compression(&Library {
                    automatic: false,
                    ..library
                })?;
            }
            snapshot.libraries.retain(|l| l.path != path);
            settings(db, snapshot)?;
        }
        // The id is removed and, when excluding, added back, so it is listed
        // once. Excluding also cancels every active job for the game.
        Command::Exclude { id, excluded } => {
            snapshot.excluded.retain(|i| i != &id);
            if excluded {
                for job in snapshot
                    .jobs
                    .iter_mut()
                    .filter(|j| j.phase.active() && j.game.ids().any(|game| game.to_string() == id))
                {
                    if let Some(worker) = active.as_mut().filter(|a| a.id == job.id) {
                        send_control(&mut worker.input, Control::Cancel)?;
                        job.phase = Phase::Cancelling;
                    } else if running_pack == Some(job.id) {
                        // A storage thread stops at its next checkpoint, as
                        // it does for Cancel. Marking it Cancelled here would
                        // report a step that must finish as already stopped.
                        job.phase = Phase::Cancelling;
                    } else {
                        job.phase = Phase::Cancelled;
                    }
                    job.message = "Excluded from future work".into();
                    save(db, job)?;
                }
                snapshot.excluded.push(id);
            }
            settings(db, snapshot)?;
        }
        // The direct storage commands run their whole transaction here, on
        // the coordinator thread. `run` has already refused them if a worker
        // or storage thread is running.
        Command::PackActivate {
            game_path,
            store_path,
            writes_path,
        } => pack_activate(snapshot, db, mounts, &game_path, &store_path, &writes_path)?,
        Command::PackRollback { game_path } => {
            pack_rollback(snapshot, db, mounts, &game_path)?;
        }
        Command::PackReclaim { game_path } => {
            pack_reclaim(snapshot, db, &game_path)?;
        }
        Command::PackCompact { game_path } => {
            pack_compact(snapshot, db, mounts, &game_path)?;
        }
        Command::PackPrune { game_path } => {
            pack_prune(snapshot, db, &game_path)?;
        }
    }
    Ok(())
}

/// The key receipts are stored under: the operation and its options, with the
/// thread count left out. Analysis uses the compression key, so it skips
/// files a compression job already finished.
pub(super) fn policy(job: &Job) -> Result<String> {
    // Concurrency does not change a file's compression policy.
    let options = CompressOpts {
        threads: 0,
        ..job.options
    };
    let operation = if job.operation == Operation::Analyze {
        Operation::Compress
    } else {
        job.operation
    };
    Ok(format!(
        "v3:{operation:?}:{}",
        serde_json::to_string(&options)?
    ))
}

/// Drops receipts from a different operation before any rewrite can start.
/// The caller holds the operation lock; `None` invalidates every policy.
pub(super) fn invalidate_receipts(store: &Path, game: &Path, keep: Option<&str>) -> Result<()> {
    if !store.try_exists()? {
        return Ok(());
    }
    let db = Connection::open_with_flags(store, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "synchronous", "FULL")?;
    db.execute(
        "DELETE FROM receipts WHERE game=?1 AND (?2 IS NULL OR policy<>?2)",
        params![game.as_os_str().as_bytes(), keep],
    )?;
    Ok(())
}

/// Tells queued jobs what they are behind when the running job is one the
/// user paused. That job keeps the only slot, and without this the queue
/// looks stuck for no reason.
fn note_waiting(snapshot: &mut Snapshot, running: Option<i64>) {
    let holder = running
        .and_then(|id| snapshot.jobs.iter().find(|job| job.id == id))
        .filter(|job| job.user_paused)
        .map(|job| format!("Waiting for {}, which is paused", job.game.title));
    for job in snapshot
        .jobs
        .iter_mut()
        .filter(|job| job.phase == Phase::Queued)
    {
        match &holder {
            Some(reason) => job.message.clone_from(reason),
            None if job.message.starts_with("Waiting for ") => job.message = "Queued".into(),
            None => {}
        }
    }
}

/// Why a job is held, in the order a user can act on it.
fn pause_reason(by_user: bool, playing: Option<&str>) -> String {
    if by_user {
        "Paused by you".into()
    } else if let Some(game) = playing {
        format!("Paused while you play {game}")
    } else {
        "Waiting for the original drive or launcher activity to finish".into()
    }
}

/// Spawns a worker child, writes the job to its stdin, and starts a thread
/// that turns its stdout lines into events. The worker's stderr is discarded.
fn start(job: &Job) -> Result<Active> {
    let mut child = std::process::Command::new(binary()?)
        .arg("__worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut input = child.stdin.take().context("Worker stdin is unavailable")?;
    serde_json::to_writer(
        &mut input,
        &Work {
            version: VERSION,
            job: job.clone(),
        },
    )?;
    input.write_all(b"\n")?;
    let output = child
        .stdout
        .take()
        .context("Worker output is unavailable")?;
    let (send, events) = mpsc::sync_channel(128);
    // Reader thread. It ends when the receiver is dropped or a read fails.
    // A failed read, which includes the worker closing stdout, is reported
    // as a `Failed` event.
    std::thread::spawn(move || {
        let mut reader = BufReader::new(output);
        loop {
            match read_message(&mut reader) {
                Ok(event) => {
                    if send.send(event).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _sent =
                        send.send(WorkerEvent::Failed(format!("Worker disconnected: {error}")));
                    break;
                }
            }
        }
    });
    Ok(Active {
        id: job.id,
        child,
        input,
        events,
        started: Instant::now(),
        paused: false,
    })
}

/// Applies one worker event to its job. Returns `true` when the event ends
/// the job. A terminal event is saved here; progress is saved by the caller.
fn event(job: &mut Job, event: WorkerEvent, db: &Connection) -> Result<bool> {
    use crate::backend::Event;
    match event {
        WorkerEvent::SpacePlan(plan) => job.space_plan = Some(plan),
        WorkerEvent::Estimate(estimate) => {
            job.estimate = Some(estimate);
        }
        WorkerEvent::Progress(progress) => {
            match progress {
                Event::Started { files, bytes } => {
                    job.files_total = files;
                    job.bytes_total = bytes;
                    job.files_done = 0;
                    job.bytes_done = 0;
                }
                Event::Progress {
                    files_done,
                    bytes_done,
                    current,
                } => {
                    job.files_done = files_done;
                    job.bytes_done = bytes_done;
                    job.message = current;
                }
                // One receipt per finished file, written as it arrives. A
                // retry under the same policy skips files that have one.
                Event::FileCompleted { entry, level } => {
                    db.execute("INSERT OR REPLACE INTO receipts(game,path,policy,entry) VALUES(?1,?2,?3,?4)", params![job.game.install_dir.as_os_str().as_bytes(), entry.rel.as_os_str().as_bytes(), policy(job)?, serde_json::to_string(&(&entry, level))?])?;
                }
                Event::Warning(message) => {
                    if job.errors.len() < 20 {
                        job.errors.push(message);
                    }
                }
                Event::Paused { by } => {
                    job.phase = Phase::Paused;
                    job.message = by;
                }
                Event::Resumed => {
                    job.phase = Phase::Running;
                }
                Event::Finished(_) => {}
            }
        }
        WorkerEvent::Done {
            cancelled,
            errors,
            drive_change,
        } => {
            job.phase = if cancelled {
                Phase::Cancelled
            } else if errors.is_empty() {
                Phase::Completed
            } else {
                Phase::Partial
            };
            job.errors = errors.into_iter().take(20).collect();
            job.drive_change = drive_change;
            job.message = if cancelled {
                "Stopped. Resume to finish the remaining files."
            } else if job.errors.is_empty() {
                "Finished"
            } else {
                "Some files need another attempt."
            }
            .into();
            save(db, job)?;
            return Ok(true);
        }
        // A job that found the operation lock taken goes back in the queue,
        // since the process holding it will finish.
        WorkerEvent::Failed(message) if message.contains(super::LOCK_BUSY) => {
            job.phase = Phase::Queued;
            job.message = "Waiting for another Flummox process to finish".into();
            save(db, job)?;
            return Ok(true);
        }
        WorkerEvent::Failed(message) => {
            job.phase = Phase::Failed;
            job.message = message;
            save(db, job)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Pause, cancel and progress shared between the coordinator loop and the
/// storage thread. The thread reads it at each `checkpoint`.
#[derive(Default)]
pub(super) struct PackControl {
    pub(super) cancel: std::sync::atomic::AtomicBool,
    paused: std::sync::atomic::AtomicBool,
    /// True until the task enters a step that must finish.
    interruptible: std::sync::atomic::AtomicBool,
    /// Progress channel to the coordinator. `None` for a direct transaction.
    events: Option<mpsc::SyncSender<crate::backend::Event>>,
    /// Held while `interruptible` changes and while a control request is
    /// checked against it, so a cancel is either seen by `transaction` or refused.
    transition: std::sync::Mutex<()>,
}

impl PackControl {
    /// Marks the start of a step that must finish and reports `message` as
    /// the stage. Fails if a cancel arrived first. Once it returns,
    /// `request_control` refuses pause and cancel.
    #[cfg(feature = "pack-mount")]
    pub(super) fn transaction(&self, message: &str) -> Result<()> {
        use crate::pack::Observer;
        use std::sync::atomic::Ordering;
        self.checkpoint()?;
        let _transition = self
            .transition
            .lock()
            .map_err(|_| anyhow::anyhow!("Storage control lock stopped"))?;
        self.interruptible.store(false, Ordering::SeqCst);
        ensure!(
            !self.cancel.load(Ordering::SeqCst),
            "Storage job stopped before switching files"
        );
        self.started(0, 0, message);
        Ok(())
    }
    /// Applies a client's pause or cancel to the storage task, or fails once
    /// the task is past the point where it can stop.
    fn request_control(&self, command: &Command) -> Result<()> {
        use std::sync::atomic::Ordering;
        let _transition = self
            .transition
            .lock()
            .map_err(|_| anyhow::anyhow!("Storage control lock stopped"))?;
        ensure!(
            self.interruptible.load(Ordering::SeqCst),
            "The storage switch is finishing; controls return when it is safe"
        );
        match command {
            Command::Cancel(_) => self.cancel.store(true, Ordering::SeqCst),
            Command::Pause { paused, .. } => self.paused.store(*paused, Ordering::SeqCst),
            _ => {}
        }
        Ok(())
    }
    /// Sends a progress event to the coordinator when a channel is attached.
    fn emit(&self, event: crate::backend::Event) {
        if let Some(events) = &self.events {
            let _sent = events.send(event);
        }
    }
}

impl crate::pack::Observer for PackControl {
    // Returns an error once cancelled. While paused it blocks here, polling
    // every 50 ms, and a cancel ends the wait.
    fn checkpoint(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        ensure!(!self.cancel.load(Ordering::Relaxed), "Storage job stopped");
        if self.paused.load(Ordering::Relaxed) {
            self.emit(crate::backend::Event::Paused {
                by: "Paused until resumed or launcher activity finishes".into(),
            });
            while self.paused.load(Ordering::Relaxed) {
                ensure!(!self.cancel.load(Ordering::Relaxed), "Storage job stopped");
                std::thread::sleep(Duration::from_millis(50));
            }
            self.emit(crate::backend::Event::Resumed);
        }
        Ok(())
    }
    fn started(&self, files: u64, bytes: u64, stage: &str) {
        self.emit(crate::backend::Event::Started { files, bytes });
        self.progress(0, 0, stage);
    }
    fn progress(&self, files: u64, bytes: u64, stage: &str) {
        self.emit(crate::backend::Event::Progress {
            files_done: files,
            bytes_done: bytes,
            current: stage.into(),
        });
    }
}

/// What the storage thread hands back when it ends: the install records and
/// mounts it held, and the task's result.
struct PackResult {
    packs: Vec<crate::pack::Install>,
    mounts: Vec<PackMount>,
    result: Result<()>,
}
/// The one running storage thread and the job it works on.
struct PackActive {
    id: i64,
    control: std::sync::Arc<PackControl>,
    events: mpsc::Receiver<crate::backend::Event>,
    thread: std::thread::JoinHandle<PackResult>,
    started: Instant,
}

/// Starts a pack job on its own thread, so the loop keeps answering clients.
/// The thread takes every mount and a copy of the install records, and opens
/// its own database connection. `poll_pack` takes the records and mounts
/// back when the thread ends.
fn start_pack(
    job: &Job,
    packs: &[crate::pack::Install],
    libraries: &[Library],
    mounts: Vec<PackMount>,
    database: &Path,
) -> Result<PackActive> {
    let task = job.pack.clone().context("Storage task is missing")?;
    let expected_plan = job.space_plan.clone();
    let (send, events) = mpsc::sync_channel(128);
    let control = std::sync::Arc::new(PackControl {
        interruptible: std::sync::atomic::AtomicBool::new(true),
        events: Some(send),
        ..Default::default()
    });
    let worker_control = control.clone();
    let game = job.game.clone();
    let database = database.to_path_buf();
    let mut snapshot = Snapshot {
        packs: packs.to_vec(),
        libraries: libraries.to_vec(),
        ..Snapshot::default()
    };
    let thread = std::thread::spawn(move || {
        let mut mounts = mounts;
        let result = (|| {
            let db = Connection::open(database)?;
            db.busy_timeout(Duration::from_secs(5))?;
            // Held for the whole task, like a worker process holds it.
            let _operation = super::operation_lock()?;
            if let Some(plan) = expected_plan {
                plan.recheck()?;
            }
            run_pack_task(
                &task,
                &game,
                &mut snapshot,
                &db,
                &mut mounts,
                &worker_control,
            )?;
            Ok(())
        })();
        PackResult {
            packs: snapshot.packs,
            mounts,
            result,
        }
    });
    Ok(PackActive {
        id: job.id,
        control,
        events,
        thread,
        started: Instant::now(),
    })
}

/// Validates a store path and creates its parent folder, which is returned
/// canonical. The path must be absolute, have no `..` steps, and lie outside
/// the game folder `root`.
#[cfg(feature = "pack-mount")]
fn storage_parent(root: &Path, store: &Path) -> Result<PathBuf> {
    ensure!(
        store.is_absolute() && store.file_name().is_some(),
        "Choose an absolute store path with a name"
    );
    ensure!(
        !store
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir)),
        "The store path cannot contain parent-directory steps"
    );
    let parent = store.parent().context("The store needs a parent folder")?;
    // The nearest existing ancestor is checked before anything is created,
    // and the created parent is checked again after symlinks resolve.
    let ancestor = parent
        .ancestors()
        .find(|path| path.exists())
        .context("The storage drive is unavailable")?
        .canonicalize()?;
    ensure!(
        !ancestor.starts_with(root),
        "Keep the store outside the game folder"
    );
    std::fs::create_dir_all(parent)?;
    let parent = parent.canonicalize()?;
    ensure!(
        !parent.starts_with(root),
        "Keep the store outside the game folder"
    );
    Ok(parent)
}

/// Performs one storage task on the storage thread. The space plan is
/// recomputed and rechecked first. Steps before a `control.transaction` call
/// can pause or stop at checkpoints; steps after it run to the end.
#[cfg(feature = "pack-mount")]
fn run_pack_task(
    task: &PackTask,
    game: &Game,
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
    control: &PackControl,
) -> Result<()> {
    use crate::pack::Observer;
    control.checkpoint()?;
    let plan = super::space_plan(
        &Command::EnqueuePack {
            game: game.clone(),
            task: task.clone(),
        },
        snapshot,
    )?;
    plan.recheck()?;
    match task {
        PackTask::Create { store } => {
            let root = validate_folder(&game.install_dir)?;
            let parent = storage_parent(&root, store)?;
            crate::pack::create_shared_observed(
                &root,
                store,
                &parent.join(".flummox-pool"),
                crate::pack::Options::maximum(),
                &control.cancel,
                control,
            )?;
            Ok(())
        }
        PackTask::Activate {
            store,
            create,
            qualification,
        } => {
            let root = validate_folder(&game.install_dir)?;
            ensure!(
                crate::busy::process_using(&root, &crate::busy::ProcFs::new()).is_none(),
                "Close the game and launcher activity before activating storage"
            );
            // With a qualification report, the installed files are hashed
            // here and again just before the switch. Both hashes must equal
            // the report's corpus.
            if let Some(report) = qualification {
                control.started(0, 0, "Checking game compatibility");
                let corpus = crate::compatibility::corpus(&root, &control.cancel, control)?;
                ensure!(
                    report.corpus == corpus
                        && report.qualifies(
                            game,
                            &corpus.sha256,
                            crate::compatibility::Policy::default()
                        ),
                    "Compatibility no longer matches this game; analyze it again"
                );
            }
            let parent = storage_parent(&root, store)?;
            if *create && !store.exists() {
                let pool = parent.join(".flummox-pool");
                crate::pack::create_shared_observed(
                    &root,
                    store,
                    &pool,
                    crate::pack::Options::maximum(),
                    &control.cancel,
                    control,
                )?;
            }
            // The update layer lives beside the store, at `<store>.writes`.
            let mut writes_name = store.as_os_str().to_os_string();
            writes_name.push(".writes");
            let writes = PathBuf::from(writes_name);
            let install =
                crate::pack::prepare_observed(&root, store, &writes, &control.cancel, control)?;
            if let Some(report) = qualification {
                let corpus = crate::compatibility::corpus(&root, &control.cancel, control)?;
                ensure!(
                    report.corpus == corpus
                        && report.qualifies(
                            game,
                            &corpus.sha256,
                            crate::compatibility::Policy::default()
                        ),
                    "Game files changed; automatic activation was stopped"
                );
            }
            control
                .transaction("Activating storage; the original remains available for rollback")?;
            activate_prepared(snapshot, db, mounts, install)
        }
        PackTask::Compact => {
            pack_compact_observed(snapshot, db, mounts, &game.install_dir, control)
        }
        PackTask::Restore => {
            control.transaction("Restoring files; this step must finish before stopping")?;
            pack_rollback(snapshot, db, mounts, &game.install_dir)
        }
        PackTask::VerifyRestored => {
            let position = snapshot
                .packs
                .iter()
                .position(|install| install.game_path == game.install_dir)
                .context("Install record is missing")?;
            let install = snapshot
                .packs
                .get(position)
                .context("Install record is missing")?;
            crate::pack::verify_restored(install, &control.cancel, control)?;
            // Verification closes only the record. Store, update layer, and backups remain.
            snapshot.packs.remove(position);
            save_packs(db, snapshot)
        }
        PackTask::Reclaim => {
            control.transaction("Reclaiming the retained original; this step must finish")?;
            pack_reclaim(snapshot, db, &game.install_dir)
        }
        PackTask::Prune => {
            control.transaction("Reclaiming the previous version; this step must finish")?;
            pack_prune(snapshot, db, &game.install_dir)
        }
    }
}

#[cfg(not(feature = "pack-mount"))]
fn run_pack_task(
    _task: &PackTask,
    _game: &Game,
    _snapshot: &mut Snapshot,
    _db: &Connection,
    _mounts: &mut Vec<PackMount>,
    _control: &PackControl,
) -> Result<()> {
    bail!("Build Flummox with pack mounting to run storage jobs")
}

/// Drives the storage thread for one loop pass: passes on cancel and pause,
/// applies its progress events to the job, and, once the thread has ended,
/// takes back its records and mounts and records the outcome.
fn poll_pack(
    active: &mut Option<PackActive>,
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    if let Some(running) = active.as_ref() {
        let job = snapshot
            .jobs
            .iter_mut()
            .find(|job| job.id == running.id)
            .context("Storage job is missing")?;
        if job.phase == Phase::Cancelling || job.phase == Phase::Cancelled {
            running.control.cancel.store(true, Ordering::SeqCst);
        }
        // The thread only acts on this flag at a checkpoint, so setting it
        // during a step that must finish has no effect.
        let paused = job.user_paused || snapshot.gaming.is_some() || !job.game.state.is_idle();
        running.control.paused.store(paused, Ordering::Relaxed);
        job.pack_interruptible = running.control.interruptible.load(Ordering::SeqCst);
        for update in running.events.try_iter() {
            let _finished = event(job, WorkerEvent::Progress(update), db)?;
        }
        if paused && job.pack_interruptible && job.phase != Phase::Cancelling {
            job.message = pause_reason(job.user_paused, snapshot.gaming.as_deref());
        }
        job.elapsed = running.started.elapsed().as_secs();
        save(db, job)?;
    }
    if active
        .as_ref()
        .is_some_and(|running| running.thread.is_finished())
        && let Some(running) = active.take()
    {
        let job = snapshot
            .jobs
            .iter_mut()
            .find(|job| job.id == running.id)
            .context("Storage job is missing")?;
        // `Ok` means the thread returned. Its records and mounts replace the
        // coordinator's, whatever the task's own result. `Err` means it
        // panicked: the mounts it held were dropped with it and the
        // coordinator keeps the records it had before the task.
        match running.thread.join() {
            Ok(result) => {
                snapshot.packs = result.packs;
                *mounts = result.mounts;
                let lock_busy = result
                    .result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.to_string().contains(super::LOCK_BUSY));
                job.phase = if result.result.is_ok() {
                    Phase::Completed
                } else if lock_busy {
                    Phase::Queued
                } else if running.control.cancel.load(Ordering::SeqCst) {
                    Phase::Cancelled
                } else {
                    Phase::Failed
                };
                job.message = match result.result {
                    Ok(()) => "Storage job finished".into(),
                    Err(error) => error.to_string(),
                };
            }
            Err(_) => {
                job.phase = Phase::Interrupted;
                job.message = "Storage worker stopped; review recovery before retrying".into();
            }
        }
        job.pack_interruptible = false;
        save(db, job)?;
        #[cfg(feature = "pack-mount")]
        save_packs(db, snapshot)?;
        super::autostart::configure(
            &binary()?,
            snapshot.libraries.iter().any(|l| l.automatic) || !snapshot.packs.is_empty(),
        )?;
    }
    Ok(())
}

/// Coordinator entry point. Returns at once when another coordinator holds
/// `owner.lock`. Otherwise it serves clients until a restart request or until
/// it has been idle for 60 seconds, and removes its socket on the way out.
pub(super) fn run() -> Result<()> {
    let dir = state_dir()?;
    // One coordinator per user. The lock is held until this function returns.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("owner.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    // With the lock held, a socket file still present has no listener.
    let socket = dir.join("control.sock");
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let (db, mut snapshot) = open_store(&dir.join("queue.sqlite"))?;
    // Startup: remount activated installs, reapply live compression, and
    // bring the login entry in line with the saved settings.
    let mut mounts: Vec<PackMount> = Vec::new();
    survive(
        "recovering Maximum Space installs",
        recover_packs(&mut snapshot, &db, &mut mounts),
    );
    for library in &snapshot.libraries {
        if library.automatic
            && let Err(error) = update_live_compression(library)
        {
            tracing::warn!(%error, path = %library.path.display(), "live compression was not enabled");
        }
    }
    survive(
        "configuring login startup",
        binary().and_then(|binary| {
            super::autostart::configure(
                &binary,
                snapshot.libraries.iter().any(|library| library.automatic)
                    || !snapshot.packs.is_empty(),
            )
        }),
    );
    let history =
        crate::db::Db::open(&crate::db::Db::default_path().context("Cannot locate history")?)?;
    // Import exclusions from older CLI-only installations and share future edits.
    for (id, _) in history.excluded()? {
        if !snapshot.excluded.contains(&id.to_string()) {
            snapshot.excluded.push(id.to_string());
        }
    }
    for id in &snapshot.excluded {
        if let Some(game) = crate::db::parse_game_id(id)
            && !history.is_excluded(&game)?
        {
            history.exclude(&game, id)?;
        }
    }
    let mut active: Option<Active> = None;
    let mut pack_active: Option<PackActive> = None;
    // Backdated so the first pass scans and checks for running games.
    let mut last_scan = Instant::now() - Duration::from_secs(60);
    let mut last_busy = Instant::now() - Duration::from_secs(60);
    snapshot.worker_epoch = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
    )?;
    snapshot.revision = 0;
    let mut discovery: Option<crate::launchers::scan_job::Worker> = None;
    let mut refresh_requested = false;
    // Maintenance observations from earlier runs. With none saved, the first
    // finished scan sets a baseline and queues nothing.
    let mut known: Observations = db
        .query_row("SELECT data FROM settings WHERE id=2", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    let mut initialized = !known.0.is_empty();
    let mut upkeep: std::collections::HashMap<String, Upkeep> = db
        .query_row("SELECT data FROM settings WHERE id=7", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    let mut last_client = Instant::now();
    let mut games = Vec::new();
    let mut last_save = Instant::now();
    let mut last_pack_recovery = Instant::now();
    // Each pass runs the stages below in order, then sleeps 50 ms.
    loop {
        // Discovery scans. A scan starts on request, every 3 seconds while
        // jobs are active, and every 30 seconds otherwise. It runs on its own
        // thread and at most one runs at a time.
        let scan_interval = if snapshot.jobs.iter().any(|job| job.phase.active()) {
            3
        } else {
            30
        };
        if discovery.is_none()
            && (refresh_requested || last_scan.elapsed() >= Duration::from_secs(scan_interval))
        {
            last_scan = Instant::now();
            refresh_requested = false;
            if let Some(env) = crate::launchers::Env::current() {
                snapshot.scan_generation = snapshot.scan_generation.saturating_add(1);
                snapshot.scan_source = Some("Starting discovery".into());
                discovery = Some(crate::launchers::scan_job::Worker::start(env));
            }
        }
        // Drain the scan's events. Batches update the visible list as they
        // arrive, and the finished scan replaces it. Events from a cancelled
        // scan are dropped, except that `Finished` still clears `discovery`.
        let events = discovery
            .as_ref()
            .map(|worker| worker.events())
            .unwrap_or_default();
        let mut completed_scan = false;
        for event in events {
            let cancelled = discovery.as_ref().is_some_and(|worker| worker.cancelled());
            match event {
                crate::launchers::scan_job::Event::Source(source) if !cancelled => {
                    snapshot.scan_source = Some(source.into())
                }
                crate::launchers::scan_job::Event::Batch(batch) if !cancelled => {
                    for game in batch {
                        if let Some(previous) =
                            snapshot.discovered.iter_mut().find(|old| old.id == game.id)
                        {
                            *previous = game;
                        } else {
                            snapshot.discovered.push(game);
                        }
                    }
                }
                crate::launchers::scan_job::Event::Finished(result) => {
                    if !cancelled && result.is_none() {
                        snapshot.scan_warnings.push(
                            "Discovery stopped unexpectedly; showing the previous library".into(),
                        );
                    }
                    if !cancelled && let Some(scan) = result {
                        snapshot.scan_warnings =
                            scan.warnings.iter().map(ToString::to_string).collect();
                        games = scan.games;
                        // A scan with warnings may have failed to read a
                        // source, so games it did not report are kept.
                        if !snapshot.scan_warnings.is_empty() {
                            for old in &snapshot.discovered {
                                if !games.iter().any(|game| game.id == old.id) {
                                    games.push(old.clone());
                                }
                            }
                        }
                        snapshot.discovered = games.clone();
                        completed_scan = true;
                    }
                    snapshot.scan_source = None;
                    last_scan = Instant::now();
                    discovery = None;
                }
                _ => {}
            }
        }
        // Running-game check, after each finished scan and at least every 3
        // seconds. `gaming` becomes the title of the first discovered or
        // queued game whose folder a process is using. This process and the
        // worker child are not counted.
        if completed_scan || last_busy.elapsed() >= Duration::from_secs(3) {
            last_busy = Instant::now();
            use crate::busy::ProcSource;
            let procs = crate::busy::ProcFs::new().processes();
            snapshot.gaming = snapshot
                .discovered
                .iter()
                .chain(
                    snapshot
                        .jobs
                        .iter()
                        .filter(|job| job.phase.active())
                        .map(|job| &job.game),
                )
                .filter(|g| !g.is_tool)
                .find_map(|g| {
                    procs
                        .iter()
                        .find(|p| {
                            p.pid != std::process::id() as i32
                                && active.as_ref().is_none_or(|a| p.pid != a.child.id() as i32)
                                && p.uses_dir(&g.install_dir)
                        })
                        .map(|_| g.title.clone())
                });
            // When /proc cannot be read, no game can be shown to be closed,
            // so work stays paused.
            if std::fs::read_dir("/proc/self/fd").is_err() {
                snapshot.gaming = Some("process information is unavailable".into());
            }
            // A store with updates folded in counts as played once its game
            // is seen running. Upkeep deletes the previous version after that.
            if let Some(title) = &snapshot.gaming {
                for install in snapshot
                    .packs
                    .iter()
                    .filter(|install| install.previous_store_path.is_some())
                {
                    if games
                        .iter()
                        .any(|game| game.install_dir == install.game_path && game.title == *title)
                    {
                        upkeep
                            .entry(install.game_path.to_string_lossy().into_owned())
                            .or_default()
                            .played = true;
                    }
                }
            }
        }
        // Maintenance, after each finished scan: queue compression for new
        // installs and settled updates in automatic libraries, and save the
        // observations when they changed.
        if completed_scan {
            let before = serde_json::to_string(&known).unwrap_or_default();
            for game in &games {
                if known.observe(game, &snapshot.libraries, initialized) {
                    match enqueue(
                        &mut snapshot,
                        game.clone(),
                        Operation::Compress,
                        CompressOpts::default(),
                        &db,
                    ) {
                        Ok(()) => {}
                        Err(error) => {
                            tracing::warn!(%error, game = %game.title, "maintenance job was not queued")
                        }
                    }
                }
            }
            initialized = true;
            let after = serde_json::to_string(&known).unwrap_or_default();
            if before != after {
                survive(
                    "saving maintenance observations",
                    db.execute(
                        "INSERT OR REPLACE INTO settings(id,data) VALUES(2,?1)",
                        [after],
                    )
                    .map_err(Into::into),
                );
            }
            // Store upkeep, on the same fresh game list.
            let before = serde_json::to_string(&upkeep).unwrap_or_default();
            run_upkeep(&mut snapshot, &games, &mut upkeep, &db);
            let after = serde_json::to_string(&upkeep).unwrap_or_default();
            if before != after {
                survive(
                    "saving store upkeep",
                    db.execute(
                        "INSERT OR REPLACE INTO settings(id,data) VALUES(7,?1)",
                        [after],
                    )
                    .map_err(Into::into),
                );
            }
        }
        // Accept at most one client per pass. The listener does not block,
        // and the whole request must arrive within 2 seconds.
        match listener.accept() {
            Ok((mut stream, _)) => {
                last_client = Instant::now();
                let mut restart = false;
                let result = (|| -> Result<()> {
                    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                    let message: Request = read_message(&mut BufReader::new(Within {
                        inner: &mut stream,
                        until: Instant::now() + Duration::from_secs(2),
                    }))?;
                    // `Restart` is accepted from any protocol version, so a
                    // client from another release can replace this process.
                    ensure!(
                        message.version == VERSION || matches!(&message.command, Command::Restart),
                        "Worker protocol changed. Restart Flummox."
                    );
                    if matches!(&message.command, Command::Restart) {
                        ensure!(
                            active.is_none()
                                && pack_active.is_none()
                                && !snapshot.jobs.iter().any(|job| job.phase.active()),
                            "Finish or cancel queued jobs before restarting the background worker."
                        );
                        ensure!(
                            snapshot.packs.is_empty(),
                            "Restore mounted Maximum Space games before restarting the background worker."
                        );
                        restart = true;
                    }
                    // After these commands the maintenance baseline and the
                    // login entry are refreshed, further down.
                    let startup_changed = matches!(
                        &message.command,
                        Command::Library(_)
                            | Command::RemoveLibrary(_)
                            | Command::PackActivate { .. }
                            | Command::PackRollback { .. }
                    );
                    match &message.command {
                        Command::RefreshDiscovery => refresh_requested = true,
                        Command::CancelDiscovery => {
                            if let Some(worker) = &discovery {
                                worker.cancel();
                            }
                            snapshot.scan_source = None;
                            refresh_requested = false;
                        }
                        _ => {}
                    }
                    // Parsed before `apply`, so an id that does not parse is
                    // refused with nothing changed.
                    let exclusion = match &message.command {
                        Command::Exclude { id, excluded } => Some((
                            crate::db::parse_game_id(id).context("Unrecognized game id")?,
                            *excluded,
                        )),
                        _ => None,
                    };
                    // Direct storage transactions run on this thread and use
                    // the mounts, which a running storage thread holds.
                    if matches!(
                        &message.command,
                        Command::PackActivate { .. }
                            | Command::PackRollback { .. }
                            | Command::PackReclaim { .. }
                            | Command::PackCompact { .. }
                            | Command::PackPrune { .. }
                    ) {
                        ensure!(
                            active.is_none() && pack_active.is_none(),
                            "A job is running. Wait for it to finish before changing storage."
                        );
                    }
                    // Pause and cancel for the running storage job go to its
                    // thread first. The request fails here, before `apply`,
                    // while the task is in a step that must finish.
                    if let Some(running) = &pack_active
                        && matches!(&message.command, Command::Pause { id, .. } | Command::Cancel(id) if *id == running.id)
                    {
                        running.control.request_control(&message.command)?;
                    }
                    apply(
                        message.command,
                        &mut snapshot,
                        &db,
                        &mut active,
                        &mut mounts,
                        pack_active.as_ref().map(|running| running.id),
                    )?;
                    // Mirror the change into the history database, which
                    // workers check before they process a game.
                    if let Some((id, excluded)) = exclusion {
                        if excluded {
                            let title = games
                                .iter()
                                .find(|game| game.ids().any(|game_id| game_id == &id))
                                .map(|game| game.title.as_str());
                            history.exclude(&id, title.unwrap_or(&id.key))?;
                        } else {
                            history.unexclude(&id)?;
                        }
                    }
                    if startup_changed {
                        known.baseline(&games, &snapshot.libraries);
                        db.execute(
                            "INSERT OR REPLACE INTO settings(id,data) VALUES(2,?1)",
                            [serde_json::to_string(&known)?],
                        )?;
                        super::autostart::configure(
                            &binary()?,
                            snapshot.libraries.iter().any(|l|l.automatic)
                                || !snapshot.packs.is_empty(),
                        )
                            .context("Library settings were saved, but login startup could not be configured")?;
                    }
                    Ok(())
                })();
                // Reply with the whole snapshot, or with the error alone. A
                // reply that cannot be written within the timeout is dropped.
                let restart = restart && result.is_ok();
                snapshot.revision = snapshot.revision.saturating_add(1);
                let response = Response {
                    version: VERSION,
                    snapshot: result.as_ref().ok().map(|_| snapshot.clone()),
                    error: result.err().map(|e| e.to_string()),
                };
                let _sent = serde_json::to_writer(&mut stream, &response)
                    .map_err(std::io::Error::other)
                    .and_then(|_| stream.write_all(b"\n"));
                if restart {
                    survive(
                        "removing the socket",
                        std::fs::remove_file(&socket).map_err(Into::into),
                    );
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                survive("accepting a client", Err::<(), _>(e.into()));
            }
        }
        // Drive the active worker. It should be paused while the user asked
        // for that, a game is running, or its own game is missing from
        // discovery or not idle. A control line is sent only when that
        // differs from the state last sent.
        let mut finished = false;
        if let Some(a) = active.as_mut()
            && let Some(job) = snapshot.jobs.iter_mut().find(|j| j.id == a.id)
        {
            let unavailable =
                current_game(&job.game, &games).is_none_or(|game| !game.state.is_idle());
            let paused = job.user_paused || snapshot.gaming.is_some() || unavailable;
            if paused != a.paused && job.phase != Phase::Cancelling {
                if let Err(error) = send_control(&mut a.input, Control::Pause(paused)) {
                    job.message = format!("Worker control disconnected: {error}");
                    // Closing stdin also asks an orphaned worker to stop.
                    finished = true;
                    job.phase = Phase::Interrupted;
                    survive("saving a job", save(&db, job));
                } else {
                    a.paused = paused;
                    job.phase = if paused {
                        Phase::Pausing
                    } else if job.operation == Operation::Analyze {
                        Phase::Analyzing
                    } else {
                        Phase::Running
                    };
                }
            }
            // Apply at most 256 queued events, so a busy worker cannot keep
            // the loop from reaching the next client.
            for update in a.events.try_iter().take(256) {
                match event(job, update, &db) {
                    Ok(true) => {
                        finished = true;
                        break;
                    }
                    Ok(false) => {}
                    // Progress cannot be recorded, so the job stops. Closing
                    // the worker's input asks it to cancel.
                    Err(error) => {
                        job.phase = Phase::Failed;
                        job.message = format!("Could not record progress: {error:#}");
                        survive("saving a job", save(&db, job));
                        finished = true;
                        break;
                    }
                }
            }
            if a.paused && !finished && job.phase != Phase::Cancelling {
                job.message = pause_reason(job.user_paused, snapshot.gaming.as_deref());
            }
            job.elapsed = a.started.elapsed().as_secs();
            // Progress reaches the database at most once a second. `event`
            // has already saved a job that ended.
            if last_save.elapsed() >= Duration::from_secs(1) {
                survive("saving a job", save(&db, job));
                last_save = Instant::now();
            }
        }
        if finished && let Some(mut a) = active.take() {
            drop(a.input);
            // Reaping must not block clients behind a filesystem operation.
            std::thread::spawn(move || {
                let _reaped = a.child.wait();
            });
        }
        // Pack tasks. Refresh the storage job's game record from discovery
        // and cancel the task if its game was excluded while it can still
        // stop. `poll_pack` then does the rest.
        if let Some(running) = &pack_active
            && let Some(job) = snapshot.jobs.iter_mut().find(|job| job.id == running.id)
        {
            if let Some(current) = current_game(&job.game, &games) {
                job.game = current.clone();
            }
            if is_excluded(&job.game, &snapshot.excluded)
                && running
                    .control
                    .interruptible
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                running
                    .control
                    .cancel
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        survive(
            "driving the storage job",
            poll_pack(&mut pack_active, &mut snapshot, &db, &mut mounts),
        );
        note_waiting(
            &mut snapshot,
            active
                .as_ref()
                .map(|a| a.id)
                .or(pack_active.as_ref().map(|p| p.id)),
        );
        // Choose the next job, only when nothing is running and no game is
        // being played. Candidates are queued, not paused by the user, not
        // excluded, and their game is discovered and idle. Requested work
        // goes before analysis, then the lowest id first.
        if active.is_none() && pack_active.is_none() && snapshot.gaming.is_none() {
            let next = snapshot
                .jobs
                .iter()
                .filter(|j| {
                    j.phase == Phase::Queued
                        && !j.user_paused
                        && !is_excluded(&j.game, &snapshot.excluded)
                        && current_game(&j.game, &games).is_some_and(|game| game.state.is_idle())
                })
                .min_by_key(|j| (j.operation == Operation::Analyze, j.id))
                .map(|j| j.id);
            if let Some(id) = next
                && let Some(job) = snapshot.jobs.iter_mut().find(|j| j.id == id)
            {
                if let Some(current) = current_game(&job.game, &games) {
                    job.game = current.clone();
                }
                job.phase = if job.operation == Operation::Analyze {
                    Phase::Analyzing
                } else {
                    Phase::Running
                };
                job.message = "Preparing files".into();
                survive("saving a job", save(&db, job));
                // A pack job runs on a storage thread in this process and
                // takes every mount with it. Other jobs get a worker process.
                if job.operation == Operation::Pack {
                    match start_pack(
                        job,
                        &snapshot.packs,
                        &snapshot.libraries,
                        std::mem::take(&mut mounts),
                        &dir.join("queue.sqlite"),
                    ) {
                        Ok(running) => pack_active = Some(running),
                        Err(error) => {
                            job.phase = Phase::Failed;
                            job.message = error.to_string();
                            survive("saving a job", save(&db, job));
                        }
                    }
                    continue;
                }
                match start(job) {
                    Ok(a) => active = Some(a),
                    Err(e) => {
                        job.phase = Phase::Failed;
                        job.message = e.to_string();
                        survive("saving a job", save(&db, job));
                    }
                }
            }
        }
        // Mount upkeep. A mount whose session has ended is dropped and its
        // install marked for attention. While fewer mounts than installs
        // exist and no storage thread holds them, recovery retries every 5
        // seconds.
        #[cfg(feature = "pack-mount")]
        {
            let failed: Vec<_> = mounts
                .iter()
                .filter(|mounted| mounted.finished())
                .map(|mounted| mounted.path.clone())
                .collect();
            mounts.retain(|mounted| !failed.contains(&mounted.path));
            for path in failed {
                if let Some(install) = snapshot
                    .packs
                    .iter_mut()
                    .find(|install| install.game_path == path)
                {
                    install.phase = crate::pack::InstallPhase::Attention;
                    install.message = "The mount stopped; Flummox will retry it".into();
                }
            }
        }
        if pack_active.is_none()
            && mounts.len() < snapshot.packs.len()
            && last_pack_recovery.elapsed() >= Duration::from_secs(5)
        {
            survive(
                "recovering Maximum Space installs",
                recover_packs(&mut snapshot, &db, &mut mounts),
            );
            last_pack_recovery = Instant::now();
        }
        // Idle exit: no running or active job, no automatic library, no
        // activated install, and no client for 60 seconds.
        if active.is_none()
            && pack_active.is_none()
            && !snapshot.jobs.iter().any(|j| j.phase.active())
            && !snapshot.libraries.iter().any(|l| l.automatic)
            && snapshot.packs.is_empty()
            && last_client.elapsed() >= Duration::from_secs(60)
        {
            survive(
                "removing the socket",
                std::fs::remove_file(&socket).map_err(Into::into),
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{GameId, InstallState, Launcher},
        testutil::{Ctx, TestResult, check, check_eq},
    };

    fn game(root: &Path, key: &str) -> Game {
        Game {
            id: GameId::new(Launcher::Manual, key),
            also: vec![],
            title: key.into(),
            install_dir: root.into(),
            build: Some("1".into()),
            size_hint: None,
            state: InstallState::Idle,
            is_tool: false,
        }
    }

    #[test]
    fn a_job_that_finds_the_operation_lock_taken_goes_back_in_the_queue() -> TestResult {
        let temp = tempfile::tempdir().ctx("state")?;
        let (db, mut snapshot) = open_store(&temp.path().join("jobs.sqlite")).ctx("store")?;
        enqueue(
            &mut snapshot,
            game(temp.path(), "game"),
            Operation::Compress,
            CompressOpts::default(),
            &db,
        )
        .ctx("job")?;
        let job = snapshot.jobs.first_mut().ctx("job")?;
        job.phase = Phase::Running;
        let ended = event(
            job,
            WorkerEvent::Failed(format!("{:#}", anyhow::anyhow!(super::super::LOCK_BUSY))),
            &db,
        )
        .ctx("lock busy")?;
        check(ended && job.phase == Phase::Queued, "requeued, not failed")?;
        let ended = event(job, WorkerEvent::Failed("disk on fire".into()), &db).ctx("failure")?;
        check(
            ended && job.phase == Phase::Failed,
            "control: another failure still fails",
        )
    }


    #[test]
    fn a_different_storage_task_is_refused_and_the_same_one_is_not_duplicated() -> TestResult {
        let temp = tempfile::tempdir().ctx("state")?;
        let folder = temp.path().join("game");
        std::fs::create_dir(&folder).ctx("game folder")?;
        let (db, mut snapshot) = open_store(&temp.path().join("jobs.sqlite")).ctx("store")?;
        let queue = |snapshot: &mut Snapshot, task: PackTask| {
            enqueue_job(
                snapshot,
                game(&folder, "game"),
                Operation::Pack,
                CompressOpts::default(),
                Some(task),
                &db,
            )
        };
        queue(&mut snapshot, PackTask::Compact).ctx("compact")?;
        queue(&mut snapshot, PackTask::Compact).ctx("the same task again")?;
        check_eq(snapshot.jobs.len(), 1, "the same task is not queued twice")?;
        check(
            queue(&mut snapshot, PackTask::Restore).is_err(),
            "a restore is refused while a compaction waits",
        )?;
        check_eq(snapshot.jobs.len(), 1, "a refused task queues nothing")
    }

    #[test]
    fn a_folder_overlapping_a_discovered_game_cannot_be_queued() -> TestResult {
        let temp = tempfile::tempdir().ctx("state")?;
        let library = temp.path().join("library");
        let inside = library.join("game");
        let nested = inside.join("bin");
        let sibling = library.join("other");
        for folder in [&nested, &sibling] {
            std::fs::create_dir_all(folder).ctx("fixture folders")?;
        }
        let (db, mut snapshot) = open_store(&temp.path().join("jobs.sqlite")).ctx("store")?;
        snapshot.discovered.push(game(&inside, "game"));
        let queue = |snapshot: &mut Snapshot, folder: &Path| {
            enqueue(
                snapshot,
                game(folder, "custom"),
                Operation::Compress,
                CompressOpts::default(),
                &db,
            )
        };
        check(
            queue(&mut snapshot, &library).is_err(),
            "a folder holding a discovered game is refused",
        )?;
        check(
            queue(&mut snapshot, &nested).is_err(),
            "a folder inside a discovered game is refused",
        )?;
        queue(&mut snapshot, &sibling).ctx("control: an unrelated sibling is accepted")?;
        queue(&mut snapshot, &inside).ctx("control: the game's own folder is accepted")
    }

    #[test]
    fn maintenance_observes_enablement_new_installs_and_updates() -> TestResult {
        let temp = tempfile::tempdir().ctx("library")?;
        let mut seen = Observations::default();
        let mut old = game(temp.path(), "old");
        let off = vec![Library {
            path: temp.path().into(),
            automatic: false,
            custom: false,
            folder_kind: FolderKind::Game,
        }];
        let on = vec![Library {
            automatic: true,
            ..off.first().ctx("library policy")?.clone()
        }];
        check(
            !seen.observe(&old, &off, false),
            "initial discovery does not queue",
        )?;
        seen.baseline(&[old.clone()], &on);
        check(
            !seen.observe(&old, &on, true),
            "enabling does not compress existing installs",
        )?;
        let new = game(temp.path(), "new");
        check(seen.observe(&new, &on, true), "subsequent install queues")?;
        check(!seen.observe(&new, &on, true), "one job per install")?;
        old.state = InstallState::UpdatePending;
        check(!seen.observe(&old, &on, true), "unfinished download waits")?;
        let bytes = serde_json::to_vec(&seen).ctx("save observations")?;
        let mut reopened: Observations =
            serde_json::from_slice(&bytes).ctx("restore observations")?;
        old.state = InstallState::Idle;
        old.build = Some("2".into());
        check(
            reopened.observe(&old, &on, true),
            "settled update survives restart",
        )?;
        check(
            !reopened.observe(&old, &off, true),
            "turning maintenance off stops jobs",
        )
    }

    #[test]
    fn appearance_settings_survive_a_coordinator_restart() -> TestResult {
        let temp = tempfile::tempdir().ctx("state")?;
        let path = temp.path().join("jobs.sqlite");
        let (db, mut snapshot) = open_store(&path).ctx("open store")?;
        let mut mounts = Vec::new();
        apply(
            Command::Theme(super::super::ThemePreference::Light),
            &mut snapshot,
            &db,
            &mut None,
            &mut mounts,
            None,
        )
        .ctx("save theme")?;
        apply(
            Command::Motion(super::super::MotionPreference::Subtle),
            &mut snapshot,
            &db,
            &mut None,
            &mut mounts,
            None,
        )
        .ctx("save motion")?;
        drop(db);

        let (_db, restored) = open_store(&path).ctx("restart")?;
        check_eq(
            restored.theme,
            super::super::ThemePreference::Light,
            "theme",
        )?;
        check_eq(
            restored.motion,
            super::super::MotionPreference::Subtle,
            "motion",
        )?;
        check(
            !restored.reduced_motion,
            "compatibility setting follows motion",
        )
    }

    #[test]
    fn steam_write_directories_only_include_existing_paths() -> TestResult {
        let temp = tempfile::tempdir().ctx("library")?;
        let steamapps = temp.path().join("steamapps");
        std::fs::create_dir_all(steamapps.join("downloading")).ctx("download directory")?;
        let dirs = steam_write_dirs(temp.path());
        check_eq(dirs.len(), 2, "existing write directories")?;
        check(dirs.contains(&steamapps), "steamapps included")?;
        check(
            dirs.contains(&steamapps.join("downloading")),
            "downloads included",
        )
    }

    #[test]
    fn restart_preserves_queued_jobs_and_marks_inflight_work_interrupted() -> TestResult {
        let temp = tempfile::tempdir().ctx("state")?;
        let path = temp.path().join("jobs.sqlite");
        let (db, mut snapshot) = open_store(&path).ctx("queue")?;
        enqueue(
            &mut snapshot,
            game(temp.path(), "game"),
            Operation::Compress,
            CompressOpts::default(),
            &db,
        )
        .ctx("first job")?;
        enqueue(
            &mut snapshot,
            game(temp.path(), "game"),
            Operation::Compress,
            CompressOpts::default(),
            &db,
        )
        .ctx("duplicate")?;
        check_eq(snapshot.jobs.len(), 1, "duplicate request coalesces")?;
        let first = snapshot.jobs.first_mut().ctx("job")?;
        first.phase = Phase::Running;
        first.files_done = 12;
        save(&db, first).ctx("checkpoint")?;
        enqueue(
            &mut snapshot,
            game(temp.path(), "game"),
            Operation::Decompress,
            CompressOpts::default(),
            &db,
        )
        .ctx("next job")?;
        drop(db);
        let (db, mut restored) = open_store(&path).ctx("restart")?;
        let mut mounts = Vec::new();
        let interrupted = restored.jobs.first().ctx("interrupted job")?;
        check_eq(
            interrupted.phase,
            Phase::Interrupted,
            "inflight work is not called complete",
        )?;
        check_eq(interrupted.files_done, 12, "progress survives")?;
        check_eq(
            restored.jobs.last().ctx("queued job")?.phase,
            Phase::Queued,
            "queued work survives",
        )?;
        apply(
            Command::Exclude {
                id: "manual:game".into(),
                excluded: true,
            },
            &mut restored,
            &db,
            &mut None,
            &mut mounts,
            None,
        )
        .ctx("exclude")?;
        check(
            !restored.jobs.iter().any(|j| j.phase.active()),
            "exclusion removes pending work",
        )?;
        check(
            apply(
                Command::Retry(1),
                &mut restored,
                &db,
                &mut None,
                &mut mounts,
                None,
            )
            .is_err(),
            "retry cannot bypass exclusion",
        )
    }

    #[test]
    fn ipc_rejects_truncation_and_oversized_payloads() -> TestResult {
        check(
            read_message::<Request>(&mut std::io::Cursor::new(b"{}".as_slice())).is_err(),
            "missing delimiter",
        )?;
        let oversized = vec![b' '; LIMIT as usize + 2];
        check(
            read_message::<Request>(&mut std::io::Cursor::new(oversized)).is_err(),
            "bounded request",
        )?;
        let valid = serde_json::to_string(&Request {
            version: VERSION,
            command: Command::Snapshot,
        })
        .ctx("request")?
            + "\n";
        check_eq(
            read_message::<Request>(&mut std::io::Cursor::new(valid))
                .ctx("valid message")?
                .version,
            VERSION,
            "positive control",
        )
    }

    #[test]
    fn receipts_preserve_distinct_non_utf8_file_names() -> TestResult {
        use std::os::unix::ffi::OsStringExt;
        let temp = tempfile::tempdir().ctx("state")?;
        let (db, mut snapshot) = open_store(&temp.path().join("jobs.sqlite")).ctx("store")?;
        enqueue(
            &mut snapshot,
            game(temp.path(), "fixture"),
            Operation::Compress,
            CompressOpts::default(),
            &db,
        )
        .ctx("job")?;
        let job = snapshot.jobs.first_mut().ctx("job")?;
        for byte in [0xfe, 0xff] {
            let entry = crate::inventory::FileEntry {
                rel: std::ffi::OsString::from_vec(vec![b'f', byte]).into(),
                size: 100_000,
                ino: 1,
                mtime_ns: 0,
                ctime_ns: 0,
                action: crate::inventory::Action::Compress,
            };
            let encoded = serde_json::to_vec(&entry).ctx("encode losslessly")?;
            let decoded: crate::inventory::FileEntry =
                serde_json::from_slice(&encoded).ctx("decode")?;
            check_eq(decoded, entry.clone(), "IPC preserves filename bytes")?;
            event(
                job,
                WorkerEvent::Progress(crate::backend::Event::FileCompleted { entry, level: 9 }),
                &db,
            )
            .ctx("checkpoint receipt")?;
        }
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0))
            .ctx("receipts")?;
        check_eq(count, 2, "lossy display names never merge outcomes")?;
        let current = policy(job).ctx("current policy")?;
        invalidate_receipts(
            &temp.path().join("jobs.sqlite"),
            &job.game.install_dir,
            Some(&current),
        )
        .ctx("resume the same operation")?;
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0))
            .ctx("retained")?;
        check_eq(count, 2, "unchanged policy preserves resume receipts")?;
        job.operation = Operation::Decompress;
        let next = policy(job).ctx("next policy")?;
        invalidate_receipts(
            &temp.path().join("jobs.sqlite"),
            &job.game.install_dir,
            Some(&next),
        )
        .ctx("start undo")?;
        // A worker can stop before sending even one completion event.
        drop(db);
        let (db, _) =
            open_store(&temp.path().join("jobs.sqlite")).ctx("restart after interrupted undo")?;
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0))
            .ctx("invalidated")?;
        check_eq(
            count,
            0,
            "interrupted undo cannot revive old compression receipts",
        )
    }
    #[test]
    fn upkeep_folds_in_updates_and_deletes_the_previous_version_after_play() -> TestResult {
        let temp = tempfile::tempdir().ctx("upkeep fixture")?;
        let folder = temp.path().join("game");
        let writes = temp.path().join("updates");
        std::fs::create_dir(&folder).ctx("game folder")?;
        std::fs::create_dir_all(writes.join("files")).ctx("update layer")?;
        let (db, mut snapshot) = open_store(&temp.path().join("queue.sqlite")).ctx("queue")?;
        let mut game = Game {
            id: crate::model::GameId::new(crate::model::Launcher::Steam, "42"),
            also: vec![],
            title: "Upkeep".into(),
            install_dir: folder.clone(),
            build: Some("1".into()),
            size_hint: None,
            state: crate::model::InstallState::Idle,
            is_tool: false,
        };
        snapshot.packs.push(crate::pack::Install {
            game_path: folder.clone(),
            store_path: temp.path().join("store"),
            writes_path: writes.clone(),
            backup_path: Some(temp.path().join("original")),
            previous_store_path: None,
            previous_writes_path: None,
            summary: None,
            phase: crate::pack::InstallPhase::Mounted,
            message: String::new(),
        });
        let mut records = std::collections::HashMap::new();
        let queued = |snapshot: &Snapshot| -> Vec<PackTask> {
            snapshot
                .jobs
                .iter()
                .filter_map(|job| job.pack.clone())
                .collect()
        };
        std::fs::write(writes.join("files/patch.bin"), b"update").ctx("update")?;
        game.build = Some("2".into());

        // While the original is kept the user has not confirmed the game
        // runs, so nothing is done however much has changed.
        run_upkeep(
            &mut snapshot,
            std::slice::from_ref(&game),
            &mut records,
            &db,
        );
        check(queued(&snapshot).is_empty(), "nothing before confirmation")?;

        // Confirmed. The first sight only records the build.
        snapshot.packs.first_mut().ctx("install")?.backup_path = None;
        run_upkeep(
            &mut snapshot,
            std::slice::from_ref(&game),
            &mut records,
            &db,
        );
        check(
            queued(&snapshot).is_empty(),
            "the first sight is a baseline",
        )?;

        // A new build with updates in the layer is folded in, once.
        game.build = Some("3".into());
        run_upkeep(
            &mut snapshot,
            std::slice::from_ref(&game),
            &mut records,
            &db,
        );
        run_upkeep(
            &mut snapshot,
            std::slice::from_ref(&game),
            &mut records,
            &db,
        );
        check_eq(
            queued(&snapshot),
            vec![PackTask::Compact],
            "one compaction for one update",
        )?;

        // The compaction ran and kept the previous version. It is deleted
        // only after the game has been played.
        {
            let job = snapshot.jobs.first_mut().ctx("compaction job")?;
            job.phase = Phase::Completed;
        }
        snapshot
            .packs
            .first_mut()
            .ctx("install")?
            .previous_store_path = Some(temp.path().join("previous"));
        run_upkeep(
            &mut snapshot,
            std::slice::from_ref(&game),
            &mut records,
            &db,
        );
        check_eq(
            queued(&snapshot).len(),
            1,
            "no delete before the game is played",
        )?;
        records
            .get_mut(folder.to_string_lossy().as_ref())
            .ctx("upkeep record")?
            .played = true;
        run_upkeep(
            &mut snapshot,
            std::slice::from_ref(&game),
            &mut records,
            &db,
        );
        check_eq(
            queued(&snapshot),
            vec![PackTask::Compact, PackTask::Prune],
            "the previous version goes after play",
        )?;

        // A delete that failed is left for the user and not queued again.
        snapshot.jobs.last_mut().ctx("prune job")?.phase = Phase::Failed;
        run_upkeep(
            &mut snapshot,
            std::slice::from_ref(&game),
            &mut records,
            &db,
        );
        check_eq(queued(&snapshot).len(), 2, "a failed task is not requeued")
    }

    #[test]
    fn a_large_update_layer_counts_as_an_update_without_a_build_number() -> TestResult {
        let install = crate::pack::Install {
            game_path: "/fixture/game".into(),
            store_path: "/fixture/store".into(),
            writes_path: "/fixture/updates".into(),
            backup_path: None,
            previous_store_path: None,
            previous_writes_path: None,
            summary: None,
            phase: crate::pack::InstallPhase::Mounted,
            message: String::new(),
        };
        let mut record = Upkeep::default();
        check_eq(
            upkeep_due(&install, None, UPKEEP_LAYER_BYTES - 1, &mut record),
            None,
            "saves and settings in the game folder are left alone",
        )?;
        check_eq(
            upkeep_due(&install, None, UPKEEP_LAYER_BYTES, &mut record),
            Some(PackTask::Compact),
            "a layer this large is an update",
        )
    }

    #[test]
    fn a_late_pause_is_harmless_and_waiting_jobs_say_what_holds_them() -> TestResult {
        let temp = tempfile::tempdir().ctx("queue fixture")?;
        let database = temp.path().join("queue.sqlite");
        let (db, mut snapshot) = open_store(&database).ctx("queue")?;
        for name in ["First", "Second"] {
            let folder = temp.path().join(name);
            std::fs::create_dir(&folder).ctx("fixture game")?;
            enqueue_job(
                &mut snapshot,
                Game {
                    id: crate::model::GameId::new(crate::model::Launcher::Manual, name),
                    also: vec![],
                    title: name.into(),
                    install_dir: folder,
                    build: None,
                    size_hint: None,
                    state: crate::model::InstallState::Idle,
                    is_tool: false,
                },
                Operation::Compress,
                CompressOpts::default(),
                None,
                &db,
            )
            .ctx("enqueue")?;
        }
        let first = snapshot.jobs.first().ctx("first job")?.id;
        let message = |snapshot: &Snapshot| -> Result<String, String> {
            Ok(snapshot.jobs.last().ctx("second job")?.message.clone())
        };
        // The first job is running and the user has paused it.
        {
            let job = snapshot.jobs.first_mut().ctx("first job")?;
            job.phase = Phase::Paused;
            job.user_paused = true;
        }
        note_waiting(&mut snapshot, Some(first));
        check_eq(
            message(&snapshot)?.as_str(),
            "Waiting for First, which is paused",
            "the queued job names what it is behind",
        )?;
        snapshot.jobs.first_mut().ctx("first job")?.user_paused = false;
        note_waiting(&mut snapshot, Some(first));
        check_eq(
            message(&snapshot)?.as_str(),
            "Queued",
            "control: the note goes when the pause does",
        )?;
        // A Pause that arrives after the job finished changes nothing and is
        // not an error.
        snapshot.jobs.first_mut().ctx("first job")?.phase = Phase::Completed;
        apply(
            Command::Pause {
                id: first,
                paused: true,
            },
            &mut snapshot,
            &db,
            &mut None,
            &mut Vec::new(),
            None,
        )
        .ctx("pause a finished job")?;
        check(
            !snapshot.jobs.first().ctx("first job")?.user_paused,
            "a finished job is not marked paused",
        )
    }

    #[test]
    fn pack_controls_and_retries_preserve_durable_task_parameters() -> TestResult {
        use crate::pack::Observer;
        let control = PackControl {
            interruptible: std::sync::atomic::AtomicBool::new(true),
            ..Default::default()
        };
        control
            .request_control(&Command::Cancel(1))
            .ctx("cancel preparing storage")?;
        check(
            control.checkpoint().is_err(),
            "cancel is observed at the next checkpoint",
        )?;
        let control = PackControl {
            interruptible: std::sync::atomic::AtomicBool::new(false),
            ..Default::default()
        };
        check(
            control.request_control(&Command::Cancel(1)).is_err(),
            "transaction cannot be cancelled midway",
        )?;
        let temp = tempfile::tempdir().ctx("durable pack queue")?;
        let source = temp.path().join("game");
        std::fs::create_dir(&source).ctx("fixture game")?;
        let database = temp.path().join("queue.sqlite");
        let (db, mut snapshot) = open_store(&database).ctx("queue")?;
        let game = Game {
            id: crate::model::GameId::new(crate::model::Launcher::Manual, "storage"),
            also: vec![],
            title: "Storage fixture".into(),
            install_dir: source,
            build: Some("1".into()),
            size_hint: Some(0),
            state: crate::model::InstallState::Idle,
            is_tool: false,
        };
        let task = PackTask::Create {
            store: temp.path().join("store"),
        };
        enqueue_job(
            &mut snapshot,
            game,
            Operation::Pack,
            CompressOpts::default(),
            Some(task.clone()),
            &db,
        )
        .ctx("enqueue pack")?;
        let id = snapshot.jobs.first().ctx("job")?.id;
        apply(
            Command::Pause { id, paused: true },
            &mut snapshot,
            &db,
            &mut None,
            &mut Vec::new(),
            None,
        )
        .ctx("pause queued task")?;
        drop(db);
        let (db, mut snapshot) = open_store(&database).ctx("restart queue")?;
        check_eq(
            snapshot.jobs.first().ctx("restored task")?.pack.clone(),
            Some(task.clone()),
            "paths and task survive restart",
        )?;
        check_eq(
            snapshot.jobs.first().ctx("restored phase")?.phase,
            Phase::Paused,
            "a job paused before it started is still paused after a restart",
        )?;
        apply(
            Command::Cancel(id),
            &mut snapshot,
            &db,
            &mut None,
            &mut Vec::new(),
            None,
        )
        .ctx("cancel the paused task")?;
        apply(
            Command::Retry(id),
            &mut snapshot,
            &db,
            &mut None,
            &mut Vec::new(),
            None,
        )
        .ctx("retry the cancelled preparation")?;
        check_eq(
            snapshot.jobs.last().ctx("retry")?.pack.clone(),
            Some(task),
            "retry keeps storage destination",
        )?;
        check_eq(
            snapshot.jobs.last().ctx("retry phase")?.phase,
            Phase::Queued,
            "retry is queued",
        )
    }
}
