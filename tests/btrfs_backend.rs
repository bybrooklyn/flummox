//! The btrfs backend against a real btrfs directory.
//!
//! Every test needs the working directory to be on btrfs. Elsewhere they skip,
//! unless `FLUMMOX_REQUIRE_BTRFS` is set, which turns the skip into a failure
//! on the machine that is supposed to have btrfs.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use flummox::backend::btrfs::{BtrfsBackend, compress_fd, compressed_bytes, is_nocow};
use flummox::backend::{Backend, BusyCheck, CompressOpts, JobCtx, NullSink, Outcome};
use flummox::fsprobe;
use flummox::inventory::{WalkOpts, walk};
use flummox::testutil::{Ctx, TestResult, check, check_eq};

/// A temporary directory on btrfs, or `None` after printing why not.
fn btrfs_tempdir() -> Result<Option<tempfile::TempDir>, String> {
    let tmp = tempfile::TempDir::new_in(std::env::current_dir().ctx("working directory")?)
        .ctx("temporary directory")?;
    let fs = fsprobe::probe(tmp.path()).ctx("probe the temporary directory")?;
    if fs.fstype == "btrfs" {
        return Ok(Some(tmp));
    }
    check(
        std::env::var_os("FLUMMOX_REQUIRE_BTRFS").is_none(),
        format!(
            "the test directory is {}, not btrfs, and FLUMMOX_REQUIRE_BTRFS is set",
            fs.fstype
        ),
    )?;
    eprintln!("skipped: the test directory is not btrfs");
    Ok(None)
}

fn text(len: usize) -> Vec<u8> {
    "compress me ".repeat(len / 12 + 1).into_bytes()
}

fn compress_dir(dir: &Path, busy: Option<&dyn BusyCheck>) -> Result<Outcome, String> {
    let inv = walk(dir, &WalkOpts::native()).ctx("walk")?;
    let cancel = AtomicBool::new(false);
    let ctx = JobCtx {
        events: &NullSink,
        cancel: &cancel,
        busy,
    };
    let opts = CompressOpts {
        threads: 1,
        ..CompressOpts::default()
    };
    BtrfsBackend
        .compress(dir, &inv, &opts, &ctx)
        .ctx("compress the directory")
}

fn completed_names(outcome: &Outcome) -> Vec<String> {
    let mut names: Vec<String> = outcome
        .completed
        .iter()
        .map(|(entry, _)| entry.rel.display().to_string())
        .collect();
    names.sort();
    names
}

struct Counting(AtomicUsize);

impl BusyCheck for Counting {
    fn in_use_by(&self) -> Option<String> {
        self.0.fetch_add(1, Ordering::Relaxed);
        None
    }
}

#[test]
fn the_busy_check_is_rate_limited_across_files() -> TestResult {
    let Some(tmp) = btrfs_tempdir()? else {
        return Ok(());
    };
    for n in 0..6 {
        std::fs::write(tmp.path().join(format!("f{n}.dat")), text(32 * 1024)).ctx("fixture")?;
    }
    let counting = Counting(AtomicUsize::new(0));
    let outcome = compress_dir(tmp.path(), Some(&counting))?;
    check_eq(
        outcome.completed.len(),
        6,
        "control: every file was compressed",
    )?;
    check_eq(
        counting.0.load(Ordering::Relaxed),
        1,
        "six files inside one interval cost one /proc scan",
    )
}

#[test]
fn a_file_the_owner_cannot_write_is_skipped_not_failed() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let Some(tmp) = btrfs_tempdir()? else {
        return Ok(());
    };
    if nix::unistd::geteuid().is_root() {
        eprintln!("skipped: root can write any file");
        return Ok(());
    }
    let locked = tmp.path().join("locked.dat");
    std::fs::write(&locked, text(32 * 1024)).ctx("locked fixture")?;
    std::fs::write(tmp.path().join("open.dat"), text(32 * 1024)).ctx("open fixture")?;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444))
        .ctx("make read-only")?;
    let outcome = compress_dir(tmp.path(), None)?;
    check_eq(
        completed_names(&outcome),
        vec!["open.dat".to_owned()],
        "control: the writable file is compressed and the locked one is not",
    )?;
    check_eq(
        outcome.errors.clone(),
        Vec::<String>::new(),
        "a read-only file is not a failure",
    )?;
    check_eq(outcome.skipped, 1, "it is counted as skipped")
}

