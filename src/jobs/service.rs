//! Local IPC, durable queue ownership, and worker supervision.

use super::*;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::os::unix::{
    fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    net::{UnixListener, UnixStream},
};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const LIMIT: u64 = 8 * 1024 * 1024;

pub fn state_dir() -> Result<PathBuf> {
    let path = crate::db::Db::default_path().context("Cannot locate Flummox's state folder")?;
    let dir = path
        .parent()
        .context("Invalid state folder")?
        .join("desktop");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let meta = std::fs::symlink_metadata(&dir)?;
    ensure!(
        meta.is_dir() && meta.uid() == nix::unistd::geteuid().as_raw(),
        "Unsafe Flummox state folder"
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn binary() -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    let sibling = current.with_file_name("flummox");
    if sibling.is_file() {
        Ok(sibling)
    } else {
        ensure!(
            current.is_file(),
            "The Flummox worker executable is missing"
        );
        Ok(current)
    }
}

/// Connects to the same user's coordinator, starting it when necessary.
/// The socket is reachable only through an owner-only directory.
pub fn request(command: Command) -> Result<Snapshot> {
    let dir = state_dir()?;
    let socket = dir.join("control.sock");
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(_) => {
            std::process::Command::new(binary()?)
                .arg("__coordinator")
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(dir.join("service.log"))?,
                )
                .spawn()?;
            let started = Instant::now();
            loop {
                if let Ok(stream) = UnixStream::connect(&socket) {
                    break stream;
                }
                ensure!(
                    started.elapsed() < Duration::from_secs(5),
                    "The background worker did not start. See service.log in Flummox's state folder."
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let filesystem_transaction = matches!(
        &command,
        Command::PackActivate { .. }
            | Command::PackRollback { .. }
            | Command::PackReclaim { .. }
            | Command::PackCompact { .. }
            | Command::PackPrune { .. }
    );
    stream.set_read_timeout(Some(Duration::from_secs(if filesystem_transaction {
        7_200
    } else {
        8
    })))?;
    stream.set_write_timeout(Some(Duration::from_secs(8)))?;
    serde_json::to_writer(
        &mut stream,
        &Request {
            version: VERSION,
            command,
        },
    )?;
    stream.write_all(b"\n")?;
    let response: Response = read_message(&mut BufReader::new(stream))?;
    ensure!(
        response.version == VERSION,
        "Restart Flummox's background worker after updating."
    );
    if let Some(error) = response.error {
        bail!(error);
    }
    response.snapshot.context("The worker returned no state")
}

pub(super) fn read_message<T: serde::de::DeserializeOwned>(reader: &mut impl BufRead) -> Result<T> {
    let mut line = String::new();
    reader.take(LIMIT + 1).read_line(&mut line)?;
    ensure!(
        line.len() as u64 <= LIMIT && line.ends_with('\n'),
        "Incomplete or oversized worker message"
    );
    Ok(serde_json::from_str(&line)?)
}

struct Active {
    id: i64,
    child: Child,
    input: ChildStdin,
    events: mpsc::Receiver<WorkerEvent>,
    started: Instant,
    paused: bool,
}

#[cfg(feature = "pack-mount")]
type PackMount = crate::pack::MountedInstall;

#[cfg(not(feature = "pack-mount"))]
struct PackMount;

#[cfg(feature = "pack-mount")]
fn save_packs(db: &Connection, snapshot: &Snapshot) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO settings(id,data) VALUES(4,?1)",
        [serde_json::to_string(&snapshot.packs)?],
    )?;
    Ok(())
}

#[cfg(feature = "pack-mount")]
fn refresh_pack_summaries(snapshot: &mut Snapshot) {
    // Shared-byte accounting depends on how many stores currently link each
    // pool object. Refresh every readable store after that population changes.
    // A broken install keeps its last useful summary and is handled by the
    // normal recovery path instead of failing an otherwise successful action.
    for install in &mut snapshot.packs {
        if let Ok(reader) = crate::pack::Reader::open(&install.store_path) {
            install.summary = Some(reader.summary().clone());
        }
    }
}

#[cfg(feature = "pack-mount")]
fn take_mount(mounts: &mut Vec<PackMount>, path: &Path) -> Option<PackMount> {
    mounts
        .iter()
        .position(|mounted| mounted.path == path)
        .map(|position| mounts.remove(position))
}

#[cfg(feature = "pack-mount")]
fn pack_activate(
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
    game_path: &Path,
    store_path: &Path,
    writes_path: &Path,
) -> Result<()> {
    ensure!(
        crate::busy::process_using(game_path, &crate::busy::ProcFs::new()).is_none(),
        "Close the game and launcher activity before activating its store"
    );
    let install = crate::pack::prepare(
        game_path,
        store_path,
        writes_path,
        &std::sync::atomic::AtomicBool::new(false),
    )?;
    ensure!(
        crate::busy::process_using(game_path, &crate::busy::ProcFs::new()).is_none(),
        "The game or launcher became active while its store was being verified"
    );
    ensure!(
        !snapshot
            .packs
            .iter()
            .any(|current| current.game_path == install.game_path),
        "This game already has an activated store"
    );
    snapshot.packs.push(install);
    save_packs(db, snapshot)?;
    let install = snapshot
        .packs
        .last_mut()
        .context("The activated install record is missing")?;
    match crate::pack::activate(install) {
        Ok(mounted) => mounts.push(mounted),
        Err(error) => {
            install.message = format!("Activation needs recovery: {error}");
            save_packs(db, snapshot)?;
            return Err(error);
        }
    }
    refresh_pack_summaries(snapshot);
    save_packs(db, snapshot)
}

#[cfg(not(feature = "pack-mount"))]
fn pack_activate(
    _snapshot: &mut Snapshot,
    _db: &Connection,
    _mounts: &mut Vec<PackMount>,
    _game_path: &Path,
    _store_path: &Path,
    _writes_path: &Path,
) -> Result<()> {
    bail!("Build Flummox with --features pack-mount to activate a store")
}

#[cfg(feature = "pack-mount")]
fn pack_rollback(
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
    game_path: &Path,
) -> Result<()> {
    let canonical = game_path
        .canonicalize()
        .context("Finding the launcher path")?;
    ensure!(
        crate::busy::process_using(&canonical, &crate::busy::ProcFs::new()).is_none(),
        "Close the game and launcher activity before restoring its files"
    );
    let position = snapshot
        .packs
        .iter()
        .position(|install| install.game_path == canonical)
        .context("This game has no activated store")?;
    let install = snapshot
        .packs
        .get_mut(position)
        .context("The activated install record is missing")?;
    install.phase = crate::pack::InstallPhase::Restoring;
    install.message = "Restoring ordinary files at the launcher path".into();
    save_packs(db, snapshot)?;
    let install = snapshot
        .packs
        .get(position)
        .cloned()
        .context("The activated install record is missing")?;
    let mounted = take_mount(mounts, &canonical);
    crate::pack::rollback(
        &install,
        mounted,
        &std::sync::atomic::AtomicBool::new(false),
    )?;
    snapshot.packs.remove(position);
    save_packs(db, snapshot)?;
    super::autostart::configure(
        &binary()?,
        snapshot.libraries.iter().any(|library| library.automatic) || !snapshot.packs.is_empty(),
    )?;
    Ok(())
}

#[cfg(not(feature = "pack-mount"))]
fn pack_rollback(
    _snapshot: &mut Snapshot,
    _db: &Connection,
    _mounts: &mut Vec<PackMount>,
    _game_path: &Path,
) -> Result<()> {
    bail!("Build Flummox with --features pack-mount to restore an activated store")
}

#[cfg(feature = "pack-mount")]
fn pack_reclaim(snapshot: &mut Snapshot, db: &Connection, game_path: &Path) -> Result<()> {
    let canonical = game_path
        .canonicalize()
        .context("Finding the launcher path")?;
    ensure!(
        crate::busy::process_using(&canonical, &crate::busy::ProcFs::new()).is_none(),
        "Close the game and launcher activity before reclaiming space"
    );
    let install = snapshot
        .packs
        .iter_mut()
        .find(|install| install.game_path == canonical)
        .context("This game has no activated store")?;
    crate::pack::reclaim(install)?;
    save_packs(db, snapshot)?;
    let install = snapshot
        .packs
        .iter_mut()
        .find(|install| install.game_path == canonical)
        .context("The activated install record is missing")?;
    crate::pack::finish_reclaim(install)?;
    save_packs(db, snapshot)
}

#[cfg(not(feature = "pack-mount"))]
fn pack_reclaim(_snapshot: &mut Snapshot, _db: &Connection, _game_path: &Path) -> Result<()> {
    bail!("Build Flummox with --features pack-mount to reclaim a rollback copy")
}

#[cfg(feature = "pack-mount")]
fn compact_path(path: &Path, identity: &Path, label: &str) -> Result<PathBuf> {
    let parent = path.parent().context("The managed path has no parent")?;
    let identity = blake3::hash(identity.as_os_str().as_bytes()).to_hex();
    for attempt in 0..100u32 {
        let candidate = parent.join(format!(
            ".flummox-{identity}-{label}-{}-{attempt}",
            std::process::id()
        ));
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => return Err(error).context("Checking the compaction destination"),
        }
    }
    bail!("Could not reserve a path for the compacted install")
}

