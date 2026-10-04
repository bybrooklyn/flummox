//! Native APFS compression with verified staging and durable replacement journals.
#![allow(unsafe_code)]

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone)]
pub struct InstalledGame {
    pub title: String,
    pub path: PathBuf,
    pub app_id: Option<u32>,
    pub build: Option<String>,
}
#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub files: u64,
    pub changed: u64,
    pub skipped: u64,
    pub bytes: u64,
    pub allocation_before: u64,
    pub allocation_after: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    device: u64,
    inode: u64,
    length: u64,
    modified: i64,
    nanos: i64,
}
fn identity(path: &Path) -> Result<Identity> {
    let stat = std::fs::symlink_metadata(path)?;
    ensure!(
        stat.is_file() && stat.nlink() == 1,
        "File is no longer an independent regular file"
    );
    Ok(Identity {
        device: stat.dev(),
        inode: stat.ino(),
        length: stat.len(),
        modified: stat.mtime(),
        nanos: stat.mtime_nsec(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    #[serde(with = "crate::path_serde")]
    pub root: PathBuf,
    #[serde(with = "crate::path_serde")]
    pub source: PathBuf,
    #[serde(with = "crate::path_serde")]
    pub staged: PathBuf,
    pub volume: crate::storage::Volume,
    original: Identity,
    candidate: Identity,
    hash: String,
}

fn journal_path(record: &Recovery) -> Result<PathBuf> {
    Ok(crate::libraries::data_dir()?.join("recovery").join(format!(
        "{}.json",
        blake3::hash(record.source.as_os_str().as_bytes()).to_hex()
    )))
}
fn save(record: &Recovery) -> Result<PathBuf> {
    let path = journal_path(record)?;
    let parent = path.parent().context("Journal has no parent")?;
    crate::libraries::private_dir(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, record)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path)?;
    File::open(parent)?.sync_all()?;
    Ok(path)
}
pub fn recovery() -> Result<Vec<Recovery>> {
    let directory = crate::libraries::data_dir()?.join("recovery");
    if !directory.exists() {
        return Ok(vec![]);
    }
    let mut records = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let file = File::open(entry.path())?;
            ensure!(
                file.metadata()?.len() < 1024 * 1024,
                "Recovery record is too large"
            );
            records.push(serde_json::from_reader(file)?);
        }
    }
    Ok(records)
}