#[test]
fn a_no_cow_file_is_skipped_not_recorded() -> TestResult {
    let Some(tmp) = btrfs_tempdir()? else {
        return Ok(());
    };
    let nocow = tmp.path().join("nocow.dat");
    std::fs::write(&nocow, b"").ctx("empty fixture")?;
    // The flag only takes on an empty file.
    let status = std::process::Command::new("chattr")
        .arg("+C")
        .arg(&nocow)
        .status()
        .ctx("run chattr")?;
    check(status.success(), "chattr +C succeeds")?;
    std::fs::write(&nocow, text(32 * 1024)).ctx("fill the no-cow file")?;
    std::fs::write(tmp.path().join("plain.dat"), text(32 * 1024)).ctx("plain fixture")?;
    check(
        is_nocow(&std::fs::File::open(&nocow).ctx("open no-cow")?).ctx("read no-cow flag")?,
        "is_nocow reports the attribute",
    )?;
    check(
        !is_nocow(&std::fs::File::open(tmp.path().join("plain.dat")).ctx("open plain")?)
            .ctx("read plain flag")?,
        "control: is_nocow is false for an ordinary file",
    )?;
    let outcome = compress_dir(tmp.path(), None)?;
    check_eq(
        completed_names(&outcome),
        vec!["plain.dat".to_owned()],
        "control: the plain file is recorded and the no-cow one is not",
    )
}

#[test]
fn the_level_reported_is_the_level_applied() -> TestResult {
    let Some(tmp) = btrfs_tempdir()? else {
        return Ok(());
    };
    let path = tmp.path().join("level.dat");
    std::fs::write(&path, text(64 * 1024)).ctx("fixture")?;
    let file = std::fs::File::open(&path).ctx("open")?;
    let release = nix::sys::utsname::uname().ctx("uname")?;
    let release = release.release().to_string_lossy().into_owned();
    let mut parts = release.split(|c: char| !c.is_ascii_digit());
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    if (major, minor) < (6, 15) {
        eprintln!("skipped: kernel {release} ignores the level");
        return Ok(());
    }
    check_eq(
        compress_fd(&file, 19).ctx("level 19")?,
        15,
        "a level past the kernel's range reports the clamped level",
    )?;
    check_eq(
        compress_fd(&file, 7).ctx("level 7")?,
        7,
        "control: an in-range level is reported as asked",
    )?;
    check_eq(
        compress_fd(&file, 0).ctx("level 0")?,
        3,
        "level 0 means the default, which is 3",
    )
}

#[test]
fn the_property_covers_every_folder_and_decompress_clears_every_copy() -> TestResult {
    let Some(tmp) = btrfs_tempdir()? else {
        return Ok(());
    };
    let root = tmp.path();
    std::fs::create_dir_all(root.join("data/deep")).ctx("folders")?;
    std::fs::write(root.join("data/deep/a.dat"), text(64 * 1024)).ctx("fixture")?;
    let property = |path: &Path| -> Result<Option<Vec<u8>>, String> {
        xattr::get(path, "btrfs.compression")
            .ctx(format!("read the property of {}", path.display()))
    };
    check_eq(
        property(&root.join("data/deep"))?,
        None,
        "control: a folder that existed before the pass has no property yet",
    )?;
    compress_dir(root, None)?;
    check_eq(
        property(&root.join("data/deep"))?,
        Some(b"zstd".to_vec()),
        "a nested folder carries the property after a pass",
    )?;
    // A file created now inherits its own copy of the property.
    std::fs::write(root.join("data/deep/b.dat"), text(64 * 1024)).ctx("later file")?;
    check_eq(
        property(&root.join("data/deep/b.dat"))?,
        Some(b"zstd".to_vec()),
        "control: a file created later inherits the property",
    )?;

    let inv = walk(root, &WalkOpts::native()).ctx("walk")?;
    let cancel = AtomicBool::new(false);
    let ctx = JobCtx {
        events: &NullSink,
        cancel: &cancel,
        busy: None,
    };
    let outcome = BtrfsBackend
        .decompress(root, &inv, &ctx)
        .ctx("decompress")?;
    check_eq(outcome.errors.clone(), Vec::<String>::new(), "no failures")?;
    for rel in ["data", "data/deep", "data/deep/a.dat", "data/deep/b.dat"] {
        check_eq(
            property(&root.join(rel))?,
            None,
            format!("{rel} carries no property after a decompress"),
        )?;
    }
    for rel in ["data/deep/a.dat", "data/deep/b.dat"] {
        let (compressed, _) = compressed_bytes(&root.join(rel)).ctx("map extents")?;
        check_eq(compressed, 0, format!("{rel} is stored uncompressed"))?;
    }
    Ok(())
}

#[test]
fn a_symlinked_library_folder_gets_the_property_on_its_target() -> TestResult {
    use flummox::backend::btrfs::{dir_property, set_dir_property};
    let Some(tmp) = btrfs_tempdir()? else {
        return Ok(());
    };
    let real = tmp.path().join("real");
    std::fs::create_dir(&real).ctx("real folder")?;
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).ctx("link")?;
    set_dir_property(&link, true).ctx("set through the link")?;
    check_eq(
        dir_property(&real).ctx("read the real folder")?,
        Some("zstd".to_owned()),
        "the property is on the folder the link leads to",
    )?;
    check_eq(
        dir_property(&link).ctx("read through the link")?,
        Some("zstd".to_owned()),
        "reading through the link agrees",
    )?;
    set_dir_property(&link, false).ctx("clear through the link")?;
    check_eq(
        dir_property(&real).ctx("re-read the real folder")?,
        None,
        "clearing through the link clears the target",
    )
}