#[cfg(feature = "pack-mount")]
fn remove_store(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(feature = "pack-mount")]
fn pack_compact(
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
    game_path: &Path,
) -> Result<()> {
    let canonical = game_path
        .canonicalize()
        .context("Finding the launcher path")?;
    let position = snapshot
        .packs
        .iter()
        .position(|install| install.game_path == canonical)
        .context("This game has no activated store")?;
    let install = snapshot
        .packs
        .get(position)
        .context("The activated install record is missing")?;
    ensure!(
        install.phase == crate::pack::InstallPhase::Mounted,
        "Install is not ready"
    );
    ensure!(
        install.previous_store_path.is_none() && install.previous_writes_path.is_none(),
        "Reclaim the previous compacted version before compacting again"
    );
    let old_store = install.store_path.clone();
    let old_writes = install.writes_path.clone();
    let pool = crate::pack::Reader::open(&old_store)?
        .pool_path()
        .map(Path::to_path_buf);
    let new_store = compact_path(&old_store, &canonical, "compact-store")?;
    let new_writes = compact_path(&old_writes, &canonical, "compact-updates")?;
    let controller = mounts
        .iter()
        .find(|mounted| mounted.path == canonical)
        .and_then(PackMount::writes)
        .context("The writable install is not mounted")?;
    let baseline = controller.generation();

    let options = crate::pack::Options::maximum();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let summary = if let Some(pool) = pool {
        crate::pack::create_shared(&canonical, &new_store, &pool, options, &cancel)
    } else {
        crate::pack::create(&canonical, &new_store, options, &cancel)
    }
    .context("Building the compacted store from the live install")?;
    let frozen = controller.freeze()?;
    if frozen.generation() != baseline {
        let _removed = remove_store(&new_store);
        bail!("The game changed while compaction finished; retry when launcher updates settle")
    }

    let install = snapshot
        .packs
        .get_mut(position)
        .context("The activated install record is missing")?;
    install.phase = crate::pack::InstallPhase::Compacting;
    install.message = "Switching to the compacted store".into();
    save_packs(db, snapshot)?;
    let mounted = take_mount(mounts, &canonical).context("The writable install is not mounted")?;
    mounted.stop().context("Unmounting the previous store")?;

    let install = snapshot
        .packs
        .get_mut(position)
        .context("The activated install record is missing")?;
    install.previous_store_path = Some(old_store);
    install.previous_writes_path = Some(old_writes);
    install.store_path = new_store;
    install.writes_path = new_writes;
    install.summary = Some(summary);
    install.phase = crate::pack::InstallPhase::Mounted;
    install.message = "Updates compacted; previous version retained for recovery".into();
    save_packs(db, snapshot)?;
    let install = snapshot
        .packs
        .get_mut(position)
        .context("The activated install record is missing")?;
    let mounted = crate::pack::recover(install)?.context("The compacted store did not remount")?;
    mounts.push(mounted);
    refresh_pack_summaries(snapshot);
    save_packs(db, snapshot)
}

