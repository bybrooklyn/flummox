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
        .env("XDG_CONFIG_HOME", home.join("config"))
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
    check(
        request(&home, Request::Restart).is_err(),
        "restart refuses to interrupt mounted game reads",
    )?;
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

#[cfg(feature = "pack-mount")]
#[test]
fn queued_store_creation_survives_clients_and_keeps_source_bytes() -> TestResult {
    use flummox::jobs::PackTask;
    let temp = tempfile::tempdir().ctx("isolated storage queue")?;
    let home = temp.path().join("home");
    let source = temp.path().join("game");
    std::fs::create_dir_all(&home).ctx("home")?;
    std::fs::create_dir(&source).ctx("game")?;
    let payload: Vec<_> = (0..128 * 1024u32)
        .map(|n| (n.wrapping_mul(7919) >> 7) as u8)
        .collect();
    std::fs::write(source.join("assets.bin"), &payload).ctx("source bytes")?;
    let store = temp.path().join("storage/game.store");
    let _service = start(&home)?;
    let game = Game {
        id: GameId::new(Launcher::Manual, "pack-fixture"),
        also: vec![],
        title: "Pack fixture".into(),
        install_dir: source.clone(),
        state: InstallState::Idle,
        size_hint: Some(payload.len() as u64),
        build: Some("1".into()),
        is_tool: false,
    };
    let snapshot = request(
        &home,
        Request::EnqueuePack {
            game,
            task: PackTask::Create {
                store: store.clone(),
            },
        },
    )?;
    let id = snapshot.jobs.last().ctx("queued pack job")?.id;
    // Every request opens and closes its own socket; the coordinator owns the work.
    let result = finished(&home, id)?;
    check_eq(
        result.phase,
        Phase::Completed,
        format!("queued build: {}", result.message),
    )?;
    check(store.is_dir(), "verified store published by coordinator")?;
    check_eq(
        std::fs::read(source.join("assets.bin")).ctx("source after job")?,
        payload.clone(),
        "source stays intact",
    )?;
    let reader = flummox::pack::Reader::open(&store).ctx("published store")?;
    reader
        .verify(&std::sync::atomic::AtomicBool::new(false))
        .ctx("verify queued result")?;
    check_eq(
        reader
            .read(Path::new("assets.bin"), 0, payload.len())
            .ctx("stored bytes")?,
        payload,
        "queued creation preserves every byte",
    )
}

#[cfg(feature = "pack-mount")]
#[test]
fn automatic_storage_rejects_a_mismatched_qualification_before_creation() -> TestResult {
    use flummox::{
        compatibility::{Checks, Corpus, GameBuild, Platform, Report, StorageMode, StorageResult},
        jobs::PackTask,
    };
    let temp = tempfile::tempdir().ctx("qualification queue")?;
    let home = temp.path().join("home");
    let source = temp.path().join("game");
    std::fs::create_dir_all(&home).ctx("home")?;
    std::fs::create_dir(&source).ctx("source")?;
    std::fs::write(source.join("asset"), b"abc").ctx("source bytes")?;
    let _service = start(&home)?;
    let game = Game {
        id: GameId::new(Launcher::Manual, "qualified-fixture"),
        also: vec![],
        title: "Qualified fixture".into(),
        install_dir: source.clone(),
        state: InstallState::Idle,
        size_hint: Some(3),
        build: Some("1".into()),
        is_tool: false,
    };
    let report = Report {
        version: flummox::compatibility::VERSION,
        game: GameBuild {
            launcher: Launcher::Manual,
            key: "qualified-fixture".into(),
            build: "1".into(),
        },
        corpus: Corpus {
            sha256: "a".repeat(64),
            files: 1,
            bytes: 3,
        },
        platform: Platform::Linux,
        mode: StorageMode::MaximumSpace,
        checks: Checks {
            bytes_verified: true,
            metadata_verified: true,
            writable_update_verified: true,
            rollback_verified: true,
            launched: true,
            anti_cheat_issue: false,
            gameplay_issue: false,
            baseline_load_ms: 1000,
            candidate_load_ms: 1000,
        },
        storage: StorageResult {
            logical_bytes: 3,
            allocated_before: 4096,
            allocated_after: 2048,
            random_read_p95_ns: None,
        },
        flummox_version: env!("CARGO_PKG_VERSION").into(),
    };
    let store = temp.path().join("storage/game.store");
    let snapshot = request(
        &home,
        Request::EnqueuePack {
            game,
            task: PackTask::Activate {
                store: store.clone(),
                create: true,
                qualification: Some(Box::new(report)),
            },
        },
    )?;
    let job = finished(&home, snapshot.jobs.last().ctx("qualification job")?.id)?;
    check_eq(
        job.phase,
        Phase::Failed,
        "mismatched corpus does not activate",
    )?;
    check(
        job.message.contains("Compatibility no longer matches"),
        format!("clear qualification error: {}", job.message),
    )?;
    check(
        !store.exists(),
        "no store created for mismatched automatic qualification",
    )?;
    check_eq(
        std::fs::read(source.join("asset")).ctx("retained source")?,
        b"abc".to_vec(),
        "original remains intact",
    )
}

