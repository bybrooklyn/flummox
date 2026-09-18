//! Real coordinator/worker IPC against an isolated home and temporary games.

use flummox::{
    jobs::{Command as Request, Operation, Phase, Snapshot},
    model::{Game, GameId, InstallState, Launcher},
    testutil::{Ctx, TestResult, check, check_eq},
};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _killed = self.0.kill();
        let _waited = self.0.wait();
    }
}
impl Service {
    fn stop(&mut self) -> Result<(), String> {
        self.0.kill().ctx("stop coordinator")?;
        self.0.wait().ctx("reap coordinator")?;
        Ok(())
    }
}

fn start(home: &Path) -> Result<Service, String> {
    Command::new(env!("CARGO_BIN_EXE_flummox"))
        .arg("__coordinator")
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map(Service)
        .ctx("start isolated coordinator")
}

fn request(home: &Path, command: Request) -> Result<Snapshot, String> {
    let socket = home.join("state/flummox/desktop/control.sock");
    let until = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        if let Ok(stream) = UnixStream::connect(&socket) {
            break stream;
        }
        check(Instant::now() < until, "coordinator did not listen")?;
        std::thread::sleep(Duration::from_millis(20));
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ctx("IPC timeout")?;
    serde_json::to_writer(
        &mut stream,
        &serde_json::json!({"version":flummox::jobs::VERSION,"command":command}),
    )
    .ctx("request")?;
    stream.write_all(b"\n").ctx("delimiter")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ctx("reply")?;
    let value: serde_json::Value = serde_json::from_str(&line).ctx("response JSON")?;
    check(
        value.get("error").is_some_and(|e| e.is_null()),
        format!("IPC error: {value}"),
    )?;
    serde_json::from_value(value.get("snapshot").ctx("snapshot field")?.clone()).ctx("snapshot")
}

