//! The Steam manifest watcher against a real inotify watch on a temporary
//! library.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use flummox::testutil::{Ctx, TestResult, check, check_eq};
use flummox::watch;

const WAIT: Duration = Duration::from_secs(10);

/// Writes a settled manifest the way Steam does, through a rename.
fn write_manifest(library: &Path, appid: u32, build: u32) -> Result<(), String> {
    let steamapps = library.join("steamapps");
    let text = format!(
        "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"Game {appid}\"\n\
         \t\"StateFlags\"\t\t\"4\"\n\t\"installdir\"\t\t\"Game{appid}\"\n\
         \t\"buildid\"\t\t\"{build}\"\n}}\n"
    );
    let temp = steamapps.join(format!("appmanifest_{appid}.acf.tmp"));
    std::fs::write(&temp, text).ctx("write the manifest")?;
    std::fs::rename(&temp, steamapps.join(format!("appmanifest_{appid}.acf")))
        .ctx("rename the manifest into place")
}

fn library() -> Result<tempfile::TempDir, String> {
    let tmp = tempfile::tempdir().ctx("library")?;
    std::fs::create_dir(tmp.path().join("steamapps")).ctx("steamapps")?;
    Ok(tmp)
}

#[test]
fn an_update_that_finishes_while_another_game_is_handled_is_not_lost() -> TestResult {
    let tmp = library()?;
    write_manifest(tmp.path(), 200, 1)?;
    let cancel = AtomicBool::new(false);
    let (ready_tx, ready_rx) = mpsc::channel::<u32>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let outcome = std::thread::scope(|scope| -> Result<Vec<u32>, String> {
        let (library, cancel) = (tmp.path().to_path_buf(), &cancel);
        let watcher = scope.spawn(move || {
            watch::run(&[library], cancel, |app| {
                ready_tx.send(app.appid).ok();
                if app.appid == 100 {
                    // Stands for a compress job that takes a while.
                    release_rx.recv_timeout(WAIT).ok();
                }
                Ok(())
            })
        });
        let mut seen = Vec::new();
        let result = (|| {
            // The watch is added on the watcher's own thread.
            std::thread::sleep(Duration::from_millis(500));
            write_manifest(tmp.path(), 100, 1)?;
            seen.push(ready_rx.recv_timeout(WAIT).ctx("game 100 becomes ready")?);
            // Game 200 updates completely while 100 is being handled.
            write_manifest(tmp.path(), 200, 2)?;
            std::thread::sleep(Duration::from_millis(300));
            release_tx.send(()).ctx("release game 100")?;
            seen.push(
                ready_rx
                    .recv_timeout(WAIT)
                    .ctx("game 200 is reported again")?,
            );
            Ok::<(), String>(())
        })();
        cancel.store(true, Ordering::Relaxed);
        let ran = watcher.join().map_err(|_| "the watcher panicked")?;
        ran.ctx("the watcher stops cleanly")?;
        result.map(|()| seen)
    })?;
    check_eq(
        outcome,
        vec![100, 200],
        "both games were reported, in order",
    )
}

#[test]
fn the_watcher_fails_when_every_watched_folder_goes_away() -> TestResult {
    let tmp = library()?;
    let cancel = AtomicBool::new(false);
    let (done_tx, done_rx) = mpsc::channel::<std::io::Result<()>>();
    std::thread::scope(|scope| -> TestResult {
        scope.spawn(|| {
            let result = watch::run(&[tmp.path().to_path_buf()], &cancel, |_| Ok(()));
            done_tx.send(result).ok();
        });
        std::thread::sleep(Duration::from_millis(300));
        std::fs::remove_dir_all(tmp.path().join("steamapps")).ctx("remove steamapps")?;
        let got = done_rx.recv_timeout(Duration::from_secs(5));
        cancel.store(true, Ordering::Relaxed);
        let result = got.ctx("the watcher returns once its folder is gone")?;
        check(result.is_err(), "and it returns an error, not success")
    })
}

#[test]
fn a_failed_callback_is_retried_and_a_success_settles_the_game() -> TestResult {
    let tmp = library()?;
    let cancel = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<u32>();
    let backoff = [Duration::from_millis(50), Duration::from_millis(50)];
    let calls = std::thread::scope(|scope| -> Result<usize, String> {
        let (library, cancel) = (tmp.path().to_path_buf(), &cancel);
        let watcher = scope.spawn(move || {
            let mut calls = 0u32;
            watch::run_with_backoff(&[library], cancel, &backoff, |app| {
                calls += 1;
                tx.send(app.appid).ok();
                if calls == 1 {
                    anyhow::bail!("the coordinator was not reachable");
                }
                Ok(())
            })
        });
        let result = (|| {
            std::thread::sleep(Duration::from_millis(500));
            write_manifest(tmp.path(), 100, 1)?;
            let mut got = vec![rx.recv_timeout(WAIT).ctx("the first attempt")?];
            got.push(rx.recv_timeout(WAIT).ctx("the retry after the failure")?);
            // A third call would mean a settled game was reported again.
            check(
                rx.recv_timeout(Duration::from_millis(500)).is_err(),
                "a game whose callback succeeded is not reported again",
            )?;
            Ok::<usize, String>(got.len())
        })();
        cancel.store(true, Ordering::Relaxed);
        let ran = watcher.join().map_err(|_| "the watcher panicked")?;
        ran.ctx("the watcher stops cleanly")?;
        result
    })?;
    check_eq(calls, 2, "one failure and one success")
}

#[test]
fn retries_are_bounded_and_a_manifest_change_starts_again() -> TestResult {
    let tmp = library()?;
    let cancel = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<u32>();
    let backoff = [Duration::from_millis(30), Duration::from_millis(30)];
    std::thread::scope(|scope| -> TestResult {
        let (library, cancel) = (tmp.path().to_path_buf(), &cancel);
        let watcher = scope.spawn(move || {
            watch::run_with_backoff(&[library], cancel, &backoff, |app| {
                tx.send(app.appid).ok();
                anyhow::bail!("always failing")
            })
        });
        let result = (|| {
            std::thread::sleep(Duration::from_millis(500));
            write_manifest(tmp.path(), 100, 1)?;
            for attempt in 1..=3 {
                rx.recv_timeout(WAIT)
                    .ctx(format!("attempt {attempt} of the first round"))?;
            }
            check(
                rx.recv_timeout(Duration::from_millis(600)).is_err(),
                "after the bounded attempts it waits for a manifest change",
            )?;
            write_manifest(tmp.path(), 200, 1)?;
            // The new game fires, and so does the one that gave up.
            let first = rx.recv_timeout(WAIT).ctx("the first call after the change")?;
            let second = rx.recv_timeout(WAIT).ctx("the second call after the change")?;
            check(
                first == 100 || second == 100,
                format!("game 100 is tried again, got {first} and {second}"),
            )?;
            Ok(())
        })();
        cancel.store(true, Ordering::Relaxed);
        let ran = watcher.join().map_err(|_| "the watcher panicked")?;
        ran.ctx("the watcher stops cleanly")?;
        result
    })
}