fn directory(path: &Path) -> Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut current = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")?;
    for part in path.components() {
        let std::path::Component::Normal(name) = part else {
            ensure!(
                matches!(part, std::path::Component::RootDir),
                "Recovery path must be absolute without parent steps"
            );
            continue;
        };
        let name = CString::new(name.as_bytes())?;
        // SAFETY: current owns the parent descriptor and name is terminated.
        let descriptor = unsafe {
            libc::openat(
                current.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        ensure!(
            descriptor >= 0,
            "Directory changed or is unavailable: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: openat returned a new owned descriptor and no other File owns it.
        current = unsafe { File::from_raw_fd(descriptor) };
    }
    Ok(current)
}

fn swap(first: &Path, second: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let first_parent = directory(first.parent().context("File has no parent")?)?;
    let second_parent = directory(second.parent().context("Staging file has no parent")?)?;
    let first = CString::new(first.file_name().context("File has no name")?.as_bytes())?;
    let second = CString::new(
        second
            .file_name()
            .context("Staging file has no name")?
            .as_bytes(),
    )?;
    // SAFETY: owned directory descriptors anchor both terminated leaf names.
    let result = unsafe {
        libc::renameatx_np(
            first_parent.as_raw_fd(),
            first.as_ptr(),
            second_parent.as_raw_fd(),
            second.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    ensure!(
        result == 0,
        "Atomic replacement failed: {}",
        std::io::Error::last_os_error()
    );
    first_parent.sync_all()?;
    second_parent.sync_all()?;
    Ok(())
}

pub fn recover_folder(folder: &Path) -> Result<()> {
    let state = crate::libraries::data_dir()?;
    crate::libraries::private_dir(&state)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join("native.lock"))?;
    ensure!(
        lock.try_lock().is_ok(),
        "Another Flummox process is working"
    );
    let folder = validate(folder)?;
    for record in recovery()?.iter().filter(|record| record.root == folder) {
        recover_original(record)?;
    }
    Ok(())
}

fn validate(root: &Path) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    ensure!(
        root.is_dir() && root.parent().is_some(),
        "Choose a game folder, not a drive"
    );
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    ensure!(
        root != home && root != Path::new("/Applications") && root != Path::new("/Users"),
        "Choose an installed game folder"
    );
    for protected in [
        PathBuf::from("/System"),
        PathBuf::from("/Library"),
        PathBuf::from("/usr"),
        PathBuf::from("/bin"),
        PathBuf::from("/sbin"),
        PathBuf::from("/private/etc"),
        PathBuf::from("/private/var/db"),
        PathBuf::from("/private/var/root"),
        home.join(".ssh"),
        home.join("Library/Application Support/flummox"),
    ] {
        ensure!(!root.starts_with(protected), "This location is protected");
    }
    let volume = crate::storage::volume(&root)?;
    ensure!(
        volume.identity.starts_with("apfs:"),
        "Native Mac compression requires APFS"
    );
    Ok(root)
}

fn idle(root: &Path) -> Result<()> {
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args(["-n", "-P", "+D"])
        .arg(root)
        .output()?;
    ensure!(
        output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty(),
        "Close the game and launcher activity before changing storage"
    );
    Ok(())
}
fn hash(path: &Path) -> Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hasher.update(buffer.get(..length).context("Invalid read length")?);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn attributes(path: &Path) -> Result<std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>> {
    let mut attributes = std::collections::BTreeMap::new();
    for name in xattr::list(path)? {
        if name == "com.apple.decmpfs" || name == "com.apple.ResourceFork" {
            continue;
        }
        attributes.insert(
            name.clone(),
            xattr::get(path, &name)?.context("File metadata changed")?,
        );
    }
    Ok(attributes)
}

fn metadata_equal(first: &Path, second: &Path) -> Result<()> {
    let a = std::fs::symlink_metadata(first)?;
    let b = std::fs::symlink_metadata(second)?;
    ensure!(
        a.mode() == b.mode()
            && a.uid() == b.uid()
            && a.gid() == b.gid()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec(),
        "File permissions or timestamps changed during staging"
    );
    ensure!(
        attributes(first)? == attributes(second)?,
        "File attributes changed during staging"
    );
    let acl = |path: &Path| -> Result<Vec<u8>> {
        let output = std::process::Command::new("/bin/ls")
            .arg("-lde")
            .arg(path)
            .output()?;
        ensure!(output.status.success(), "Cannot verify file ACL");
        Ok(output
            .stdout
            .split(|byte| *byte == b'\n')
            .skip(1)
            .flatten()
            .copied()
            .collect())
    };
    ensure!(
        acl(first)? == acl(second)?,
        "File ACL changed during staging"
    );
    Ok(())
}

fn bundle(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|parent| {
            parent
                .extension()
                .is_some_and(|extension| extension == "app")
        })
        .map(Path::to_path_buf)
}
fn signed_bundle(path: &Path) -> Result<Option<PathBuf>> {
    let Some(bundle) = bundle(path) else {
        return Ok(None);
    };
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["--display"])
        .arg(&bundle)
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    verify_bundle(&bundle)?;
    Ok(Some(bundle))
}
fn verify_bundle(path: &Path) -> Result<()> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(path)
        .output()?;
    ensure!(
        output.status.success(),
        "Application signature verification failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn clear_record(record: &Recovery) -> Result<()> {
    let parent = record
        .staged
        .parent()
        .context("Staging path is incomplete")?;
    ensure!(
        parent
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".flummox-work-")),
        "Unexpected staging directory"
    );
    std::fs::remove_dir_all(parent)?;
    std::fs::remove_file(journal_path(record)?)?;
    Ok(())
}

