//! Watches Steam's manifests and reports a game when its download settles.
//!
//! Steam rewrites `appmanifest_<appid>.acf` repeatedly while it downloads,
//! stages and commits. This reads the file after each change and reports an
//! app once it is installed with nothing left to transfer, so a caller can
//! compress what the filesystem's write heuristic left behind.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::inotify;

use crate::launchers::steam::{self, App, state_flags};

/// How long a wait lasts before the loop checks whether it should stop.
///
/// `signal-hook` registers handlers with `SA_RESTART`, so a blocking read is
/// resumed after a signal and never sees the flag. Waking regularly is what
/// lets Ctrl-C and `systemctl stop` end the watcher.
const WAIT: Timespec = Timespec {
    tv_sec: 1,
    tv_nsec: 0,
};

/// How many bytes of events to read at a time.
const BUF_BYTES: usize = 4096;

/// Whether Steam has finished with an app.
///
/// Installed alone is not enough: Steam sets that bit while an update is still
/// downloading, and compressing then would rewrite files it is about to
/// replace.
pub fn is_settled(app: &App) -> bool {
    app.state_flags & state_flags::FULLY_INSTALLED != 0
        && !steam::is_working(app.state_flags)
        && !app.transfer_pending()
}

/// The appid a manifest file name names.
///
/// `None` for anything else, including the temporary files Steam writes
/// alongside a manifest before renaming it into place.
pub fn appid_from_manifest(name: &str) -> Option<u32> {
    name.strip_prefix("appmanifest_")?
        .strip_suffix(".acf")?
        .parse()
        .ok()
}

/// Waits before each retry of an `on_ready` that failed: three retries after
/// the first attempt.
pub const RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(10),
    Duration::from_secs(60),
    Duration::from_secs(300),
];

/// An app whose `on_ready` failed and is waiting for its next attempt.
struct Retry {
    app: App,
    /// Attempts made so far.
    attempts: usize,
    due: Instant,
}

/// What the watcher last saw of an app.
struct Seen {
    settled: bool,
    build: Option<String>,
}

/// Whether an app is newly ready: settled now, and either it was not settled
/// before or it settled on a different build.
///
/// An update that starts and finishes while a caller is busy with another
/// game leaves the app settled at both readings, and only the build changes.
fn newly_ready(before: Option<&Seen>, app: &App) -> bool {
    is_settled(app) && before.is_none_or(|seen| !seen.settled || seen.build != app.build)
}

/// Watches each library's `steamapps` directory, reporting apps as they settle.
///
/// Runs until `cancel` is set, or fails once every watched folder is gone.
/// `on_ready` is called once per app each time it becomes settled or settles
/// on a new build, not once per manifest write. An app counts as handled only
/// when `on_ready` returns `Ok`. After an error it is retried on the
/// [`RETRY_BACKOFF`] schedule, and once that is spent, at the next manifest change.
pub fn run(
    libraries: &[PathBuf],
    cancel: &AtomicBool,
    on_ready: impl FnMut(&App) -> anyhow::Result<()>,
) -> io::Result<()> {
    run_with_backoff(libraries, cancel, &RETRY_BACKOFF, on_ready)
}

