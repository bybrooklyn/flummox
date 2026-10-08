//! Landlock enforcement in a fresh executable, without inheriting live FUSE locks.

#![cfg(target_os = "linux")]

use flummox::{
    sandbox::{SandboxPlan, deny_sockets, restrict},
    testutil::{Ctx, TestResult, check, check_eq},
};

/// A skip, or a failure on a machine that is supposed to have Landlock.
fn skipped(why: &str) -> TestResult {
    check(
        std::env::var_os("FLUMMOX_REQUIRE_LANDLOCK").is_none(),
        format!("{why}, and FLUMMOX_REQUIRE_LANDLOCK is set"),
    )?;
    eprintln!("skipped: {why}");
    Ok(())
}

/// Re-emits what a child test wrote, so its skips and failures show up in the
/// parent's output, then checks that it succeeded.
fn child_passed(name: &str, result: &std::process::Output) -> TestResult {
    eprint!("{}", String::from_utf8_lossy(&result.stderr));
    print!("{}", String::from_utf8_lossy(&result.stdout));
    check(
        result.status.success(),
        format!(
            "{name} child failed: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        ),
    )
}

#[test]
fn enforcement_blocks_paths_outside_the_game() -> TestResult {
    if let Some(root) = std::env::var_os("FLUMMOX_SANDBOX_FIXTURE") {
        let root = std::path::PathBuf::from(root);
        let game = root.join("game");
        let status = restrict(&SandboxPlan::for_paths(vec![game.clone()], Vec::new()));
        if !status.is_active() {
            return skipped("Landlock is unavailable");
        }
        check(
            std::fs::read(game.join("inside.dat")).is_ok(),
            "sandbox permits the game",
        )?;
        check(
            std::fs::read(root.join("secret.dat")).is_err(),
            "sandbox refuses unrelated files",
        )?;
        return Ok(());
    }
    let temp = tempfile::tempdir().ctx("sandbox fixture")?;
    let game = temp.path().join("game");
    std::fs::create_dir(&game).ctx("game")?;
    std::fs::write(game.join("inside.dat"), b"game data").ctx("game bytes")?;
    std::fs::write(temp.path().join("secret.dat"), b"private data").ctx("outside bytes")?;
    let result = std::process::Command::new(std::env::current_exe().ctx("test executable")?)
        .args([
            "--exact",
            "enforcement_blocks_paths_outside_the_game",
            "--nocapture",
        ])
        .env("FLUMMOX_SANDBOX_FIXTURE", temp.path())
        .output()
        .ctx("isolated sandbox test")?;
    child_passed("enforcement_blocks_paths_outside_the_game", &result)
}

#[test]
fn a_worker_reaches_its_database_and_not_the_coordinator() -> TestResult {
    let append = |path: &std::path::Path| {
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, b"x"))
    };
    if let Some(root) = std::env::var_os("FLUMMOX_WORKER_SANDBOX_FIXTURE") {
        let root = std::path::PathBuf::from(root);
        let state = root.join("state/flummox");
        let database = state.join("state.sqlite");
        let status = restrict(&SandboxPlan::for_worker(&root.join("game"), &database));
        if !status.is_active() {
            return skipped("Landlock is unavailable");
        }
        check(append(&root.join("game/inside.dat")).is_ok(), "the game")?;
        check(append(&database).is_ok(), "the database")?;
        check(
            append(&state.join("state.sqlite-wal")).is_ok(),
            "the database's write-ahead log",
        )?;
        check(
            std::fs::read(state.join("compatibility/report.json")).is_ok(),
            "saved reports can be read",
        )?;
        check(
            append(&state.join("compatibility/report.json")).is_err(),
            "saved reports cannot be changed",
        )?;
        check(
            std::fs::read(state.join("desktop/queue.sqlite")).is_err(),
            "the coordinator's queue cannot be read",
        )?;
        check(
            append(&state.join("desktop/queue.sqlite")).is_err(),
            "the coordinator's queue cannot be changed",
        )?;
        check(
            std::fs::write(state.join("planted"), b"x").is_err(),
            "nothing new can be created beside the database",
        )?;
        return Ok(());
    }
    let temp = tempfile::tempdir().ctx("sandbox fixture")?;
    let state = temp.path().join("state/flummox");
    for folder in [
        "game",
        "state/flummox/desktop",
        "state/flummox/compatibility",
    ] {
        std::fs::create_dir_all(temp.path().join(folder)).ctx(folder)?;
    }
    for file in [
        "game/inside.dat",
        "state/flummox/state.sqlite",
        "state/flummox/state.sqlite-wal",
        "state/flummox/state.sqlite-shm",
        "state/flummox/desktop/queue.sqlite",
        "state/flummox/compatibility/report.json",
    ] {
        std::fs::write(temp.path().join(file), b"fixture").ctx(file)?;
    }
    // Control: before restriction every one of these is reachable.
    check(
        append(&state.join("desktop/queue.sqlite")).is_ok(),
        "the queue is writable without the sandbox",
    )?;
    let result = std::process::Command::new(std::env::current_exe().ctx("test executable")?)
        .args([
            "--exact",
            "a_worker_reaches_its_database_and_not_the_coordinator",
            "--nocapture",
        ])
        .env("FLUMMOX_WORKER_SANDBOX_FIXTURE", temp.path())
        .output()
        .ctx("isolated sandbox test")?;
    child_passed(
        "a_worker_reaches_its_database_and_not_the_coordinator",
        &result,
    )
}