#[cfg(not(feature = "pack-mount"))]
fn pack_compact(
    _snapshot: &mut Snapshot,
    _db: &Connection,
    _mounts: &mut Vec<PackMount>,
    _game_path: &Path,
) -> Result<()> {
    bail!("Build Flummox with --features pack-mount to compact an activated store")
}

#[cfg(feature = "pack-mount")]
fn pack_prune(snapshot: &mut Snapshot, db: &Connection, game_path: &Path) -> Result<()> {
    let canonical = game_path
        .canonicalize()
        .context("Finding the launcher path")?;
    let position = snapshot
        .packs
        .iter()
        .position(|install| install.game_path == canonical)
        .context("This game has no activated store")?;
    let install = snapshot
        .packs
        .get_mut(position)
        .context("The activated install record is missing")?;
    let pool = crate::pack::Reader::open(&install.store_path)?
        .pool_path()
        .map(Path::to_path_buf);
    crate::pack::begin_prune(install)?;
    save_packs(db, snapshot)?;
    let install = snapshot
        .packs
        .get_mut(position)
        .context("The activated install record is missing")?;
    crate::pack::finish_prune(install)?;
    save_packs(db, snapshot)?;
    if let Some(pool) = pool {
        let _pruned = crate::pack::prune_shared_pool(&pool)?;
    }
    refresh_pack_summaries(snapshot);
    save_packs(db, snapshot)
}

