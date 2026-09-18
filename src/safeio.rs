//! Opening game files without trusting the path.
//!
//! A job walks a directory and then, moments later, opens what it found. In
//! between, anything could replace a component of that path with a symlink,
//! and an ordinary `File::open` would follow it, letting a job rewrite a file
//! outside the game. The window is small and a game directory is usually only
//! writable by its owner, but the fix costs nothing: hold the install
//! directory open once, then open every file *relative to that handle* with
//! the kernel refusing both symlinks and any escape above it.
//!
//! [`Anchor::open_file`] uses `openat2(2)` with `RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS` (Linux 5.6+). Where that syscall is missing it falls
//! back to `openat(2)` with `O_NOFOLLOW`, which still refuses a symlink as the
//! final component but cannot police the directories above it; callers can
//! check [`Anchor::fully_resolved`] to know which guarantee they have.

// Opening files is this module's whole job, and `openat2` comes through
// `rustix`, so the unsafe here is in the test helper rather than the API.
#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags, ResolveFlags};

/// An install directory, held open as the anchor for every file below it.
#[derive(Debug)]
pub struct Anchor {
    dir: OwnedFd,
    path: PathBuf,
    fully_resolved: bool,
}

impl Anchor {
    /// Opens a directory to anchor later file opens against.
    ///
    /// The directory itself is opened with symlinks allowed: the caller chose
    /// it, and a Steam library legitimately can be reached through one.
    pub fn open(path: &Path) -> io::Result<Self> {
        let dir = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        // Probe once so callers can report which guarantee is in force,
        // instead of discovering it per file.
        let fully_resolved = match rustix::fs::openat2(
            &dir,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH,
        ) {
            Ok(_) => true,
            Err(e) if e == rustix::io::Errno::NOSYS || e == rustix::io::Errno::OPNOTSUPP => false,
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            dir,
            path: path.to_path_buf(),
            fully_resolved,
        })
    }

    /// The directory this anchor was opened on.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the kernel is enforcing the full guarantee.
    ///
    /// True when `openat2` is available, so no component of a path may be a
    /// symlink and nothing may resolve above the anchor. False on kernels
    /// before 5.6, where only the final component is checked.
    pub fn fully_resolved(&self) -> bool {
        self.fully_resolved
    }

    /// Opens a file below the anchor, read-only.
    ///
    /// `rel` must stay inside: no absolute paths, no `..`, no symlinks.
    pub fn open_file(&self, rel: &Path) -> io::Result<File> {
        self.open_with(rel, OFlags::RDONLY)
    }

    /// Opens a file below the anchor with explicit flags.
    pub fn open_with(&self, rel: &Path, flags: OFlags) -> io::Result<File> {
        if !is_contained(rel) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} escapes the install directory", rel.display()),
            ));
        }
        let flags = flags | OFlags::CLOEXEC;
        let fd = if self.fully_resolved {
            rustix::fs::openat2(
                &self.dir,
                rel,
                flags,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
            )?
        } else {
            rustix::fs::openat(&self.dir, rel, flags | OFlags::NOFOLLOW, Mode::empty())?
        };
        Ok(File::from(fd))
    }

    /// Borrows the directory handle, for syscalls that take a `dirfd`.
    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.dir.as_fd()
    }
}

/// Whether a relative path stays inside the directory it is resolved against.
///
/// Rejects absolute paths, roots, prefixes and any `..`. A leading `./` is
/// fine, and so is a path that merely *contains* dots in a name.
pub fn is_contained(rel: &Path) -> bool {
    if rel.as_os_str().is_empty() {
        return false;
    }
    rel.components().all(|c| match c {
        Component::Normal(_) | Component::CurDir => true,
        Component::ParentDir | Component::RootDir | Component::Prefix(_) => false,
    })
}

#[cfg(test)]
mod tests {
    use crate::testutil::{Ctx, TestResult, check};

    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, PathBuf), String> {
        let tmp = tempfile::tempdir().ctx("temporary directory")?;
        let game = tmp.path().join("game");
        std::fs::create_dir_all(game.join("data")).ctx("create the game tree")?;
        std::fs::write(game.join("data/real.dat"), b"payload").ctx("write a game file")?;
        std::fs::write(tmp.path().join("outside.dat"), b"secret").ctx("write the outside file")?;
        Ok((tmp, game))
    }

    #[test]
    fn opens_a_regular_file_below_the_anchor() -> TestResult {
        let (_tmp, game) = fixture()?;
        let anchor = Anchor::open(&game).ctx("open the anchor")?;
        let mut file = anchor
            .open_file(Path::new("data/real.dat"))
            .ctx("open a real file")?;
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut file, &mut buf).ctx("read it back")?;
        check(
            buf == "payload",
            "the file's contents should come back intact",
        )
    }

    #[test]
    fn refuses_a_symlink_that_points_outside() -> TestResult {
        let (tmp, game) = fixture()?;
        // The classic swap: the walk saw a regular file, and by the time the
        // job opens it the name is a symlink to somewhere else entirely.
        std::os::unix::fs::symlink(
            tmp.path().join("outside.dat"),
            game.join("data/swapped.dat"),
        )
        .ctx("plant the symlink")?;
        let anchor = Anchor::open(&game).ctx("open the anchor")?;
        check(
            anchor.open_file(Path::new("data/swapped.dat")).is_err(),
            "a symlink leading outside must be refused",
        )
    }

    #[test]
    fn refuses_a_symlinked_parent_directory() -> TestResult {
        let (tmp, game) = fixture()?;
        std::os::unix::fs::symlink(tmp.path(), game.join("escape")).ctx("plant the symlink")?;
        let anchor = Anchor::open(&game).ctx("open the anchor")?;
        let result = anchor.open_file(Path::new("escape/outside.dat"));
        if anchor.fully_resolved() {
            check(
                result.is_err(),
                "openat2 must refuse a symlinked parent component",
            )
        } else {
            // Without openat2 only the last component is policed, so this is
            // documented as the weaker guarantee rather than asserted away.
            check(true, "no openat2 on this kernel")
        }
    }

    #[test]
    fn refuses_paths_that_escape_by_name() -> TestResult {
        let (_tmp, game) = fixture()?;
        let anchor = Anchor::open(&game).ctx("open the anchor")?;
        check(
            anchor.open_file(Path::new("../outside.dat")).is_err(),
            "`..` must be refused",
        )?;
        check(
            anchor.open_file(Path::new("/etc/passwd")).is_err(),
            "absolute paths refused",
        )?;
        check(
            anchor.open_file(Path::new("")).is_err(),
            "the empty path is not a file",
        )?;
        check(
            !is_contained(Path::new("a/../../b")),
            "`..` anywhere must be refused",
        )?;
        check(is_contained(Path::new("./a/b.dat")), "a leading ./ is fine")?;
        check(
            is_contained(Path::new("a/..weird/b")),
            "dots inside a name are fine",
        )
    }

    #[test]
    fn reports_which_guarantee_the_kernel_gives() -> TestResult {
        let (_tmp, game) = fixture()?;
        let anchor = Anchor::open(&game).ctx("open the anchor")?;
        // This is informational rather than a hard assertion: the crate
        // supports older kernels, it just protects less on them.
        if !anchor.fully_resolved() {
            eprintln!("note: openat2 unavailable; only the final component is checked");
        }
        check(
            anchor.path() == game,
            "the anchor should remember its directory",
        )
    }
}