#[test]
fn a_worker_cannot_reach_a_socket_by_path() -> TestResult {
    use std::os::unix::net::{UnixListener, UnixStream};
    if let Some(socket) = std::env::var_os("FLUMMOX_SOCKET_FIXTURE") {
        check(
            UnixStream::connect(&socket).is_ok(),
            "control: the socket accepts a connection before the filter",
        )?;
        deny_sockets().ctx("socket filter")?;
        let refused = UnixStream::connect(&socket);
        check(
            refused
                .as_ref()
                .err()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied),
            format!("a connection after the filter is refused: {refused:?}"),
        )?;
        // Threads started afterwards inherit the filter.
        let from_thread = std::thread::spawn(move || UnixStream::connect(&socket).is_err())
            .join()
            .map_err(|_| "thread panicked")?;
        return check(from_thread, "a new thread is refused too");
    }
    let temp = tempfile::tempdir().ctx("socket fixture")?;
    let socket = temp.path().join("control.sock");
    let _listener = UnixListener::bind(&socket).ctx("listen")?;
    let result = std::process::Command::new(std::env::current_exe().ctx("test executable")?)
        .args([
            "--exact",
            "a_worker_cannot_reach_a_socket_by_path",
            "--nocapture",
        ])
        .env("FLUMMOX_SOCKET_FIXTURE", &socket)
        .output()
        .ctx("isolated socket test")?;
    child_passed("a_worker_cannot_reach_a_socket_by_path", &result)
}

#[test]
fn a_sandboxed_scan_cannot_see_other_processes_but_a_prestarted_one_can() -> TestResult {
    use flummox::busy::{BackgroundScan, ProcFs, Usage, usage};
    const NAME: &str = "a_sandboxed_scan_cannot_see_other_processes_but_a_prestarted_one_can";
    if let Some(dir) = std::env::var_os("FLUMMOX_PROC_FIXTURE") {
        let dir = std::path::PathBuf::from(dir);
        let parent = std::os::unix::process::parent_id();
        let exe_link = format!("/proc/{parent}/exe");
        let exe =
            std::fs::read_link(&exe_link).ctx("control: the parent's exe before the sandbox")?;
        let game = exe.parent().ctx("the parent's directory")?.to_path_buf();
        check(
            matches!(usage(&game, &ProcFs::new()), Usage::InUse(_)),
            "control: a scan before the sandbox sees the parent",
        )?;
        let scan = BackgroundScan::start(game.clone(), std::time::Duration::from_millis(100));
        let status = restrict(&SandboxPlan::for_paths(vec![dir], vec!["/proc".into()]));
        if !status.is_active() {
            return skipped("Landlock is unavailable");
        }
        check(
            std::fs::read_link(&exe_link).is_err(),
            "after the sandbox another process's exe link is unreadable",
        )?;
        check_eq(
            usage(&game, &ProcFs::new()),
            Usage::Unknown,
            "an in-process scan reports that it cannot tell",
        )?;
        std::thread::sleep(std::time::Duration::from_millis(400));
        return check(
            matches!(scan.latest(), Usage::InUse(_)),
            format!(
                "the scanner started before the sandbox still sees it: {:?}",
                scan.latest()
            ),
        );
    }
    let temp = tempfile::tempdir().ctx("fixture")?;
    let result = std::process::Command::new(std::env::current_exe().ctx("test executable")?)
        .args(["--exact", NAME, "--nocapture"])
        .env("FLUMMOX_PROC_FIXTURE", temp.path())
        .output()
        .ctx("isolated scan test")?;
    child_passed(NAME, &result)
}