/// [`run`] with the waits between retries of a failing `on_ready`.
pub fn run_with_backoff(
    libraries: &[PathBuf],
    cancel: &AtomicBool,
    backoff: &[Duration],
    mut on_ready: impl FnMut(&App) -> anyhow::Result<()>,
) -> io::Result<()> {
    let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;

    let mut seen: HashMap<u32, Seen> = HashMap::new();
    let mut watched: Vec<PathBuf> = Vec::new();
    for library in libraries {
        let steamapps = library.join("steamapps");
        if !steamapps.is_dir() {
            continue;
        }
        // CLOSE_WRITE covers a manifest written in place, MOVED_TO covers one
        // written elsewhere and renamed over the old file.
        inotify::add_watch(
            &fd,
            &steamapps,
            inotify::WatchFlags::CLOSE_WRITE | inotify::WatchFlags::MOVED_TO,
        )?;
        // Seeded from what is installed now. Without this, starting the
        // watcher on a full library would report every finished game at once.
        for app in steam::apps_in_library(library).unwrap_or_default() {
            seen.insert(
                app.appid,
                Seen {
                    settled: is_settled(&app),
                    build: app.build.clone(),
                },
            );
        }
        watched.push(library.clone());
    }
    if watched.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no Steam library could be watched",
        ));
    }

    let mut alive = watched.len();
    let mut retries: HashMap<u32, Retry> = HashMap::new();
    let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); BUF_BYTES];
    while !cancel.load(Ordering::Relaxed) {
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let timeout = wait_for(&retries);
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => {
                retry_due(&mut retries, &mut seen, backoff, &mut on_ready);
                continue;
            }
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let drained = drain(&fd, &mut buf)?;
        alive = alive.saturating_sub(drained.lost);
        if alive == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "every watched Steam folder has gone away",
            ));
        }
        retry_due(&mut retries, &mut seen, backoff, &mut on_ready);
        if !drained.rescan {
            continue;
        }
        // One manifest changed, so every library is re-read. A library holds
        // tens of manifests, and reading them is cheaper than tracking which
        // watch descriptor belongs to which directory.
        for library in &watched {
            for app in steam::apps_in_library(library).unwrap_or_default() {
                let settled = is_settled(&app);
                let ready = newly_ready(seen.get(&app.appid), &app);
                // An app waiting on a backoff keeps waiting while its build is
                // unchanged, so another game's manifest cannot cut the wait short.
                let waiting = retries.get(&app.appid).is_some_and(|retry| {
                    retry.app.build == app.build && retry.due > Instant::now()
                });
                if ready && waiting {
                    continue;
                }
                // An app that is ready is recorded as handled only once
                // `on_ready` succeeds.
                seen.insert(
                    app.appid,
                    Seen {
                        settled: settled && !ready,
                        build: app.build.clone(),
                    },
                );
                if !settled {
                    retries.remove(&app.appid);
                }
                if ready {
                    attempt(&app, 1, &mut retries, &mut seen, backoff, &mut on_ready);
                }
            }
        }
    }
    Ok(())
}

/// Runs `on_ready` for `app`, which has made `attempts - 1` earlier tries.
/// Success marks it settled. A failure schedules the next try, or once the
/// backoff is spent logs the error and leaves the app for the next manifest change.
fn attempt(
    app: &App,
    attempts: usize,
    retries: &mut HashMap<u32, Retry>,
    seen: &mut HashMap<u32, Seen>,
    backoff: &[Duration],
    on_ready: &mut impl FnMut(&App) -> anyhow::Result<()>,
) {
    match on_ready(app) {
        Ok(()) => {
            retries.remove(&app.appid);
            seen.insert(
                app.appid,
                Seen {
                    settled: true,
                    build: app.build.clone(),
                },
            );
        }
        Err(error) => match backoff.get(attempts.saturating_sub(1)) {
            Some(wait) => {
                tracing::debug!(appid = app.appid, attempts, %error, "retrying a finished download");
                retries.insert(
                    app.appid,
                    Retry {
                        app: app.clone(),
                        attempts,
                        due: Instant::now() + *wait,
                    },
                );
            }
            None => {
                retries.remove(&app.appid);
                tracing::warn!(
                    appid = app.appid,
                    name = %app.name,
                    attempts,
                    "gave up on a finished download until its manifest changes: {error:#}"
                );
            }
        },
    }
}

/// Retries every app whose wait is over.
fn retry_due(
    retries: &mut HashMap<u32, Retry>,
    seen: &mut HashMap<u32, Seen>,
    backoff: &[Duration],
    on_ready: &mut impl FnMut(&App) -> anyhow::Result<()>,
) {
    let now = Instant::now();
    let due: Vec<u32> = retries
        .iter()
        .filter(|(_, retry)| retry.due <= now)
        .map(|(appid, _)| *appid)
        .collect();
    for appid in due {
        if let Some(retry) = retries.remove(&appid) {
            attempt(
                &retry.app,
                retry.attempts + 1,
                retries,
                seen,
                backoff,
                on_ready,
            );
        }
    }
}

