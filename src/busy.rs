//! Detects whether anything is using an install directory.
//!
//! Launcher metadata is the first signal, but it lies: this machine has three
//! Steam apps flagged "running" with nothing running. So before touching a
//! directory we also look for a live process with a file open inside it.

// `getuid` is the only unsafe call here, used to skip other users' processes.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

/// A process, reduced to the paths that tell us whether it is using a game.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcInfo {
    /// Process id.
    pub pid: i32,
    /// `comm`, for the message shown to the user.
    pub name: String,
    /// The executable.
    pub exe: Option<PathBuf>,
    /// The working directory.
    pub cwd: Option<PathBuf>,
    /// The process's root, which is not `/` inside a container such as
    /// pressure-vessel.
    pub root: Option<PathBuf>,
    /// Open files, best effort.
    pub open_files: Vec<PathBuf>,
}

impl ProcInfo {
    /// Whether any of this process's paths is inside `dir`.
    ///
    /// Steam's Proton games run inside pressure-vessel, whose mount namespace
    /// gives paths relative to its own root, so the directory is also tried
    /// with that root stripped.
    pub fn uses_dir(&self, dir: &Path) -> bool {
        let paths = self
            .exe
            .iter()
            .chain(self.cwd.iter())
            .chain(self.open_files.iter());
        for p in paths {
            if p.starts_with(dir) {
                return true;
            }
            if let Some(root) = &self.root
                && root != Path::new("/")
                && let Ok(rel) = p.strip_prefix(root)
                && Path::new("/").join(rel).starts_with(dir)
            {
                return true;
            }
        }
        false
    }
}

/// Where process information comes from.
///
/// Tests use a fake so busy detection can be exercised without real
/// processes.
pub trait ProcSource {
    /// Every process this source can see.
    fn processes(&self) -> Vec<ProcInfo>;

    /// [`processes`](Self::processes) plus whether the list can be trusted.
    fn scan(&self) -> Scan {
        Scan {
            processes: self.processes(),
            readable: true,
        }
    }
}

/// A process list and whether it says anything about what is running.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    /// The processes found.
    pub processes: Vec<ProcInfo>,
    /// False when `/proc` could not be listed, or when other processes of this
    /// user exist and none of their links could be read.
    pub readable: bool,
}

/// What a scan concluded about a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Usage {
    /// Nothing is using it.
    Free,
    /// This process is, described for the user.
    InUse(String),
    /// The scan could not see processes, so "free" would be a guess.
    Unknown,
}

impl Usage {
    /// The reason to stay paused, or `None` when work may continue.
    ///
    /// `Unknown` pauses, because a caller that treated it as free would
    /// rewrite a running game's files.
    pub fn blocking(&self) -> Option<String> {
        match self {
            Self::Free => None,
            Self::InUse(who) => Some(who.clone()),
            Self::Unknown => Some("process information is unavailable".to_owned()),
        }
    }
}

/// Reads processes from a `/proc` mount.
#[derive(Debug, Clone)]
pub struct ProcFs {
    root: PathBuf,
    /// Only processes with this uid are inspected; others' `fd` entries are
    /// unreadable anyway.
    uid: u32,
}

impl Default for ProcFs {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcFs {
    /// Reads the real `/proc` for the current user's processes.
    pub fn new() -> Self {
        // SAFETY: getuid() takes no arguments and cannot fail.
        let uid = unsafe { libc::getuid() };
        Self {
            root: PathBuf::from("/proc"),
            uid,
        }
    }

    /// Reads a different `/proc`-shaped tree, for tests.
    pub fn with_root(root: impl Into<PathBuf>, uid: u32) -> Self {
        Self {
            root: root.into(),
            uid,
        }
    }

    fn read_one(&self, dir: &Path, pid: i32) -> Option<ProcInfo> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(dir).ok()?;
        if meta.uid() != self.uid {
            return None;
        }
        let name = std::fs::read_to_string(dir.join("comm"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let open_files = std::fs::read_dir(dir.join("fd"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| std::fs::read_link(e.path()).ok())
                    // Sockets and pipes come back as "socket:[12345]", which
                    // is not a path and never matches a game directory.
                    .filter(|p| p.is_absolute())
                    .collect()
            })
            .unwrap_or_default();
        Some(ProcInfo {
            pid,
            name,
            exe: std::fs::read_link(dir.join("exe")).ok(),
            cwd: std::fs::read_link(dir.join("cwd")).ok(),
            root: std::fs::read_link(dir.join("root")).ok(),
            open_files,
        })
    }
}

impl ProcSource for ProcFs {
    fn processes(&self) -> Vec<ProcInfo> {
        self.scan().processes
    }

    fn scan(&self) -> Scan {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::error!(
                    proc = %self.root.display(),
                    error = %e,
                    "cannot read /proc, so a running game cannot be detected"
                );
                return Scan::default();
            }
        };
        let own = std::process::id() as i32;
        let processes: Vec<ProcInfo> = entries
            .flatten()
            .filter_map(|entry| {
                let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
                self.read_one(&entry.path(), pid)
            })
            .collect();
        // Landlock refuses the ptrace-level check behind these links for every
        // process outside the caller's domain, so a sandboxed caller reads
        // its own links and nothing else's.
        let seen = processes.iter().filter(|p| p.pid != own).count();
        let readable = processes
            .iter()
            .filter(|p| p.pid != own)
            .filter(|p| p.exe.is_some() || p.cwd.is_some() || p.root.is_some())
            .count();
        Scan {
            processes,
            readable: seen == 0 || readable > 0,
        }
    }
}