#[cfg(feature = "pack-mount")]
#[test]
fn queued_activation_compaction_reclaim_and_restore_preserve_updates() -> TestResult {
    use flummox::jobs::PackTask;
    if !Path::new("/dev/fuse").exists() {
        check(
            std::env::var_os("FLUMMOX_REQUIRE_FUSE").is_none(),
            "FUSE required for queued lifecycle",
        )?;
        eprintln!("skipped: queued storage lifecycle requires /dev/fuse");
        return Ok(());
    }
    let temp = tempfile::tempdir().ctx("queued pack lifecycle")?;
    let home = temp.path().join("home");
    let source = temp.path().join("game");
    std::fs::create_dir_all(&home).ctx("home")?;
    std::fs::create_dir(&source).ctx("game")?;
    std::fs::write(source.join("asset"), b"original").ctx("original")?;
    let _service = start(&home)?;
    let game = Game {
        id: GameId::new(Launcher::Manual, "queued-pack"),
        also: vec![],
        title: "Queued pack".into(),
        install_dir: source.clone(),
        state: InstallState::Idle,
        size_hint: Some(8),
        build: Some("1".into()),
        is_tool: false,
    };
    let store = temp.path().join("storage/game.store");
    let enqueue = |task: PackTask| -> Result<Snapshot, String> {
        let snapshot = request(
            &home,
            Request::EnqueuePack {
                game: game.clone(),
                task,
            },
        )?;
        let job = finished(&home, snapshot.jobs.last().ctx("queued storage job")?.id)?;
        check_eq(
            job.phase,
            Phase::Completed,
            format!("storage operation: {}", job.message),
        )?;
        request(&home, Request::Snapshot)
    };
    let activated = enqueue(PackTask::Activate {
        store,
        create: true,
        qualification: None,
    })?;
    let backup = activated
        .packs
        .first()
        .ctx("activated install")?
        .backup_path
        .as_ref()
        .ctx("retained original")?;
    check(backup.is_dir(), "activation retains original")?;
    std::fs::write(source.join("asset"), b"patched").ctx("mounted patch")?;
    std::fs::write(source.join("download"), b"new content").ctx("mounted new file")?;
    let compacted = enqueue(PackTask::Compact)?;
    check(
        compacted
            .packs
            .first()
            .ctx("compacted install")?
            .previous_store_path
            .is_some(),
        "compaction retains previous version",
    )?;
    enqueue(PackTask::Prune)?;
    enqueue(PackTask::Reclaim)?;
    let restored = enqueue(PackTask::Restore)?;
    check(
        restored.packs.is_empty(),
        "ordinary files leave managed storage",
    )?;
    check_eq(
        std::fs::read(source.join("asset")).ctx("restored patch")?,
        b"patched".to_vec(),
        "patch survives every queued transaction",
    )?;
    check_eq(
        std::fs::read(source.join("download")).ctx("restored download")?,
        b"new content".to_vec(),
        "new file survives reclaim and restore",
    )
}