/// Restores a journaled original only when both identities still match.
pub fn recover_original(record: &Recovery) -> Result<()> {
    let root = validate(&record.root)?;
    ensure!(
        record.source.starts_with(&root) && record.staged.starts_with(&root),
        "Recovery paths escaped the game folder"
    );
    ensure!(
        crate::storage::volume(&root)?.identity == record.volume.identity,
        "Reconnect the original drive"
    );
    idle(&root)?;
    if identity(&record.source)? == record.original && identity(&record.staged)? == record.candidate
    {
        return clear_record(record);
    }
    ensure!(
        identity(&record.source)? == record.candidate
            && identity(&record.staged)? == record.original,
        "Files changed after interruption; retain both copies for review"
    );
    swap(&record.source, &record.staged)?;
    File::open(record.source.parent().context("File has no parent")?)?.sync_all()?;
    ensure!(
        hash(&record.source)? == record.hash,
        "Original verification failed; recovery copies retained"
    );
    clear_record(record)
}

fn stage(root: &Path, source: &Path, restore: bool) -> Result<bool> {
    let original = identity(source)?;
    let stat = std::fs::symlink_metadata(source)?;
    let compressed = stat.st_flags() & libc::UF_COMPRESSED != 0;
    if restore != compressed || stat.len() <= 4096 || stat.nlink() != 1 {
        return Ok(false);
    }
    if !restore && xattr::get(source, "com.apple.ResourceFork")?.is_some() {
        return Ok(false);
    }
    let mut space = crate::storage::SpacePlan::default();
    space.add(
        crate::storage::volume(root)?,
        stat.len().saturating_mul(2),
        "Temporary file and replacement",
    )?;
    space.recheck()?;
    let signature = signed_bundle(source)?;
    let parent = source.parent().context("File has no parent")?;
    let temporary = tempfile::Builder::new()
        .prefix(".flummox-work-")
        .tempdir_in(parent)?;
    let staged = temporary.path().join("candidate");
    let before_hash = hash(source)?;
    if restore {
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staged)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        let source_c = CString::new(source.as_os_str().as_bytes())?;
        let stage_c = CString::new(staged.as_os_str().as_bytes())?;
        // SAFETY: paths are terminated and null requests copyfile's internal state.
        let result = unsafe {
            libc::copyfile(
                source_c.as_ptr(),
                stage_c.as_ptr(),
                std::ptr::null_mut(),
                libc::COPYFILE_METADATA,
            )
        };
        ensure!(result == 0, "Cannot preserve file metadata");
        for attribute in ["com.apple.decmpfs", "com.apple.ResourceFork"] {
            if xattr::get(&staged, attribute)?.is_some() {
                xattr::remove(&staged, attribute)?;
            }
        }
        // SAFETY: stage_c names our private staging file; compression metadata was removed.
        let result =
            unsafe { libc::chflags(stage_c.as_ptr(), stat.st_flags() & !libc::UF_COMPRESSED) };
        ensure!(result == 0, "Cannot restore ordinary file flags");
    } else {
        let result = std::process::Command::new("/usr/bin/ditto")
            .args(["--hfsCompression", "--rsrc", "--extattr", "--acl"])
            .arg(source)
            .arg(&staged)
            .output()?;
        ensure!(
            result.status.success(),
            "Mac compression failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let candidate = std::fs::symlink_metadata(&staged)?;
        if candidate.st_flags() & libc::UF_COMPRESSED == 0 || candidate.blocks() >= stat.blocks() {
            return Ok(false);
        }
    }
    ensure!(
        hash(&staged)? == before_hash,
        "Staged file bytes failed verification"
    );
    metadata_equal(source, &staged)?;
    ensure!(
        identity(source)? == original && source.canonicalize()?.starts_with(root),
        "Game changed while staging; original retained"
    );
    let mut publish = crate::storage::SpacePlan::default();
    publish.add(
        crate::storage::volume(root)?,
        0,
        "Publication safety headroom",
    )?;
    publish.check()?;
    let record = Recovery {
        root: root.to_path_buf(),
        source: source.to_path_buf(),
        staged: staged.clone(),
        volume: crate::storage::volume(root)?,
        original,
        candidate: identity(&staged)?,
        hash: before_hash,
    };
    let _retained = temporary.keep();
    save(&record)?;
    swap(source, &staged)?;
    File::open(parent)?.sync_all()?;
    ensure!(
        identity(&staged)? == record.original && identity(source)? == record.candidate,
        "Replacement identities changed; recovery copies retained"
    );
    ensure!(
        hash(source)? == record.hash,
        "Published bytes failed verification; recovery copies retained"
    );
    metadata_equal(source, &staged)?;
    if let Some(bundle) = signature {
        verify_bundle(&bundle)?;
    }
    clear_record(&record)?;
    Ok(true)
}