/// Who is using `dir`, or [`Usage::Unknown`] when the scan could not tell.
pub fn usage(dir: &Path, source: &dyn ProcSource) -> Usage {
    let own = std::process::id() as i32;
    let scan = source.scan();
    if let Some(p) = scan
        .processes
        .iter()
        .find(|p| p.pid != own && p.uses_dir(dir))
    {
        return Usage::InUse(format!("{} (pid {})", p.name, p.pid));
    }
    if scan.readable {
        Usage::Free
    } else {
        Usage::Unknown
    }
}

/// Scans `/proc` on a background thread and keeps the latest answer.
///
/// Landlock applies to the calling thread and the threads it starts later, so
/// a scanner started before [`crate::sandbox::restrict`] keeps its view of
/// other processes. Start it first, then restrict, and have the job read
/// [`latest`](Self::latest).
pub struct BackgroundScan {
    latest: std::sync::Arc<std::sync::Mutex<Usage>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BackgroundScan {
    /// Scans once before returning, then again every `every`.
    pub fn start(dir: PathBuf, every: std::time::Duration) -> Self {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};
        let latest = Arc::new(Mutex::new(usage(&dir, &ProcFs::new())));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("busy-scan".to_owned())
            .spawn({
                let (latest, stop) = (Arc::clone(&latest), Arc::clone(&stop));
                move || {
                    let step = std::time::Duration::from_millis(100);
                    while !stop.load(Ordering::Relaxed) {
                        let mut waited = std::time::Duration::ZERO;
                        while waited < every && !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(step);
                            waited += step;
                        }
                        let now = usage(&dir, &ProcFs::new());
                        if let Ok(mut slot) = latest.lock() {
                            *slot = now;
                        }
                    }
                }
            })
            .ok();
        Self {
            latest,
            stop,
            thread,
        }
    }

    /// The newest scan result. `Unknown` when the thread could not start.
    pub fn latest(&self) -> Usage {
        if self.thread.is_none() {
            return Usage::Unknown;
        }
        self.latest
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or(Usage::Unknown)
    }
}

impl Drop for BackgroundScan {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The first process using `dir`, described for the user.
///
/// Returns `None` when the directory looks free.
pub fn process_using(dir: &Path, source: &dyn ProcSource) -> Option<String> {
    let own = std::process::id() as i32;
    source
        .processes()
        .into_iter()
        .find(|p| p.pid != own && p.uses_dir(dir))
        .map(|p| format!("{} (pid {})", p.name, p.pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    struct Fake(Vec<ProcInfo>);

    impl ProcSource for Fake {
        fn processes(&self) -> Vec<ProcInfo> {
            self.0.clone()
        }
    }

    #[test]
    fn finds_a_process_with_the_game_open() -> TestResult {
        let game = Path::new("/games/Terraria");
        let src = Fake(vec![
            ProcInfo {
                pid: 10,
                name: "firefox".to_owned(),
                exe: Some(PathBuf::from("/usr/lib/firefox/firefox")),
                open_files: vec![PathBuf::from("/home/u/.cache/x")],
                ..ProcInfo::default()
            },
            ProcInfo {
                pid: 11,
                name: "Terraria.bin".to_owned(),
                exe: Some(PathBuf::from("/games/Terraria/Terraria.bin.x86_64")),
                ..ProcInfo::default()
            },
        ]);
        check_eq(
            process_using(game, &src),
            Some("Terraria.bin (pid 11)".to_owned()),
            "the process whose exe is inside the game directory should be named",
        )?;
        check_eq(
            process_using(Path::new("/games/Portal"), &src),
            None,
            "an unrelated directory should look free",
        )
    }

    #[test]
    fn sees_through_a_pressure_vessel_root() -> TestResult {
        // Inside the container the game lives at the same path, but every
        // link is reported below the container's root.
        let proc = ProcInfo {
            pid: 12,
            name: "wine".to_owned(),
            root: Some(PathBuf::from("/newroot")),
            open_files: vec![PathBuf::from("/newroot/games/Portal 2/bin/x.so")],
            ..ProcInfo::default()
        };
        check(
            proc.uses_dir(Path::new("/games/Portal 2")),
            "stripping the container root should reveal the game directory",
        )?;
        check(
            !proc.uses_dir(Path::new("/games/Terraria")),
            "a different game should not match",
        )
    }

    #[test]
    fn reads_a_fake_proc_tree() -> TestResult {
        let tmp = tempfile::tempdir().ctx("make a temporary directory")?;
        let proc = tmp.path().join("proc/42");
        std::fs::create_dir_all(proc.join("fd")).ctx("create the fake fd directory")?;
        std::fs::write(proc.join("comm"), "game\n").ctx("write the fake comm file")?;
        // The uid filter is what keeps us from reading other users' processes;
        // our own uid is what the fixture gets.
        // SAFETY: getuid() takes no arguments and cannot fail.
        let uid = unsafe { libc::getuid() };
        let source = ProcFs::with_root(tmp.path().join("proc"), uid);
        let found = source.processes();
        check_eq(found.len(), 1, "the fixture holds exactly one process")?;
        check_eq(
            found.first().map(|p| p.name.as_str()),
            Some("game"),
            "the process name comes from the comm file",
        )
    }
}