#[cfg(not(feature = "pack-mount"))]
fn pack_prune(_snapshot: &mut Snapshot, _db: &Connection, _game_path: &Path) -> Result<()> {
    bail!("Build Flummox with --features pack-mount to reclaim a previous store")
}

#[cfg(feature = "pack-mount")]
fn recover_packs(
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
) -> Result<()> {
    let mut recovered = Vec::with_capacity(snapshot.packs.len());
    for mut install in std::mem::take(&mut snapshot.packs) {
        if install.phase == crate::pack::InstallPhase::Restoring {
            let completed = install
                .backup_path
                .as_ref()
                .is_none_or(|backup| !backup.exists())
                && install.game_path.is_dir()
                && std::fs::read_dir(&install.game_path)
                    .is_ok_and(|mut entries| entries.next().is_some());
            if completed {
                continue;
            }
            match crate::pack::rollback(&install, None, &std::sync::atomic::AtomicBool::new(false))
            {
                Ok(()) => continue,
                Err(error) => {
                    install.message = format!("Could not finish restoring files: {error}");
                    recovered.push(install);
                    continue;
                }
            }
        }
        if mounts
            .iter()
            .any(|mounted| mounted.path == install.game_path)
        {
            recovered.push(install);
            continue;
        }
        match crate::pack::recover(&mut install) {
            Ok(Some(mounted)) => mounts.push(mounted),
            Ok(None) => {}
            Err(error) => {
                install.phase = crate::pack::InstallPhase::Attention;
                install.message = format!("Could not mount automatically: {error}");
            }
        }
        recovered.push(install);
    }
    snapshot.packs = recovered;
    refresh_pack_summaries(snapshot);
    save_packs(db, snapshot)
}

#[cfg(not(feature = "pack-mount"))]
fn recover_packs(
    snapshot: &mut Snapshot,
    _db: &Connection,
    _mounts: &mut Vec<PackMount>,
) -> Result<()> {
    for install in &mut snapshot.packs {
        install.phase = crate::pack::InstallPhase::Attention;
        install.message = "This build cannot mount pack stores".into();
    }
    Ok(())
}

fn send_control(input: &mut impl Write, control: Control) -> Result<()> {
    serde_json::to_writer(&mut *input, &control)?;
    input.write_all(b"\n")?;
    input.flush()?;
    Ok(())
}

fn is_excluded(game: &Game, excluded: &[String]) -> bool {
    game.ids().any(|id| excluded.contains(&id.to_string()))
}

fn current_game<'a>(game: &'a Game, games: &'a [Game]) -> Option<&'a Game> {
    games
        .iter()
        .find(|current| current.install_dir == game.install_dir)
        .or_else(|| {
            (game.id.launcher == crate::model::Launcher::Manual && game.install_dir.is_dir())
                .then_some(game)
        })
}

#[derive(Default, Serialize, Deserialize)]
struct Observations(std::collections::HashMap<String, (Option<String>, bool, bool)>);

impl Observations {
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

    fn baseline(&mut self, games: &[Game], libraries: &[Library]) {
        for game in games {
            let _changed = self.observe(game, libraries, false);
        }
    }
}

fn save(db: &Connection, job: &Job) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO queue(id, data) VALUES(?1, ?2)",
        params![job.id, serde_json::to_string(job)?],
    )?;
    Ok(())
}

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