fn finished(home: &Path, id: i64) -> Result<flummox::jobs::Job, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let snapshot = request(home, Request::Snapshot)?;
        let job = snapshot
            .jobs
            .iter()
            .find(|j| j.id == id)
            .ctx("queued job")?;
        if !job.phase.active() {
            return Ok(job.clone());
        }
        check(Instant::now() < deadline, format!("job timed out: {job:?}"))?;
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(feature = "pack-mount")]
#[test]
fn managed_pack_remounts_after_coordinator_restart_and_keeps_updates() -> TestResult {
    if !Path::new("/dev/fuse").exists() {
        check(
            std::env::var_os("FLUMMOX_REQUIRE_FUSE").is_none(),
            "FUSE is required for this test run",
        )?;
        eprintln!("skipped: managed pack lifecycle requires /dev/fuse");
        return Ok(());
    }
    let temp = tempfile::tempdir().ctx("fixture")?;
    let home = temp.path().join("home");
    let game = temp.path().join("game");
    let store = temp.path().join("game.flumpack");
    let pool = temp.path().join("pool");
    let writes = temp.path().join("updates");
    std::fs::create_dir(&home).ctx("home")?;
    std::fs::create_dir(&game).ctx("game")?;
    std::fs::write(game.join("data"), b"base").ctx("source")?;
    flummox::pack::create_shared(
        &game,
        &store,
        &pool,
        flummox::pack::Options::default(),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .ctx("pack store")?;

    let mut service = start(&home)?;
    let snapshot = request(
        &home,
        Request::PackActivate {
            game_path: game.clone(),
            store_path: store.clone(),
            writes_path: writes.clone(),
        },
    )?;
    check_eq(snapshot.packs.len(), 1, "activation is durable")?;
    std::fs::write(game.join("data"), b"launcher update").ctx("mounted update")?;
    service.stop()?;

    let mut restarted = start(&home)?;
    let snapshot = request(&home, Request::Snapshot)?;
    check_eq(
        snapshot.packs.len(),
        1,
        "install survives coordinator restart",
    )?;
    check_eq(
        std::fs::read(game.join("data")).ctx("remounted update")?,
        b"launcher update".to_vec(),
        "automatic remount exposes persistent updates",
    )?;
    let snapshot = request(
        &home,
        Request::PackCompact {
            game_path: game.clone(),
        },
    )?;
    let compacted = snapshot.packs.first().ctx("compacted install")?;
    check(
        compacted.store_path != store,
        "compaction switches to a new store",
    )?;
    check(
        compacted.store_path.is_dir(),
        "compaction preserves the shared directory-store format",
    )?;
    check_eq(
        compacted.previous_store_path.as_ref(),
        Some(&store),
        "the previous store is retained",
    )?;
    check_eq(
        compacted.previous_writes_path.as_ref(),
        Some(&writes),
        "the previous update layer is retained",
    )?;
    check_eq(
        std::fs::read(game.join("data")).ctx("compacted update")?,
        b"launcher update".to_vec(),
        "the compacted mount includes existing updates",
    )?;
    std::fs::write(game.join("data"), b"second launcher update").ctx("post-compaction update")?;
    restarted.stop()?;

    let _restarted_again = start(&home)?;
    let snapshot = request(&home, Request::Snapshot)?;
    let compacted = snapshot.packs.first().ctx("remounted compacted install")?;
    check_eq(
        std::fs::read(game.join("data")).ctx("remounted compacted update")?,
        b"second launcher update".to_vec(),
        "the compacted install keeps later updates across restart",
    )?;
    let previous_store = compacted
        .previous_store_path
        .clone()
        .ctx("previous store")?;
    let previous_writes = compacted
        .previous_writes_path
        .clone()
        .ctx("previous updates")?;
    let snapshot = request(
        &home,
        Request::PackPrune {
            game_path: game.clone(),
        },
    )?;
    let pruned = snapshot.packs.first().ctx("pruned install")?;
    check(
        pruned.previous_store_path.is_none() && pruned.previous_writes_path.is_none(),
        "pruning clears retained-version metadata",
    )?;
    check(
        !previous_store.exists() && !previous_writes.exists(),
        "pruning deletes the retained store and update layer",
    )?;
    let snapshot = request(
        &home,
        Request::PackRollback {
            game_path: game.clone(),
        },
    )?;
    check(
        snapshot.packs.is_empty(),
        "rollback removes the durable mount",
    )?;
    check_eq(
        std::fs::read(game.join("data")).ctx("restored update")?,
        b"second launcher update".to_vec(),
        "rollback keeps updates written after compaction",
    )
}

#[test]
fn native_worker_reuses_receipts_and_reprocesses_a_changed_file() -> TestResult {
    let temp = tempfile::TempDir::new_in(std::env::current_dir().ctx("cwd")?).ctx("fixture")?;
    if flummox::fsprobe::probe(temp.path())
        .ctx("filesystem")?
        .fstype
        != "btrfs"
    {
        eprintln!("skipped: native worker round trip requires btrfs");
        return Ok(());
    }
    let home = temp.path().join("home");
    let path = temp.path().join("game");
    std::fs::create_dir_all(&home).ctx("home")?;
    std::fs::create_dir_all(&path).ctx("game")?;
    let payload = path.join("stored.zip");
    let mut data = b"PK\x03\x04".to_vec();
    data.resize(1024 * 1024, b'A');
    std::fs::write(&payload, &data).ctx("fixture data")?;
    let anchor = flummox::safeio::Anchor::open(&path).ctx("anchor")?;
    // Establish an uncompressed starting state despite the mount's default.
    flummox::backend::btrfs::decompress_fd(
        &anchor
            .open_file(Path::new("stored.zip"))
            .ctx("fixture file")?,
    )
    .ctx("raw baseline")?;
    let (compressed, mapped) =
        flummox::backend::btrfs::compressed_bytes(&payload).ctx("baseline extents")?;
    check(mapped > 0, "baseline maps actual extents")?;
    check_eq(
        compressed,
        0,
        "the baseline must be uncompressed before comparing",
    )?;
    let _service = start(&home)?;
    let game = Game {
        id: GameId::new(Launcher::Manual, "fixture"),
        also: vec![],
        title: "Fixture".into(),
        install_dir: path.clone(),
        build: None,
        size_hint: None,
        state: InstallState::Idle,
        is_tool: false,
    };
    for (iteration, expected) in [(0, 1), (1, 0), (2, 1)] {
        if iteration == 2 {
            data.push(b'B');
            std::fs::write(&payload, &data).ctx("game update")?;
            flummox::backend::btrfs::decompress_fd(
                &anchor
                    .open_file(Path::new("stored.zip"))
                    .ctx("updated file")?,
            )
            .ctx("updated raw baseline")?;
        }
        let snapshot = request(
            &home,
            Request::Enqueue {
                game: game.clone(),
                operation: Operation::Compress,
                options: Default::default(),
            },
        )?;
        let id = snapshot.jobs.last().ctx("new job")?.id;
        let job = finished(&home, id)?;
        check_eq(
            job.phase,
            Phase::Completed,
            format!("worker result: {job:?}"),
        )?;
        check_eq(
            job.files_done,
            expected,
            format!("iteration {iteration}: only new or changed data is rewritten; {job:?}"),
        )?;
        check_eq(
            std::fs::read(&payload).ctx("verify payload")?,
            data.clone(),
            "all original bytes remain playable",
        )?;
    }
    for (operation, expected_files, should_be_compressed) in [
        (Operation::Decompress, 1, false),
        (Operation::Decompress, 0, false),
        (Operation::Compress, 1, true),
    ] {
        let snapshot = request(
            &home,
            Request::Enqueue {
                game: game.clone(),
                operation,
                options: Default::default(),
            },
        )?;
        let job = finished(&home, snapshot.jobs.last().ctx("operation queued")?.id)?;
        check_eq(
            job.phase,
            Phase::Completed,
            format!("{operation:?}: {job:?}"),
        )?;
        check_eq(
            job.files_done,
            expected_files,
            "receipts describe the current operation",
        )?;
        let (compressed, mapped) =
            flummox::backend::btrfs::compressed_bytes(&payload).ctx("result extents")?;
        check(mapped > 0, "result has actual extents")?;
        check_eq(
            compressed > 0,
            should_be_compressed,
            "undo and recompress change extent state",
        )?;
        check_eq(
            std::fs::read(&payload).ctx("round-trip bytes")?,
            data.clone(),
            "switching operation preserves every byte",
        )?;
    }
    Ok(())
}

#[test]
fn closing_clients_keeps_jobs_and_restart_preserves_results() -> TestResult {
    let temp = tempfile::TempDir::new_in(std::env::current_dir().ctx("cwd")?).ctx("fixture")?;
    let home = temp.path().join("home");
    let game_path = temp.path().join("game");
    std::fs::create_dir_all(&home).ctx("fixture home")?;
    std::fs::create_dir_all(&game_path).ctx("fixture game")?;
    let data = b"fixture data for transparent compression\n".repeat(40_000);
    std::fs::write(game_path.join("raw.dds"), &data).ctx("fixture payload")?;
    let service = start(&home)?;
    let game = Game {
        id: GameId::new(Launcher::Manual, "fixture"),
        also: vec![],
        title: "Fixture".into(),
        install_dir: game_path.clone(),
        build: None,
        size_hint: Some(data.len() as u64),
        state: InstallState::Idle,
        is_tool: false,
    };
    let snapshot = request(
        &home,
        Request::Enqueue {
            game,
            operation: Operation::Analyze,
            options: Default::default(),
        },
    )?;
    let id = snapshot.jobs.first().ctx("job queued")?.id;
    // Every request drops its connection. The service owns the operation.
    let deadline = Instant::now() + Duration::from_secs(20);
    let phase = loop {
        let snapshot = request(&home, Request::Snapshot)?;
        let job = snapshot
            .jobs
            .iter()
            .find(|j| j.id == id)
            .ctx("job retained")?;
        if !job.phase.active() {
            break job.phase;
        }
        check(
            Instant::now() < deadline,
            format!("job did not finish: {job:?}"),
        )?;
        std::thread::sleep(Duration::from_millis(50));
    };
    let btrfs = flummox::fsprobe::probe(&game_path)
        .ctx("fixture filesystem")?
        .fstype
        == "btrfs";
    check_eq(
        phase,
        if btrfs {
            Phase::Completed
        } else {
            Phase::Failed
        },
        "supported drives analyze; unsupported drives fail clearly",
    )?;
    check_eq(
        std::fs::read(game_path.join("raw.dds")).ctx("payload after analysis")?,
        data,
        "analysis never changes bytes",
    )?;
    drop(service);
    let _reopened = start(&home)?;
    // Wait for the replacement process to acquire the lock and bind its socket.
    std::thread::sleep(Duration::from_millis(100));
    let reopened = request(&home, Request::Snapshot)?;
    check_eq(
        reopened.jobs.first().ctx("durable result")?.phase,
        phase,
        "reopening sees the same result",
    )?;
    request(
        &home,
        Request::Exclude {
            id: "manual:fixture".into(),
            excluded: true,
        },
    )?;
    let history =
        flummox::db::Db::open(&home.join("state/flummox/state.sqlite")).ctx("shared history")?;
    check(
        history
            .is_excluded(&GameId::new(Launcher::Manual, "fixture"))
            .ctx("excluded")?,
        "GUI exclusion is visible to CLI history",
    )?;
    request(
        &home,
        Request::Exclude {
            id: "manual:fixture".into(),
            excluded: false,
        },
    )?;
    check(
        !history
            .is_excluded(&GameId::new(Launcher::Manual, "fixture"))
            .ctx("restored")?,
        "restoring clears both views",
    )
}