/// How long to wait for events: the regular interval, or less when a retry is
/// due sooner.
fn wait_for(retries: &HashMap<u32, Retry>) -> Timespec {
    let now = Instant::now();
    let soonest = retries
        .values()
        .map(|retry| retry.due.saturating_duration_since(now))
        .min();
    match soonest {
        Some(left) if left < Duration::from_secs(1) => Timespec {
            tv_sec: 0,
            tv_nsec: i64::from(left.subsec_nanos()).max(1_000_000),
        },
        _ => WAIT,
    }
}

/// What the pending events asked for.
struct Drained {
    /// A manifest changed, or the kernel dropped events and any might have.
    rescan: bool,
    /// Watches the kernel removed, because their folder went away.
    lost: usize,
}

/// Reads the pending events.
fn drain(fd: &impl rustix::fd::AsFd, buf: &mut [std::mem::MaybeUninit<u8>]) -> io::Result<Drained> {
    use inotify::ReadFlags;
    let mut drained = Drained {
        rescan: false,
        lost: 0,
    };
    let mut reader = inotify::Reader::new(fd, buf);
    loop {
        match reader.next() {
            Ok(event) => {
                if event
                    .file_name()
                    .and_then(|name| name.to_str().ok())
                    .and_then(appid_from_manifest)
                    .is_some()
                    || event.events().contains(ReadFlags::QUEUE_OVERFLOW)
                {
                    drained.rescan = true;
                }
                if event.events().contains(ReadFlags::IGNORED) {
                    drained.lost += 1;
                }
            }
            Err(rustix::io::Errno::WOULDBLOCK) => return Ok(drained),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::testutil::{TestResult, check, check_eq};

    use super::*;

    /// An app with the fields this module reads, and nothing real behind it.
    fn app(flags: u32, download: (u64, u64), stage: (u64, u64)) -> App {
        App {
            appid: 1,
            name: "Test".to_owned(),
            state_flags: flags,
            install_dir: PathBuf::from("/nonexistent"),
            build: None,
            target_build: None,
            size_on_disk: None,
            download,
            stage,
            library: PathBuf::from("/nonexistent"),
        }
    }

    #[test]
    fn a_manifest_name_yields_its_appid() -> TestResult {
        check_eq(
            appid_from_manifest("appmanifest_105600.acf"),
            Some(105_600),
            "a manifest",
        )?;
        check_eq(appid_from_manifest("appmanifest_.acf"), None, "no digits")?;
        check_eq(
            appid_from_manifest("libraryfolders.vdf"),
            None,
            "not a manifest",
        )?;
        check_eq(
            appid_from_manifest("appmanifest_105600.acf.tmp"),
            None,
            "a temporary file",
        )?;
        check_eq(
            appid_from_manifest("appmanifest_-1.acf"),
            None,
            "not an appid",
        )
    }

    #[test]
    fn an_install_still_being_worked_on_is_not_settled() -> TestResult {
        let downloading = app(
            state_flags::FULLY_INSTALLED | state_flags::DOWNLOADING,
            (0, 0),
            (0, 0),
        );
        check(
            !is_settled(&downloading),
            "a download in progress is not settled",
        )?;
        let validating = app(
            state_flags::FULLY_INSTALLED | state_flags::VALIDATING,
            (0, 0),
            (0, 0),
        );
        check(
            !is_settled(&validating),
            "a validating install is not settled",
        )?;
        let uninstalled = app(state_flags::UNINSTALLED, (0, 0), (0, 0));
        check(
            !is_settled(&uninstalled),
            "an uninstalled app is not settled",
        )
    }

    #[test]
    fn bytes_left_to_transfer_mean_it_is_not_settled() -> TestResult {
        let fetching = app(state_flags::FULLY_INSTALLED, (100, 20), (0, 0));
        check(!is_settled(&fetching), "bytes left to fetch is not settled")?;
        let staging = app(state_flags::FULLY_INSTALLED, (0, 0), (100, 20));
        check(!is_settled(&staging), "bytes left to stage is not settled")?;
        let done = app(state_flags::FULLY_INSTALLED, (0, 0), (0, 0));
        check(
            is_settled(&done),
            "installed with nothing pending is settled",
        )
    }
}
