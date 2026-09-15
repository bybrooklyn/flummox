//! Detects whether anything is using an install directory.
//!
//! Launcher metadata is the first signal, but it lies: this machine has three
//! Steam apps flagged "running" with nothing running. So before touching a
//! directory we also look for a live process with a file open inside it.

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
        Self { root: PathBuf::from("/proc"), uid }
    }

    /// Reads a different `/proc`-shaped tree, for tests.
    pub fn with_root(root: impl Into<PathBuf>, uid: u32) -> Self {
        Self { root: root.into(), uid }
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
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
                self.read_one(&entry.path(), pid)
            })
            .collect()
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
    use gc_testutil::{Ctx, TestResult, check, check_eq};

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