#[test]
fn graceful_restart_preserves_settings_and_rejects_old_mutations() -> TestResult {
    let temp = tempfile::tempdir().ctx("upgrade fixture")?;
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).ctx("home")?;
    let mut service = start(&home)?;
    request(&home, Request::ReducedMotion(true))?;
    let socket = home.join("state/flummox/desktop/control.sock");
    let mut stream = UnixStream::connect(&socket).ctx("old client")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ctx("timeout")?;
    serde_json::to_writer(
        &mut stream,
        &serde_json::json!({
            "version": flummox::jobs::VERSION - 1,
            "command": {"ReducedMotion": false}
        }),
    )
    .ctx("old mutation")?;
    stream.write_all(b"\n").ctx("delimiter")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).ctx("reply")?;
    let reply: serde_json::Value = serde_json::from_str(&reply).ctx("response")?;
    check(
        reply.get("error").is_some_and(serde_json::Value::is_string),
        "old mutations rejected",
    )?;
    check(
        request(&home, Request::Snapshot)?.reduced_motion,
        "rejected client leaves settings intact",
    )?;
    request(&home, Request::Restart)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = service.0.try_wait().ctx("reap graceful exit")? {
            check(status.success(), "restart exits successfully")?;
            break;
        }
        check(Instant::now() < deadline, "coordinator did not exit")?;
        std::thread::sleep(Duration::from_millis(20));
    }
    check(!socket.exists(), "graceful exit removes stale socket")?;
    let _replacement = start(&home)?;
    check(
        request(&home, Request::Snapshot)?.reduced_motion,
        "replacement keeps durable settings",
    )
}

#[test]
fn custom_locations_persist_discover_games_and_remove_without_deletion() -> TestResult {
    use flummox::jobs::{FolderKind, Library};
    let temp = tempfile::tempdir().ctx("custom locations")?;
    let home = temp.path().join("home");
    let root = home.join("My Games");
    let game_path = root.join("Example Game");
    std::fs::create_dir_all(&game_path).ctx("fixture game")?;
    std::fs::write(game_path.join("save.dat"), b"keep this save").ctx("fixture save")?;
    let mut service = start(&home)?;
    let output = Command::new(env!("CARGO_BIN_EXE_flummox"))
        .args(["jobs", "add-folder", "~/My\\ Games"])
        .env("HOME", &home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .output()
        .ctx("add custom location")?;
    check(
        output.status.success(),
        format!("add location: {}", String::from_utf8_lossy(&output.stderr)),
    )?;
    let snapshot = request(&home, Request::Snapshot)?;
    check_eq(
        snapshot
            .libraries
            .first()
            .ctx("saved location")?
            .folder_kind,
        FolderKind::Collection,
        "CLI registers a collection",
    )?;
    service.stop()?;
    let _replacement = start(&home)?;
    check_eq(
        request(&home, Request::Snapshot)?.libraries.len(),
        1,
        "locations survive restart",
    )?;
    let scan = || -> Result<Vec<serde_json::Value>, String> {
        let output = Command::new(env!("CARGO_BIN_EXE_flummox"))
            .args(["--json", "scan"])
            .env("HOME", &home)
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_CONFIG_HOME", home.join("config"))
            .output()
            .ctx("scan custom games")?;
        check(output.status.success(), "scan succeeded")?;
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).ctx("scan JSON")?;
        serde_json::from_value(result).ctx("games list")
    };
    check_eq(scan()?.len(), 1, "collection child appears as a game")?;
    request(
        &home,
        Request::Library(Library {
            path: game_path.clone(),
            automatic: false,
            custom: true,
            folder_kind: FolderKind::Game,
        }),
    )?;
    check_eq(
        scan()?.len(),
        1,
        "overlapping single-game registration is deduplicated",
    )?;
    request(&home, Request::RemoveLibrary(root.clone()))?;
    check_eq(
        scan()?.len(),
        1,
        "removing collection preserves separately added games",
    )?;
    request(&home, Request::RemoveLibrary(game_path.clone()))?;
    check(
        scan()?.is_empty(),
        "removed locations disappear from discovery",
    )?;
    check_eq(
        std::fs::read(game_path.join("save.dat")).ctx("save after removal")?,
        b"keep this save".to_vec(),
        "removal preserves files",
    )
}