fn open_store(path: &Path) -> Result<(Connection, Snapshot)> {
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
        CREATE TABLE IF NOT EXISTS queue(id INTEGER PRIMARY KEY, data TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS settings(id INTEGER PRIMARY KEY, data TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS receipts(game TEXT NOT NULL, path TEXT NOT NULL, policy TEXT NOT NULL, entry TEXT NOT NULL, PRIMARY KEY(game,path));")?;
    let mut snapshot = Snapshot::default();
    let mut stmt = db.prepare("SELECT data FROM queue ORDER BY id DESC LIMIT 500")?;
    for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
        let mut job: Job = serde_json::from_str(&row?)?;
        if job.phase.active() && job.phase != Phase::Queued {
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

fn enqueue(
    snapshot: &mut Snapshot,
    game: Game,
    operation: Operation,
    options: CompressOpts,
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
    if snapshot
        .jobs
        .iter()
        .any(|j| j.phase.active() && j.game.install_dir == path && j.operation == operation)
    {
        return Ok(());
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
    };
    save(db, &job)?;
    snapshot.jobs.push(job);
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

fn apply(
    command: Command,
    snapshot: &mut Snapshot,
    db: &Connection,
    active: &mut Option<Active>,
    mounts: &mut Vec<PackMount>,
) -> Result<()> {
    match command {
        Command::Snapshot => {}
        Command::ReducedMotion(value) => {
            db.execute(
                "INSERT OR REPLACE INTO settings(id,data) VALUES(3,?1)",
                [value.to_string()],
            )?;
            snapshot.reduced_motion = value;
        }
        Command::Enqueue {
            game,
            operation,
            options,
        } => {
            enqueue(snapshot, game, operation, options, db)?;
            if operation != Operation::Analyze
                && let Some(worker) = active.as_mut()
                && let Some(job) = snapshot
                    .jobs
                    .iter_mut()
                    .find(|j| j.id == worker.id && j.operation == Operation::Analyze)
            {
                send_control(&mut worker.input, Control::Cancel)?;
                job.phase = Phase::Cancelling;
                job.message = "Making room for your requested job".into();
                save(db, job)?;
            }
        }
        Command::Pause { id, paused } => {
            let job = snapshot
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .context("Job no longer exists")?;
            ensure!(job.phase.active(), "This job has finished");
            job.user_paused = paused;
            if job.phase == Phase::Queued
                || (job.phase == Phase::Paused && active.as_ref().is_none_or(|a| a.id != id))
            {
                job.phase = if paused { Phase::Paused } else { Phase::Queued };
            }
            save(db, job)?;
        }
        Command::Cancel(id) => {
            let job = snapshot
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .context("Job no longer exists")?;
            if let Some(a) = active.as_mut().filter(|a| a.id == id) {
                send_control(&mut a.input, Control::Cancel)?;
                job.phase = Phase::Cancelling;
            } else if job.phase.active() {
                job.phase = Phase::Cancelled;
            }
            save(db, job)?;
        }
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
            enqueue(snapshot, game, operation, options, db)?;
        }
        Command::Library(mut library) => {
            library.path = validate_folder(&library.path)?;
            snapshot.libraries.retain(|l| l.path != library.path);
            snapshot.libraries.push(library);
            settings(db, snapshot)?;
        }
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

fn start(job: &Job, db: &Connection) -> Result<Active> {
    let _store = db;
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

fn event(job: &mut Job, event: WorkerEvent, db: &Connection) -> Result<bool> {
    use crate::backend::Event;
    match event {
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
        WorkerEvent::Failed(message) => {
            job.phase = Phase::Failed;
            job.message = message;
            save(db, job)?;
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn run() -> Result<()> {
    let dir = state_dir()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("owner.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let socket = dir.join("control.sock");
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let (db, mut snapshot) = open_store(&dir.join("queue.sqlite"))?;
    let mut mounts: Vec<PackMount> = Vec::new();
    recover_packs(&mut snapshot, &db, &mut mounts)?;
    super::autostart::configure(
        &binary()?,
        snapshot.libraries.iter().any(|library| library.automatic) || !snapshot.packs.is_empty(),
    )?;
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
    let mut last_scan = Instant::now() - Duration::from_secs(60);
    let mut known: Observations = db
        .query_row("SELECT data FROM settings WHERE id=2", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .map(|json| serde_json::from_str(&json))
        .transpose()?
        .unwrap_or_default();
    let mut initialized = !known.0.is_empty();
    let mut last_client = Instant::now();
    let mut games = Vec::new();
    let mut last_save = Instant::now();
    let mut last_pack_recovery = Instant::now();
    loop {
        let scan_interval = if snapshot.jobs.iter().any(|job| job.phase.active()) {
            3
        } else {
            30
        };
        if last_scan.elapsed() >= Duration::from_secs(scan_interval) {
            last_scan = Instant::now();
            if let Some(env) = crate::launchers::Env::current() {
                games = crate::launchers::scan_all(&env).games;
            }
            use crate::busy::ProcSource;
            let procs = crate::busy::ProcFs::new().processes();
            snapshot.gaming = games.iter().filter(|g| !g.is_tool).find_map(|g| {
                procs
                    .iter()
                    .find(|p| {
                        p.pid != std::process::id() as i32
                            && active.as_ref().is_none_or(|a| p.pid != a.child.id() as i32)
                            && p.uses_dir(&g.install_dir)
                    })
                    .map(|_| g.title.clone())
            });
            if std::fs::read_dir("/proc/self/fd").is_err() {
                snapshot.gaming = Some("process information is unavailable".into());
            }
            let before = serde_json::to_string(&known)?;
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
            let after = serde_json::to_string(&known)?;
            if before != after {
                db.execute(
                    "INSERT OR REPLACE INTO settings(id,data) VALUES(2,?1)",
                    [after],
                )?;
            }
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                last_client = Instant::now();
                stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                let result = (|| -> Result<()> {
                    let message: Request = read_message(&mut BufReader::new(&mut stream))?;
                    ensure!(
                        message.version == VERSION,
                        "Worker protocol changed. Restart Flummox."
                    );
                    let startup_changed = matches!(
                        &message.command,
                        Command::Library(_)
                            | Command::PackActivate { .. }
                            | Command::PackRollback { .. }
                    );
                    let exclusion = match &message.command {
                        Command::Exclude { id, excluded } => Some((
                            crate::db::parse_game_id(id).context("Unrecognized game id")?,
                            *excluded,
                        )),
                        _ => None,
                    };
                    apply(
                        message.command,
                        &mut snapshot,
                        &db,
                        &mut active,
                        &mut mounts,
                    )?;
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
                let response = Response {
                    version: VERSION,
                    snapshot: result.as_ref().ok().map(|_| snapshot.clone()),
                    error: result.err().map(|e| e.to_string()),
                };
                let _sent = serde_json::to_writer(&mut stream, &response)
                    .map_err(std::io::Error::other)
                    .and_then(|_| stream.write_all(b"\n"));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
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
                    save(&db, job)?;
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
            for update in a.events.try_iter().take(256) {
                if event(job, update, &db)? {
                    finished = true;
                    break;
                }
            }
            job.elapsed = a.started.elapsed().as_secs();
            if last_save.elapsed() >= Duration::from_secs(1) {
                save(&db, job)?;
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
        if active.is_none() && snapshot.gaming.is_none() {
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
                save(&db, job)?;
                match start(job, &db) {
                    Ok(a) => active = Some(a),
                    Err(e) => {
                        job.phase = Phase::Failed;
                        job.message = e.to_string();
                        save(&db, job)?;
                    }
                }
            }
        }
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
        if mounts.len() < snapshot.packs.len()
            && last_pack_recovery.elapsed() >= Duration::from_secs(5)
        {
            recover_packs(&mut snapshot, &db, &mut mounts)?;
            last_pack_recovery = Instant::now();
        }
        if active.is_none()
            && !snapshot.jobs.iter().any(|j| j.phase.active())
            && !snapshot.libraries.iter().any(|l| l.automatic)
            && snapshot.packs.is_empty()
            && last_client.elapsed() >= Duration::from_secs(60)
        {
            std::fs::remove_file(&socket)?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Reads custom library configuration without starting the coordinator.
pub fn configured_libraries() -> Result<Vec<Library>> {
    let Some(path) = crate::db::Db::default_path()
        .and_then(|p| p.parent().map(|p| p.join("desktop/queue.sqlite")))
    else {
        return Ok(vec![]);
    };
    if !path.exists() {
        return Ok(vec![]);
    }
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = db.prepare("SELECT data FROM settings WHERE id=1")?;
    let Some(row) = stmt.query_map([], |r| r.get::<_, String>(0))?.next() else {
        return Ok(vec![]);
    };
    let (libraries, _): (Vec<Library>, Vec<String>) = serde_json::from_str(&row?)?;
    Ok(libraries)
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
    fn maintenance_observes_enablement_new_installs_and_updates() -> TestResult {
        let temp = tempfile::tempdir().ctx("library")?;
        let mut seen = Observations::default();
        let mut old = game(temp.path(), "old");
        let off = vec![Library {
            path: temp.path().into(),
            automatic: false,
            custom: false,
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
}
