//! Landlock enforcement in a fresh executable, without inheriting live FUSE locks.

#![cfg(target_os = "linux")]

use flummox::{
    sandbox::{SandboxPlan, restrict},
    testutil::{Ctx, TestResult, check},
};

#[test]
fn enforcement_blocks_paths_outside_the_game() -> TestResult {
    if let Some(root) = std::env::var_os("FLUMMOX_SANDBOX_FIXTURE") {
        let root = std::path::PathBuf::from(root);
        let game = root.join("game");
        let status = restrict(&SandboxPlan::for_paths(vec![game.clone()], Vec::new()));
        if !status.is_active() {
            eprintln!("skipped: Landlock is unavailable");
            return Ok(());
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
    check(
        result.status.success(),
        format!(
            "sandbox child failed: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        ),
    )
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
            eprintln!("skipped: Landlock is unavailable");
            return Ok(());
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
    check(
        result.status.success(),
        format!(
            "sandbox child failed: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        ),
    )
}