fn visit(
    root: &Path,
    restore: bool,
    cancel: &AtomicBool,
    mut report: impl FnMut(Progress),
) -> Result<String> {
    let root = validate(root)?;
    let state = crate::libraries::data_dir()?;
    crate::libraries::private_dir(&state)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join("native.lock"))?;
    ensure!(
        lock.try_lock().is_ok(),
        "Another Flummox process is working"
    );
    ensure!(
        !recovery()?.iter().any(|record| record.root == root),
        "Review Recovery before processing this game"
    );
    idle(&root)?;
    crate::storage::native_plan(&root, restore)?.recheck()?;
    let mut summary = Progress::default();
    for entry in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".flummox-work-")
        })
    {
        ensure!(!cancel.load(Ordering::Relaxed), "Operation stopped");
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let stat = entry.metadata()?;
        summary.files += 1;
        summary.bytes = summary.bytes.saturating_add(stat.len());
        summary.allocation_before = summary
            .allocation_before
            .saturating_add(stat.blocks().saturating_mul(512));
        if stat.nlink() == 1 && stage(&root, entry.path(), restore)? {
            summary.changed += 1;
        } else {
            summary.skipped += 1;
        }
        summary.allocation_after = summary.allocation_after.saturating_add(
            entry
                .path()
                .symlink_metadata()?
                .blocks()
                .saturating_mul(512),
        );
        report(summary.clone());
    }
    Ok(format!(
        "{} files processed; {} changed, {} skipped. Allocated storage: {} before, {} after.",
        summary.files,
        summary.changed,
        summary.skipped,
        summary.allocation_before,
        summary.allocation_after
    ))
}

pub fn optimize_folder_with(
    root: &Path,
    cancel: &AtomicBool,
    report: impl FnMut(Progress),
) -> Result<String> {
    visit(root, false, cancel, report)
}
pub fn restore_folder_with(
    root: &Path,
    cancel: &AtomicBool,
    report: impl FnMut(Progress),
) -> Result<String> {
    visit(root, true, cancel, report)
}

pub fn discover_steam() -> Vec<InstalledGame> {
    discover().unwrap_or_default()
}
fn discover() -> Result<Vec<InstalledGame>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let steam = home.join("Library/Application Support/Steam/steamapps");
    let mut roots = vec![steam.clone()];
    if let Ok(text) = std::fs::read_to_string(steam.join("libraryfolders.vdf"))
        && let Ok(value) = crate::native_vdf::parse(&text)
    {
        roots.extend(
            value
                .entries()
                .iter()
                .filter_map(|(_, value)| value.as_obj())
                .filter_map(|value| value.get_str("path"))
                .map(|path| PathBuf::from(path).join("steamapps")),
        );
    }
    let mut games = vec![];
    for library in roots {
        let Ok(entries) = std::fs::read_dir(&library) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with("appmanifest_")
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(value) = crate::native_vdf::parse(&text) else {
                continue;
            };
            let (Some(title), Some(folder)) = (value.get_str("name"), value.get_str("installdir"))
            else {
                continue;
            };
            let common = library.join("common");
            let Ok(path) = common.join(folder).canonicalize() else {
                continue;
            };
            if common
                .canonicalize()
                .is_ok_and(|common| path.starts_with(common))
                && path.is_dir()
            {
                games.push(InstalledGame {
                    title: title.into(),
                    path,
                    app_id: value.get_u32("appid"),
                    build: value.get_str("buildid").map(str::to_owned),
                });
            }
        }
    }
    games.sort_by_key(|game| game.title.to_lowercase());
    games.dedup_by(|first, second| first.path == second.path);
    Ok(games)
}

