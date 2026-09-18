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

/// Watches each library's `steamapps` directory, reporting apps as they settle.
///
/// Runs until `cancel` is set. `on_ready` is called once per app each time it
/// becomes settled, not once per manifest write.
pub fn run(
    libraries: &[PathBuf],
    cancel: &AtomicBool,
    mut on_ready: impl FnMut(&App),
) -> io::Result<()> {
    let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;

    let mut settled: HashMap<u32, bool> = HashMap::new();
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
            settled.insert(app.appid, is_settled(&app));
        }
        watched.push(library.clone());
    }
    if watched.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no Steam library could be watched",
        ));
    }

    let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); BUF_BYTES];
    while !cancel.load(Ordering::Relaxed) {
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        match poll(&mut fds, Some(&WAIT)) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        if !drain(&fd, &mut buf)? {
            continue;
        }
        // One manifest changed, so every library is re-read. A library holds
        // tens of manifests, and reading them is cheaper than tracking which
        // watch descriptor belongs to which directory.
        for library in &watched {
            for app in steam::apps_in_library(library).unwrap_or_default() {
                let now = is_settled(&app);
                let before = settled.insert(app.appid, now).unwrap_or(false);
                if now && !before {
                    on_ready(&app);
                }
            }
        }
    }
    Ok(())
}

/// Reads the pending events, reporting whether any named a manifest.
fn drain(fd: &impl rustix::fd::AsFd, buf: &mut [std::mem::MaybeUninit<u8>]) -> io::Result<bool> {
    let mut saw_manifest = false;
    let mut reader = inotify::Reader::new(fd, buf);
    loop {
        match reader.next() {
            Ok(event) => {
                if event
                    .file_name()
                    .and_then(|name| name.to_str().ok())
                    .and_then(appid_from_manifest)
                    .is_some()
                {
                    saw_manifest = true;
                }
            }
            Err(rustix::io::Errno::WOULDBLOCK) => return Ok(saw_manifest),
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
