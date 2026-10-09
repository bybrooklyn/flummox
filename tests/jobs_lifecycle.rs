//! Real coordinator/worker IPC against an isolated home and temporary games.

#![cfg(target_os = "linux")]

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

/// Whether `path` is on btrfs. When it is not, the test skips, unless
/// `FLUMMOX_REQUIRE_BTRFS` is set, which turns the skip into a failure.
fn on_btrfs(path: &Path, what: &str) -> Result<bool, String> {
    let native = flummox::fsprobe::probe(path).ctx("filesystem")?.fstype == "btrfs";
    if !native {
        check(
            std::env::var_os("FLUMMOX_REQUIRE_BTRFS").is_none(),
            "btrfs is required for this test run",
        )?;
        eprintln!("skipped: {what} requires btrfs");
    }
    Ok(native)
}

fn fixture_game(path: &Path, key: &str) -> Game {
    Game {
        id: GameId::new(Launcher::Manual, key),
        also: vec![],
        title: key.into(),
        install_dir: path.to_path_buf(),
        build: None,
        size_hint: None,
        state: InstallState::Idle,
        is_tool: false,
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
    let source_volume = flummox::storage::volume(&game).ctx("source volume")?;
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
    check_eq(
        flummox::storage::volume(&game)
            .ctx("mounted volume")?
            .identity,
        source_volume.identity.clone(),
        "managed mount keeps the underlying library drive identity",
    )?;
    // A process working in the mounted folder keeps the coordinator running.
    let mut player = Command::new("sleep")
        .arg("30")
        .current_dir(&game)
        .spawn()
        .ctx("process in the game folder")?;
    let refused = request(&home, Request::Restart);
    let _killed = player.kill();
    let _waited = player.wait();
    check(
        refused
            .as_ref()
            .err()
            .is_some_and(|error| error.contains("game") && error.contains("Close")),
        format!("restart names the running game and says to close it: {refused:?}"),
    )?;
    check(
        service.0.try_wait().ctx("coordinator state")?.is_none(),
        "a refused restart leaves the coordinator running",
    )?;
    check_eq(
        std::fs::read(game.join("data")).ctx("read after refusal")?,
        b"base".to_vec(),
        "the store still serves after a refusal",
    )?;
    // With nothing using the folder, restart unmounts and exits, and the
    // next coordinator mounts the store again.
    request(&home, Request::Restart).ctx("restart with an idle mounted game")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = service.0.try_wait().ctx("reap restarted coordinator")? {
            check(status.success(), "restart exits successfully")?;
            break;
        }
        check(Instant::now() < deadline, "coordinator did not exit")?;
        std::thread::sleep(Duration::from_millis(20));
    }
    check(
        std::fs::read_dir(&game)
            .ctx("unmounted folder")?
            .next()
            .is_none(),
        "the store is unmounted before the coordinator exits",
    )?;
    let mut service = start(&home)?;
    let snapshot = request(&home, Request::Snapshot)?;
    check_eq(snapshot.packs.len(), 1, "the install is still recorded")?;
    check_eq(
        std::fs::read(game.join("data")).ctx("remounted after restart")?,
        b"base".to_vec(),
        "the next coordinator mounts the store again",
    )?;
    std::fs::write(game.join("data"), b"launcher update").ctx("mounted update")?;
    service.stop()?;

    let mut restarted = start(&home)?;
    let snapshot = request(&home, Request::Snapshot)?;
    check_eq(
        flummox::storage::volume(&game)
            .ctx("remounted volume")?
            .identity,
        source_volume.identity,
        "library drive identity survives remounting",
    )?;
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
    std::fs::write(game.join("only-before"), b"written before compaction")
        .ctx("update that is never rewritten")?;
    let refused = request(
        &home,
        Request::PackCompact {
            game_path: game.clone(),
        },
    );
    check(
        refused
            .as_ref()
            .err()
            .is_some_and(|error| error.contains("Delete the original before folding in updates")),
        format!("folding in updates waits for the original to be deleted: {refused:?}"),
    )?;
    request(
        &home,
        Request::PackReclaim {
            game_path: game.clone(),
        },
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
        std::fs::read(game.join("only-before")).ctx("restored earlier update")?,
        b"written before compaction".to_vec(),
        "rollback keeps updates the compacted store absorbed",
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
    if !on_btrfs(temp.path(), "native worker round trip")? {
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
    // A retained directory handle would make the test itself look like a running game.
    drop(anchor);
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
    let mut levels = Vec::new();
    for (iteration, expected) in [(0, 1), (1, 0), (2, 1)] {
        if iteration == 2 {
            data.push(b'B');
            std::fs::write(&payload, &data).ctx("game update")?;
            flummox::backend::btrfs::decompress_fd(
                &flummox::safeio::Anchor::open(&path)
                    .ctx("updated anchor")?
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
        let history =
            flummox::db::Db::open(&home.join("state/flummox/state.sqlite")).ctx("open history")?;
        levels.push(
            history
                .game(&game.id)
                .ctx("read history")?
                .ctx("game recorded")?
                .level,
        );
    }
    check_eq(
        levels.get(1),
        levels.first(),
        "a pass with nothing to do keeps the level the first pass recorded",
    )?;
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
fn a_compress_worker_reports_its_totals_once() -> TestResult {
    let temp = tempfile::TempDir::new_in(std::env::current_dir().ctx("cwd")?).ctx("fixture")?;
    if !on_btrfs(temp.path(), "worker progress")? {
        return Ok(());
    }
    let home = temp.path().join("home");
    let path = temp.path().join("game");
    std::fs::create_dir_all(&home).ctx("fixture home")?;
    std::fs::create_dir_all(&path).ctx("fixture game")?;
    let chunk = b"progress fixture payload line\n".repeat(40_000);
    let anchor = flummox::safeio::Anchor::open(&path).ctx("anchor")?;
    for index in 0..4 {
        let name = format!("payload-{index}.bin");
        std::fs::write(path.join(&name), &chunk).ctx("fixture payload")?;
        // Without a raw baseline the mount default leaves nothing to rewrite.
        flummox::backend::btrfs::decompress_fd(
            &anchor.open_file(Path::new(&name)).ctx("fixture file")?,
        )
        .ctx("raw baseline")?;
    }
    drop(anchor);
    // The coordinator creates the receipt store a worker reads, and its
    // finished analysis supplies a complete job record to hand to one.
    let mut service = start(&home)?;
    let snapshot = request(
        &home,
        Request::Enqueue {
            game: Game {
                id: GameId::new(Launcher::Manual, "progress-fixture"),
                also: vec![],
                title: "Progress Fixture".into(),
                install_dir: path.clone(),
                build: None,
                size_hint: None,
                state: InstallState::Idle,
                is_tool: false,
            },
            operation: Operation::Analyze,
            options: Default::default(),
        },
    )?;
    let mut job = finished(&home, snapshot.jobs.last().ctx("job queued")?.id)?;
    check_eq(job.phase, Phase::Completed, format!("analysis: {job:?}"))?;
    service.stop()?;
    job.operation = Operation::Compress;

    let mut worker = Command::new(env!("CARGO_BIN_EXE_flummox"))
        .arg("__worker")
        .env("HOME", &home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ctx("start worker")?;
    // Closing this pipe asks the worker to stop, so it stays open until exit.
    let mut input = worker.stdin.take().ctx("worker stdin")?;
    serde_json::to_writer(
        &mut input,
        &serde_json::json!({"version": flummox::jobs::VERSION, "job": job}),
    )
    .ctx("work")?;
    input.write_all(b"\n").ctx("delimiter")?;
    let mut started = 0;
    let mut last_bytes = 0;
    let mut done = None;
    for line in BufReader::new(worker.stdout.take().ctx("worker stdout")?).lines() {
        let event: serde_json::Value =
            serde_json::from_str(&line.ctx("worker line")?).ctx("event JSON")?;
        if let Some(totals) = event.pointer("/Progress/Started") {
            started += 1;
            check_eq(
                totals.get("files").and_then(|files| files.as_u64()),
                Some(4),
                "the rewrite counts every fixture file",
            )?;
        }
        if let Some(bytes) = event
            .pointer("/Progress/Progress/bytes_done")
            .and_then(|bytes| bytes.as_u64())
        {
            check(
                started > 0 || bytes == 0,
                format!("progress before the totals were known: {event}"),
            )?;
            check(
                bytes >= last_bytes,
                format!("progress moved backwards from {last_bytes}: {event}"),
            )?;
            last_bytes = bytes;
        }
        if let Some(result) = event.get("Done") {
            done = Some(result.clone());
        }
    }
    drop(input);
    check(worker.wait().ctx("reap worker")?.success(), "worker exit")?;
    check_eq(started, 1, "one set of totals for one progress bar")?;
    check_eq(
        last_bytes,
        4 * chunk.len() as u64,
        "progress ends at the total",
    )?;
    let done = done.ctx("the worker reported an outcome")?;
    check_eq(
        (done.get("cancelled"), done.get("errors")),
        (Some(&false.into()), Some(&serde_json::json!([]))),
        "the worker finished cleanly",
    )
}

#[test]
fn analysis_scales_in_the_files_its_budget_did_not_reach() -> TestResult {
    let temp = tempfile::TempDir::new_in(std::env::current_dir().ctx("cwd")?).ctx("fixture")?;
    if !on_btrfs(temp.path(), "analysis scaling")? {
        return Ok(());
    }
    let home = temp.path().join("home");
    let path = temp.path().join("game");
    std::fs::create_dir_all(&home).ctx("fixture home")?;
    std::fs::create_dir_all(&path).ctx("fixture game")?;
    // Analysis samples at most 2 MiB from a file and 32 MiB in all, so 48
    // files of this size leave a third of the game unsampled.
    let chunk = b"analysis fixture payload line\n".repeat(40_000);
    let anchor = flummox::safeio::Anchor::open(&path).ctx("anchor")?;
    let files = 48u64;
    for index in 0..files {
        let name = format!("payload-{index:02}.bin");
        std::fs::write(path.join(&name), &chunk).ctx("fixture payload")?;
        flummox::backend::btrfs::decompress_fd(
            &anchor.open_file(Path::new(&name)).ctx("fixture file")?,
        )
        .ctx("raw baseline")?;
    }
    drop(anchor);
    let total = files * chunk.len() as u64;
    let _service = start(&home)?;
    let snapshot = request(
        &home,
        Request::Enqueue {
            game: Game {
                id: GameId::new(Launcher::Manual, "analysis-fixture"),
                also: vec![],
                title: "Analysis Fixture".into(),
                install_dir: path.clone(),
                build: None,
                size_hint: None,
                state: InstallState::Idle,
                is_tool: false,
            },
            operation: Operation::Analyze,
            options: Default::default(),
        },
    )?;
    let job = finished(&home, snapshot.jobs.last().ctx("job queued")?.id)?;
    check_eq(job.phase, Phase::Completed, format!("analysis: {job:?}"))?;
    let estimate = job.estimate.ctx("estimate")?;
    check(
        estimate.unsampled_files > 0,
        format!("control: the budget must run out for this to test anything: {estimate:?}"),
    )?;
    check(
        estimate.disk_now > total / 10 * 9 && estimate.disk_now < total / 10 * 11,
        format!("the estimate covers the whole game of {total} bytes: {estimate:?}"),
    )?;
    check(
        estimate.saving() > total / 2,
        format!("repeated text saves most of its size: {estimate:?}"),
    )
}

#[test]
fn user_pause_holds_a_queued_job_and_resume_completes_it() -> TestResult {
    let temp = tempfile::TempDir::new_in(std::env::current_dir().ctx("cwd")?).ctx("fixture")?;
    if !on_btrfs(temp.path(), "pause round trip")? {
        return Ok(());
    }
    let home = temp.path().join("home");
    let path = temp.path().join("game");
    std::fs::create_dir_all(&home).ctx("fixture home")?;
    std::fs::create_dir_all(&path).ctx("fixture game")?;
    let chunk = b"pause fixture payload line for compression testing\n".repeat(150_000);
    let mut names = Vec::new();
    for index in 0..8 {
        let name = format!("payload-{index}.bin");
        std::fs::write(path.join(&name), &chunk).ctx("fixture payload")?;
        names.push(name);
    }
    // New files may land compressed under the mount default, leaving the
    // worker nothing to rewrite. Undo that first so the job takes real work.
    let anchor = flummox::safeio::Anchor::open(&path).ctx("anchor")?;
    for name in &names {
        flummox::backend::btrfs::decompress_fd(
            &anchor.open_file(Path::new(name)).ctx("fixture file")?,
        )
        .ctx("raw baseline")?;
    }
    let (compressed, mapped) =
        flummox::backend::btrfs::compressed_bytes(&path.join(names.first().ctx("fixture name")?))
            .ctx("baseline extents")?;
    check(mapped > 0, "baseline maps actual extents")?;
    check_eq(compressed, 0, "the baseline must be uncompressed")?;
    drop(anchor);
    let _service = start(&home)?;
    request(
        &home,
        Request::Library(flummox::jobs::Library {
            path: path.clone(),
            automatic: false,
            custom: true,
            folder_kind: flummox::jobs::FolderKind::Game,
        }),
    )?;
    request(&home, Request::RefreshDiscovery)?;
    // The rewrite takes milliseconds, so a pause sent after Enqueue can arrive
    // once the work is done. An open file makes the coordinator treat the game
    // as running, which keeps the job queued until the pause is recorded.
    let playing =
        std::fs::File::open(path.join(names.first().ctx("fixture name")?)).ctx("open game file")?;
    let settled = |wanted: bool, what: &str| -> Result<Snapshot, String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let snapshot = request(&home, Request::Snapshot)?;
            let found = snapshot.discovered.iter().any(|g| g.install_dir == path);
            if found && snapshot.gaming.is_some() == wanted {
                return Ok(snapshot);
            }
            check(
                Instant::now() < deadline,
                format!("{what}: discovered {found}, gaming {:?}", snapshot.gaming),
            )?;
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let snapshot = settled(true, "the open file never counted as play")?;
    let game = snapshot
        .discovered
        .iter()
        .find(|g| g.install_dir == path)
        .cloned()
        .ctx("discovered fixture")?;
    let snapshot = request(
        &home,
        Request::Enqueue {
            game,
            operation: Operation::Compress,
            options: Default::default(),
        },
    )?;
    let queued = snapshot
        .jobs
        .iter()
        .find(|j| j.operation == Operation::Compress)
        .ctx("job queued")?;
    check_eq(queued.phase, Phase::Queued, "play keeps the job queued")?;
    let id = queued.id;
    let snapshot = request(&home, Request::Pause { id, paused: true }).ctx("pause job")?;
    let job = snapshot
        .jobs
        .iter()
        .find(|j| j.id == id)
        .ctx("paused job")?;
    check_eq(job.phase, Phase::Paused, "a queued job pauses at once")?;
    check(job.user_paused, "pause records the user request")?;
    drop(playing);
    settled(false, "play never ended")?;
    // Without the pause, the job would start now that nothing uses the game.
    std::thread::sleep(Duration::from_millis(500));
    let snapshot = request(&home, Request::Snapshot)?;
    let held = snapshot.jobs.iter().find(|j| j.id == id).ctx("held job")?;
    check_eq(held.phase, Phase::Paused, "paused work stays paused")?;
    request(&home, Request::Pause { id, paused: false }).ctx("resume job")?;
    let job = finished(&home, id)?;
    check_eq(
        job.phase,
        Phase::Completed,
        format!("resumed result: {job:?}"),
    )?;
    check_eq(
        job.files_done,
        names.len() as u64,
        "resuming rewrites every fixture file",
    )?;
    for name in &names {
        check_eq(
            std::fs::read(path.join(name)).ctx("verify payload")?,
            chunk.clone(),
            "paused and resumed work preserves every byte",
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
    check_eq(
        phase,
        Phase::Completed,
        "read-only analysis works on native and other filesystems",
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
        job.message
            .contains("The compatibility report no longer matches"),
        format!("clear compatibility report error: {}", job.message),
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
fn queued_activation_reclaim_compaction_and_restore_preserve_updates() -> TestResult {
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
    enqueue(PackTask::Reclaim)?;
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

/// Stands in for a coordinator of another version: answers each request with
/// `version` and returns the commands it was sent, stopping after a restart.
fn other_version_coordinator(
    home: &Path,
    version: u32,
    requests: usize,
) -> Result<std::thread::JoinHandle<Result<Vec<serde_json::Value>, String>>, String> {
    let dir = home.join("state/flummox/desktop");
    std::fs::create_dir_all(&dir).ctx("state folder")?;
    std::fs::write(dir.join("owner.lock"), b"").ctx("owner lock")?;
    let socket = dir.join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).ctx("bind")?;
    Ok(std::thread::spawn(move || {
        let mut commands = Vec::new();
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().ctx("accept")?;
            let mut line = String::new();
            BufReader::new(&stream)
                .read_line(&mut line)
                .ctx("request")?;
            let request: serde_json::Value = serde_json::from_str(&line).ctx("request JSON")?;
            let command = request.get("command").cloned().ctx("command")?;
            let restart = command == serde_json::json!("Restart");
            let error = if restart {
                serde_json::Value::Null
            } else {
                "The background worker is from another version. Restart Flummox.".into()
            };
            if restart {
                std::fs::remove_file(&socket).ctx("remove socket")?;
            }
            serde_json::to_writer(
                &mut stream,
                &serde_json::json!({"version": version, "snapshot": null, "error": error}),
            )
            .ctx("reply")?;
            stream.write_all(b"\n").ctx("delimiter")?;
            commands.push(command);
            if restart {
                break;
            }
        }
        Ok(commands)
    }))
}

fn jobs_command(home: &Path, arguments: &[&str]) -> Result<std::process::Output, String> {
    Command::new(env!("CARGO_BIN_EXE_flummox"))
        .args(arguments)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .output()
        .ctx("run flummox")
}

#[test]
fn an_older_idle_coordinator_is_replaced_and_a_newer_one_is_left_alone() -> TestResult {
    let temp = tempfile::tempdir().ctx("upgrade fixture")?;
    let home = temp.path().join("older");
    let older = other_version_coordinator(&home, flummox::jobs::VERSION - 1, 2)?;
    let output = jobs_command(&home, &["--json", "jobs"])?;
    let commands = older.join().map_err(|_| "older coordinator thread")??;
    check_eq(
        commands,
        vec![serde_json::json!("Snapshot"), serde_json::json!("Restart")],
        "the client asks the older coordinator to restart",
    )?;
    check(
        output.status.success(),
        format!(
            "the command is answered by this version: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    check_eq(
        request(&home, Request::Snapshot)?.jobs.len(),
        0,
        "the replacement coordinator answers this protocol",
    )?;
    // The replacement was started by the command above, so nothing here owns
    // it. An idle coordinator exits when asked to restart.
    request(&home, Request::Restart).ctx("stop replacement")?;

    let home = temp.path().join("newer");
    let newer = other_version_coordinator(&home, flummox::jobs::VERSION + 1, 1)?;
    let output = jobs_command(&home, &["jobs"])?;
    let commands = newer.join().map_err(|_| "newer coordinator thread")??;
    check_eq(
        commands,
        vec![serde_json::json!("Snapshot")],
        "a newer coordinator is never asked to restart",
    )?;
    check(!output.status.success(), "the command fails")?;
    check(
        String::from_utf8_lossy(&output.stderr).contains("newer Flummox"),
        format!("the error names the cause: {:?}", output),
    )
}

#[test]
fn a_batch_request_queues_the_valid_items_and_lists_the_refused_ones() -> TestResult {
    let temp = tempfile::tempdir().ctx("fixture")?;
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).ctx("home")?;
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    std::fs::create_dir(&first).ctx("first game")?;
    std::fs::create_dir(&second).ctx("second game")?;
    let _service = start(&home)?;
    let options = serde_json::to_value(flummox::backend::CompressOpts::default()).ctx("options")?;
    let item = |game: Game| serde_json::json!([game, "Analyze", options]);
    let mut relative = fixture_game(&first, "relative");
    relative.install_dir = "not/absolute".into();
    let reply = {
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
            .ctx("timeout")?;
        serde_json::to_writer(
            &mut stream,
            &serde_json::json!({
                "version": flummox::jobs::VERSION,
                "command": {"EnqueueMany": {"items": [
                    item(fixture_game(&first, "first")),
                    item(relative),
                    item(fixture_game(&second, "second")),
                ]}}
            }),
        )
        .ctx("request")?;
        stream.write_all(b"\n").ctx("delimiter")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).ctx("reply")?;
        serde_json::from_str::<serde_json::Value>(&line).ctx("reply JSON")?
    };
    check(
        reply.get("error").is_some_and(serde_json::Value::is_null),
        format!("the batch itself succeeds: {reply}"),
    )?;
    let refused = reply.get("refused").and_then(serde_json::Value::as_array);
    check_eq(refused.map(Vec::len), Some(1), "one item is refused")?;
    check_eq(
        refused
            .and_then(|list| list.first())
            .and_then(|item| item.get("title"))
            .and_then(serde_json::Value::as_str),
        Some("relative"),
        "the refusal names the game",
    )?;
    let snapshot = request(&home, Request::Snapshot)?;
    let mut titles: Vec<_> = snapshot
        .jobs
        .iter()
        .map(|job| job.game.title.clone())
        .collect();
    titles.sort();
    check_eq(
        titles,
        vec!["first".to_string(), "second".to_string()],
        "both valid items are queued around the refused one",
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

fn analysis_game(root: &Path, key: &str) -> Result<Game, String> {
    let path = root.join(key);
    std::fs::create_dir_all(&path).ctx("fixture game")?;
    std::fs::write(
        path.join("data.bin"),
        b"lock fixture payload\n".repeat(20_000),
    )
    .ctx("fixture payload")?;
    Ok(fixture_game(&path, key))
}

#[test]
fn a_busy_operation_lock_delays_a_job_and_never_fails_it() -> TestResult {
    let temp = tempfile::tempdir().ctx("fixture")?;
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).ctx("home")?;
    let _service = start(&home)?;
    request(&home, Request::Snapshot)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.join("state/flummox/desktop/operation.lock"))
        .ctx("open operation.lock")?;
    lock.lock().ctx("hold the lock like a long CLI run")?;
    let first = request(
        &home,
        Request::Enqueue {
            game: analysis_game(temp.path(), "first")?,
            operation: Operation::Analyze,
            options: Default::default(),
        },
    )?;
    let first = first.jobs.last().ctx("first job")?.id;
    // Longer than the worker waits for the lock, so the job gives up once.
    let until = Instant::now() + Duration::from_secs(13);
    while Instant::now() < until {
        let job = finished_or_active(&home, first)?;
        check(
            job.phase != Phase::Failed,
            format!("a busy lock must not fail the job: {job:?}"),
        )?;
        std::thread::sleep(Duration::from_millis(250));
    }
    drop(lock);
    let job = finished(&home, first)?;
    check_eq(
        job.phase,
        Phase::Completed,
        format!("the job runs once the lock is free: {job:?}"),
    )?;
    // Two jobs one after the other: the second starts while the first
    // worker is still tearing down.
    let second = analysis_game(temp.path(), "second")?;
    let third = analysis_game(temp.path(), "third")?;
    let mut ids = Vec::new();
    for game in [second, third] {
        let snapshot = request(
            &home,
            Request::Enqueue {
                game,
                operation: Operation::Analyze,
                options: Default::default(),
            },
        )?;
        ids.push(snapshot.jobs.last().ctx("queued")?.id);
    }
    for id in ids {
        let job = finished(&home, id)?;
        check_eq(
            job.phase,
            Phase::Completed,
            format!("back-to-back jobs both complete: {job:?}"),
        )?;
    }
    Ok(())
}

fn finished_or_active(home: &Path, id: i64) -> Result<flummox::jobs::Job, String> {
    request(home, Request::Snapshot)?
        .jobs
        .into_iter()
        .find(|job| job.id == id)
        .ctx("job in snapshot")
}

#[test]
fn unreadable_saved_rows_do_not_stop_the_coordinator_starting() -> TestResult {
    let temp = tempfile::tempdir().ctx("fixture")?;
    let home = temp.path().join("home");
    let desktop = home.join("state/flummox/desktop");
    std::fs::create_dir_all(&desktop).ctx("state folder")?;
    let db = rusqlite::Connection::open(desktop.join("queue.sqlite")).ctx("seed database")?;
    db.execute_batch(
        "CREATE TABLE queue(id INTEGER PRIMARY KEY, data TEXT NOT NULL);
         CREATE TABLE settings(id INTEGER PRIMARY KEY, data TEXT NOT NULL);
         CREATE TABLE receipts(game TEXT NOT NULL, path TEXT NOT NULL, policy TEXT NOT NULL, entry TEXT NOT NULL, PRIMARY KEY(game,path));
         INSERT INTO queue VALUES(1, 'not a job');
         INSERT INTO settings VALUES(2, 'not observations');
         INSERT INTO settings VALUES(7, 'not upkeep');",
    )
    .ctx("damaged rows")?;
    drop(db);
    let _service = start(&home)?;
    let snapshot = request(&home, Request::Snapshot)?;
    check(snapshot.jobs.is_empty(), "the damaged job is skipped")
}

#[test]
fn an_argument_that_is_not_utf8_is_left_to_the_parser() -> TestResult {
    use std::os::unix::ffi::OsStrExt;
    let temp = tempfile::tempdir().ctx("fixture")?;
    let output = Command::new(env!("CARGO_BIN_EXE_flummox"))
        .arg(std::ffi::OsStr::from_bytes(b"\xff\xfe"))
        .env("HOME", temp.path())
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .output()
        .ctx("run flummox")?;
    let errors = String::from_utf8_lossy(&output.stderr).into_owned();
    check(
        !errors.contains("panicked") && output.status.code() != Some(101),
        format!("the process must not panic: {:?} {errors}", output.status),
    )?;
    check(
        !output.status.success(),
        "control: the parser still rejects it",
    )
}