#[derive(Parser)]
#[command(
    name = "flummox",
    version,
    about = "Compress installed games using native APFS storage"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Scan,
    Analyze { folder: PathBuf },
    Compress { folder: PathBuf },
    Decompress { folder: PathBuf },
    Recovery,
    Recover { folder: PathBuf },
}
pub fn run() -> Result<()> {
    match Args::parse().command {
        Command::Scan => {
            for game in discover()? {
                println!("{}  {}", game.title, game.path.display());
            }
        }
        Command::Analyze { folder } => println!(
            "{}",
            serde_json::to_string_pretty(&crate::storage::native_plan(
                &validate(&folder)?,
                false
            )?)?
        ),
        Command::Compress { folder } => println!(
            "{}",
            optimize_folder_with(&folder, &AtomicBool::new(false), |_| {})?
        ),
        Command::Decompress { folder } => println!(
            "{}",
            restore_folder_with(&folder, &AtomicBool::new(false), |_| {})?
        ),
        Command::Recovery => println!("{}", serde_json::to_string_pretty(&recovery()?)?),
        Command::Recover { folder } => {
            let folder = validate(&folder)?;
            for record in recovery()?.iter().filter(|record| record.root == folder) {
                recover_original(record)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn native_apfs_staging_round_trips_bytes_and_metadata() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let root = validate(fixture.path()).ctx("APFS capability")?;
        let path = root.join("compressible");
        let bytes = vec![b'a'; 4 * 1024 * 1024];
        std::fs::write(&path, &bytes).ctx("source")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ctx("mode")?;
        xattr::set(&path, "user.flummox-fixture", b"metadata").ctx("attribute")?;
        let before = std::fs::symlink_metadata(&path).ctx("original metadata")?;
        check(
            stage(&root, &path, false).ctx("compress")?,
            "APFS must actually compress the positive control",
        )?;
        let compressed = std::fs::symlink_metadata(&path).ctx("compressed metadata")?;
        check(
            compressed.blocks() < before.blocks(),
            "allocated bytes must decrease",
        )?;
        check_eq(
            std::fs::read(&path).ctx("transparent read")?,
            bytes.clone(),
            "ordinary reads preserve bytes",
        )?;
        check(
            stage(&root, &path, true).ctx("restore")?,
            "compressed file must restore",
        )?;
        check_eq(
            std::fs::read(&path).ctx("restored bytes")?,
            bytes,
            "restored bytes",
        )?;
        check_eq(
            std::fs::symlink_metadata(&path).ctx("flags")?.st_flags() & libc::UF_COMPRESSED,
            0,
            "restore removes compression",
        )?;
        check_eq(
            xattr::get(&path, "user.flummox-fixture").ctx("restored attribute")?,
            Some(b"metadata".to_vec()),
            "attributes survive",
        )?;
        check_eq(
            std::fs::symlink_metadata(&path)
                .ctx("restored mode")?
                .mode(),
            before.mode(),
            "executable permissions survive",
        )
    }
    #[test]
    fn incomplete_exchange_retains_and_restores_original() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let root = validate(fixture.path()).ctx("root")?;
        let source = root.join("original");
        std::fs::write(&source, b"original bytes").ctx("source")?;
        let temporary = tempfile::Builder::new()
            .prefix(".flummox-work-")
            .tempdir_in(&root)
            .ctx("staging")?;
        let staged = temporary.path().join("candidate");
        std::fs::write(&staged, b"candidate bytes").ctx("candidate")?;
        let record = Recovery {
            root: root.clone(),
            source: source.clone(),
            staged: staged.clone(),
            volume: crate::storage::volume(&root).ctx("volume")?,
            original: identity(&source).ctx("original identity")?,
            candidate: identity(&staged).ctx("candidate identity")?,
            hash: hash(&source).ctx("hash")?,
        };
        let _retained = temporary.keep();
        save(&record).ctx("journal")?;
        swap(&source, &staged).ctx("exchange")?;
        recover_original(&record).ctx("recover interrupted exchange")?;
        check_eq(
            std::fs::read(source).ctx("recovered source")?,
            b"original bytes".to_vec(),
            "original restored",
        )?;
        check(
            !journal_path(&record).ctx("journal")?.exists(),
            "successful recovery clears its journal",
        )
    }
}
