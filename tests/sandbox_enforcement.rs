//! Landlock enforcement in a fresh executable, without inheriting live FUSE locks.

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
