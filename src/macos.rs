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

// Every staging directory starts with this prefix.
const WORK_PREFIX: &str = ".flummox-work-";
// How often a pass looks again for programs that opened files in the game folder.
const IDLE_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

// A Steam game as the `scan` command lists it.
#[derive(Debug, Clone)]
pub struct InstalledGame {
    pub title: String,
    pub path: PathBuf,
    pub app_id: Option<u32>,
    pub build: Option<String>,
}
// Running totals for one pass. `bytes` is logical size. The two allocation fields
// sum the visited files' allocated size before and after each was processed.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub files: u64,
    pub changed: u64,
    pub skipped: u64,
    pub bytes: u64,
    pub allocation_before: u64,
    pub allocation_after: u64,
}

// What must stay equal for a path to still hold the file that was examined:
// device, inode, length and modification time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    device: u64,
    inode: u64,
    length: u64,
    modified: i64,
    nanos: i64,
}
// Fails for anything but a regular file with exactly one hard link. Does not
// follow a symlink at `path`.
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

// Journal of one file replacement. It is written before the two files are swapped
// and deleted once the result is verified, so one found on disk marks a swap that
// may or may not have happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    // The game folder the job was started on.
    #[serde(with = "crate::path_serde")]
    pub root: PathBuf,
    // The game file being replaced.
    #[serde(with = "crate::path_serde")]
    pub source: PathBuf,
    // The new copy, at `<work parent>/.flummox-work-*/candidate`. The work parent is
    // the source's directory, or the folder holding its outermost `.app`. Journals
    // from earlier versions always used the source's directory.
    #[serde(with = "crate::path_serde")]
    pub staged: PathBuf,
    pub volume: crate::storage::Volume,
    // Identities of the two files before the swap, and the BLAKE3 of the original's
    // bytes. After a swap the identities are found at each other's path.
    original: Identity,
    candidate: Identity,
    hash: String,
}

// One journal per source path, named by the hash of that path.
fn journal_path(record: &Recovery) -> Result<PathBuf> {
    Ok(crate::libraries::data_dir()?.join("recovery").join(format!(
        "{}.json",
        blake3::hash(record.source.as_os_str().as_bytes()).to_hex()
    )))
}
// Writes the journal durably: temp file, fsync, rename, directory fsync.
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
// Lists every journal on disk. A journal over 1 MiB or with unknown fields is an
// error.
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

// Opens a directory by walking down from `/` one component at a time with
// O_NOFOLLOW, so no symlink is followed anywhere along the path. The path must be
// absolute and free of `..`.
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

// Exchanges two files in one atomic step with renameatx_np(RENAME_SWAP), then
// fsyncs both parent directories. Both names stay present throughout.
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

// Resolves every journal recorded for this game folder. Takes native.lock, so it
// fails while another Flummox job is running.
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

// Canonicalises a game folder and refuses one that is too broad (a filesystem
// root, a volume's mount point, the home directory, /Applications, /Users, any
// folder holding Flummox's state), one inside a system or Flummox-owned
// directory, and one that is not on APFS.
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
    let state = crate::libraries::data_dir()?;
    let state = state.canonicalize().unwrap_or_else(|_| state.clone());
    if let Some(reason) = too_broad(&root, &volume.path, &state) {
        anyhow::bail!(reason);
    }
    ensure!(
        volume.identity.starts_with("apfs:"),
        "Native Mac compression requires APFS"
    );
    Ok(root)
}

// Why `root` cannot be a game folder: it is its volume's mount point, or it
// contains Flummox's state directory.
fn too_broad(root: &Path, mount: &Path, state: &Path) -> Option<&'static str> {
    if root == mount {
        Some("Choose a game folder, not a drive")
    } else if state.starts_with(root) {
        Some("This folder contains Flummox's own data")
    } else {
        None
    }
}

// Fails if any process has a file open under `root`. The check passes only when
// `lsof +D` exits with status 1 and prints nothing, which is its report of no open
// files. Every other outcome counts as busy.
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
// BLAKE3 of the file's logical bytes, as hex. Refuses a symlink at `path`.
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

// Attributes the kernel or the compressor sets and that may differ between two
// copies of one file: compression state, the resource fork and the provenance
// tag that macOS 13 and later attaches to files written by tracked apps.
fn kernel_managed(name: &std::ffi::OsStr) -> bool {
    [
        "com.apple.decmpfs",
        "com.apple.ResourceFork",
        "com.apple.provenance",
    ]
    .iter()
    .any(|managed| name == *managed)
}

// The file's extended attributes, leaving out the kernel-managed ones.
fn attributes(path: &Path) -> Result<std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>> {
    let mut attributes = std::collections::BTreeMap::new();
    for name in xattr::list(path)? {
        if kernel_managed(&name) {
            continue;
        }
        attributes.insert(
            name.clone(),
            xattr::get(path, &name)?.context("File metadata changed")?,
        );
    }
    Ok(attributes)
}

// Fails unless both files have the same mode, owner, group, mtime, extended
// attributes and ACL.
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
    // `ls -lde` prints the file's own line first and its ACL entries on the lines
    // after it. The first line names the file, so only the rest is compared.
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

fn is_app(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "app")
}
// The nearest of the path and its ancestors that has an `.app` extension.
fn bundle(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|parent| is_app(parent))
        .map(Path::to_path_buf)
}
// The outermost of the path and its ancestors that has an `.app` extension.
fn outer_bundle(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .filter(|parent| is_app(parent))
        .last()
        .map(Path::to_path_buf)
}
// The directory that holds the work directory for `source`. For a file inside an
// application bundle it is the folder containing the outermost bundle, because a
// work directory inside a signed bundle breaks its seal. Otherwise it is the
// file's own folder. Both are on the file's volume, so the swap stays atomic.
fn work_parent(source: &Path) -> Option<PathBuf> {
    outer_bundle(source)
        .as_deref()
        .unwrap_or(source)
        .parent()
        .map(Path::to_path_buf)
}
// Whether a journal's staged path is somewhere `stage` could have put it for
// `source`: the current layout, or the older one beside the file inside `root`.
fn staging_expected(root: &Path, source: &Path, staged: &Path) -> bool {
    let Some(parent) = staged.parent().and_then(Path::parent) else {
        return false;
    };
    work_parent(source).is_some_and(|expected| expected == parent)
        || (source.parent() == Some(parent) && staged.starts_with(root))
}
// True when `codesign --display` succeeds, which is how this module reads "signed".
fn is_signed(bundle: &Path) -> Result<bool> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["--display"])
        .arg(bundle)
        .output()?;
    Ok(output.status.success())
}
// Signature state of each application bundle met in one pass.
#[derive(Default)]
struct Seals {
    bundles: std::collections::BTreeMap<PathBuf, Seal>,
}
struct Seal {
    // The bundle was signed and verified before its first file was replaced.
    valid: bool,
    // At least one file in it has been replaced.
    changed: bool,
}
impl Seals {
    // Called before a file is replaced. The first call for a bundle verifies it. A
    // bundle that is unsigned, or whose signature is already invalid, is not
    // verified again afterwards.
    fn before(&mut self, path: &Path) -> Result<()> {
        let Some(bundle) = bundle(path) else {
            return Ok(());
        };
        if self.bundles.contains_key(&bundle) {
            return Ok(());
        }
        let valid = is_signed(&bundle)? && verify_bundle(&bundle).is_ok();
        self.bundles.insert(
            bundle,
            Seal {
                valid,
                changed: false,
            },
        );
        Ok(())
    }
    // Notes that a file under `path`'s bundle was replaced.
    fn changed(&mut self, path: &Path) {
        if let Some(bundle) = bundle(path)
            && let Some(seal) = self.bundles.get_mut(&bundle)
        {
            seal.changed = true;
        }
    }
    // Verifies each bundle that was valid before the pass and had a file replaced.
    fn verify_after(&self) -> Result<()> {
        for (bundle, seal) in &self.bundles {
            if seal.valid && seal.changed {
                verify_bundle(bundle).with_context(|| {
                    format!(
                        "{} no longer verifies; decompress the game to undo the pass",
                        bundle.display()
                    )
                })?;
            }
        }
        Ok(())
    }
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

// Treats a missing path as already removed.
fn ignore_missing(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}
// True when nothing exists at `path`. Any other failure to look is an error.
fn is_missing(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}
// Deletes the journal and syncs its directory.
fn remove_journal(record: &Recovery) -> Result<()> {
    let path = journal_path(record)?;
    std::fs::remove_file(&path)?;
    File::open(path.parent().context("Journal has no parent")?)?.sync_all()?;
    Ok(())
}

// Deletes the staging directory with whichever copy it holds, then the journal.
// Refuses a directory whose name lacks the `.flummox-work-` prefix.
fn clear_record(record: &Recovery) -> Result<()> {
    let parent = record
        .staged
        .parent()
        .context("Staging path is incomplete")?;
    ensure!(
        parent
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(WORK_PREFIX)),
        "Unexpected staging directory"
    );
    ignore_missing(std::fs::remove_dir_all(parent))?;
    if let Some(outer) = parent.parent() {
        File::open(outer)?.sync_all()?;
    }
    remove_journal(record)
}

/// Restores a journaled original only when both identities still match.
pub fn recover_original(record: &Recovery) -> Result<()> {
    let root = validate(&record.root)?;
    ensure!(
        record.source.starts_with(&root) && staging_expected(&root, &record.source, &record.staged),
        "Recovery paths escaped the game folder"
    );
    ensure!(
        crate::storage::volume(&root)?.identity == record.volume.identity,
        "Reconnect the original drive"
    );
    idle(&root)?;
    // The work directory is gone and the journal remains: a crash between the two
    // deletions in `clear_record`, or a journal whose work directory was never
    // kept. The source must hold the verified new copy or the untouched original.
    if is_missing(&record.staged)? {
        let current = identity(&record.source)?;
        ensure!(
            (current == record.candidate && hash(&record.source)? == record.hash)
                || current == record.original,
            "Files changed after interruption; retain both copies for review"
        );
        return remove_journal(record);
    }
    // The journal was written but the swap never ran. The original is in place, so
    // only the candidate and the journal need removing.
    if identity(&record.source)? == record.original && identity(&record.staged)? == record.candidate
    {
        return clear_record(record);
    }
    // Otherwise the files must be exactly swapped. Check the original's bytes
    // where it sits, so a bad original is never put back over the new copy.
    ensure!(
        identity(&record.source)? == record.candidate
            && identity(&record.staged)? == record.original,
        "Files changed after interruption; retain both copies for review"
    );
    ensure!(
        hash(&record.staged)? == record.hash,
        "Original verification failed; recovery copies retained"
    );
    swap(&record.source, &record.staged)?;
    File::open(record.source.parent().context("File has no parent")?)?.sync_all()?;
    clear_record(record)
}

// Replaces one file with a compressed copy, or with an ordinary copy when `restore`
// is set. Returns false when the file is left alone. The copy is built beside the
// file, verified, journaled and swapped in. The original is deleted only after the
// swapped result has been verified too.
fn stage(root: &Path, source: &Path, restore: bool, seals: &mut Seals) -> Result<bool> {
    let original = identity(source)?;
    let stat = std::fs::symlink_metadata(source)?;
    let compressed = stat.st_flags() & libc::UF_COMPRESSED != 0;
    // Skip a file already in the wanted state, one of 4096 bytes or fewer, and one
    // that already has a resource fork when compressing.
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
    seals.before(source)?;
    // The work directory sits beside the file, or beside the enclosing `.app`, so
    // the swap stays on one volume and the bundle's seal is not disturbed.
    let parent = source.parent().context("File has no parent")?;
    let work = work_parent(source).context("File has no parent")?;
    let temporary = tempfile::Builder::new()
        .prefix(WORK_PREFIX)
        .tempdir_in(&work)?;
    let staged = temporary.path().join("candidate");
    let before_hash = hash(source)?;
    // Restore: reading a compressed file yields its plain bytes, so a byte copy is
    // already decompressed. copyfile then brings over the metadata, after which the
    // compression attributes and the UF_COMPRESSED flag are removed from the copy.
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
        // SAFETY: stage_c is a terminated path string that outlives the call.
        let result =
            unsafe { libc::chflags(stage_c.as_ptr(), stat.st_flags() & !libc::UF_COMPRESSED) };
        ensure!(result == 0, "Cannot restore ordinary file flags");
    } else {
        // Compress: ditto writes the compressed copy. It is used only if it carries
        // the compressed flag and occupies fewer blocks than the original.
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
    // Before touching the original: same bytes, same metadata, and the original
    // has not changed or moved outside the root since staging began.
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
    // The journal must be on disk before the swap. If saving fails, `temporary`
    // drops and removes the work directory. Once it is saved the directory is kept
    // on any error below, with both files, for `recover_original`.
    save(&record)?;
    let _retained = temporary.keep();
    swap(source, &staged)?;
    seals.changed(source);
    File::open(parent)?.sync_all()?;
    // After the swap `staged` holds the original and `source` holds the new copy.
    ensure!(
        identity(&staged)? == record.original && identity(source)? == record.candidate,
        "Replacement identities changed; recovery copies retained"
    );
    ensure!(
        hash(source)? == record.hash,
        "Published bytes failed verification; recovery copies retained"
    );
    metadata_equal(source, &staged)?;
    clear_record(&record)?;
    Ok(true)
}

// Runs one pass over a game folder and returns a summary. Holds native.lock for
// the whole pass. Refuses to start while the folder has a journal, while any file
// under it is open, or when the space plan does not fit. A file that fails is
// counted as skipped and listed in the summary. `cancel` is checked before each
// file and ends the pass with an error. A program opening files ends it early.
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
    let found = survey(&root);
    remove_orphans(&found.work_dirs, &recovery()?)?;
    crate::storage::native_plan(&root, restore)?.recheck()?;
    let mut summary = Progress {
        skipped: found.unreadable,
        ..Progress::default()
    };
    let mut seals = Seals::default();
    let mut failures = Vec::new();
    if found.unreadable > 0 {
        failures.push(format!("{} entries could not be read", found.unreadable));
    }
    let mut stopped = None;
    let mut busy = false;
    let mut last_idle = std::time::Instant::now();
    for path in &found.files {
        if cancel.load(Ordering::Relaxed) {
            stopped = Some(anyhow::anyhow!("Operation stopped"));
            break;
        }
        if last_idle.elapsed() >= IDLE_RECHECK {
            if idle(&root).is_err() {
                busy = true;
                break;
            }
            last_idle = std::time::Instant::now();
        }
        let outcome = visit_file(&root, path, restore, &mut seals, &mut summary);
        report(summary.clone());
        if let Err(error) = outcome {
            failures.push(format!("{}: {error:#}", path.display()));
            // A journal left behind marks a swap that needs review. Stop there.
            let review = match recovery() {
                Ok(records) => records.iter().any(|record| record.root == root),
                Err(_) => true,
            };
            if review {
                stopped = Some(error);
                break;
            }
        }
    }
    let sealed = seals.verify_after();
    if let Some(error) = stopped {
        return Err(match sealed {
            Ok(()) => error,
            Err(seal) => anyhow::anyhow!("{error:#}; {seal:#}"),
        });
    }
    sealed?;
    let mut lines = vec![format!(
        "{} files processed; {} changed, {} skipped. Allocated storage: {} before, {} after.",
        summary.files,
        summary.changed,
        summary.skipped,
        summary.allocation_before,
        summary.allocation_after
    )];
    if !failures.is_empty() {
        lines.push(format!("Skipped after an error ({}):", failures.len()));
        lines.extend(failures.into_iter().take(5));
    }
    if busy {
        lines.push(
            "Stopped early: a program opened files in this game folder. Close it and run the pass again."
                .to_owned(),
        );
    }
    Ok(lines.join("\n"))
}

// What one walk of a game folder finds.
struct Survey {
    // Regular files, in walk order.
    files: Vec<PathBuf>,
    // Staging directories from earlier passes, here and beside an enclosing `.app`.
    work_dirs: Vec<PathBuf>,
    // Entries the walk could not read.
    unreadable: u64,
}
fn is_work_name(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with(WORK_PREFIX)
}
// Lists the folder's files up front, so no directory handle of ours is open while
// the pass re-checks for other programs. Does not descend into staging directories.
fn survey(root: &Path) -> Survey {
    let mut found = Survey {
        files: Vec::new(),
        work_dirs: Vec::new(),
        unreadable: 0,
    };
    let mut walker = walkdir::WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else {
            found.unreadable += 1;
            continue;
        };
        if is_work_name(entry.file_name()) {
            if entry.file_type().is_dir() {
                found.work_dirs.push(entry.path().to_path_buf());
                walker.skip_current_dir();
            }
        } else if entry.file_type().is_file() {
            found.files.push(entry.into_path());
        }
    }
    // Files inside an enclosing bundle stage in the folder that holds it.
    if let Some(outer) = outer_bundle(root)
        && let Some(parent) = outer.parent()
        && let Ok(entries) = std::fs::read_dir(parent)
    {
        for entry in entries.flatten() {
            if is_work_name(&entry.file_name()) && entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                found.work_dirs.push(entry.path());
            }
        }
    }
    found
}
// Removes the staging directories that no journal names, which are left by a pass
// that was killed or whose journal could not be saved. Returns how many it removed.
fn remove_orphans(directories: &[PathBuf], journals: &[Recovery]) -> Result<u64> {
    let mut removed = 0;
    for directory in directories {
        let named = directory.file_name().is_some_and(is_work_name);
        let kept = journals
            .iter()
            .any(|record| record.staged.parent() == Some(directory.as_path()));
        if named && !kept {
            ignore_missing(std::fs::remove_dir_all(directory))?;
            removed += 1;
        }
    }
    Ok(removed)
}
// Processes one file and adds it to the running totals. A file that cannot be
// processed counts as skipped and its error is returned.
fn visit_file(
    root: &Path,
    path: &Path,
    restore: bool,
    seals: &mut Seals,
    summary: &mut Progress,
) -> Result<()> {
    let before = std::fs::symlink_metadata(path);
    summary.files += 1;
    if let Ok(stat) = &before {
        summary.bytes = summary.bytes.saturating_add(stat.len());
        // `blocks()` counts 512-byte units whatever the filesystem's block size.
        summary.allocation_before = summary
            .allocation_before
            .saturating_add(stat.blocks().saturating_mul(512));
    }
    let outcome = match &before {
        Ok(stat) if stat.nlink() == 1 => stage(root, path, restore, seals),
        Ok(_) => Ok(false),
        Err(error) => Err(anyhow::anyhow!("{error}")),
    };
    if matches!(outcome, Ok(true)) {
        summary.changed += 1;
    } else {
        summary.skipped += 1;
    }
    if let Ok(stat) = std::fs::symlink_metadata(path) {
        summary.allocation_after = summary
            .allocation_after
            .saturating_add(stat.blocks().saturating_mul(512));
    }
    outcome.map(|_| ())
}

// Compresses the files under `root`. `report` receives running totals after each file.
pub fn optimize_folder_with(
    root: &Path,
    cancel: &AtomicBool,
    report: impl FnMut(Progress),
) -> Result<String> {
    visit(root, false, cancel, report)
}
// Returns the files under `root` to ordinary storage.
pub fn restore_folder_with(
    root: &Path,
    cancel: &AtomicBool,
    report: impl FnMut(Progress),
) -> Result<String> {
    visit(root, true, cancel, report)
}

// `discover`, with any error turned into an empty list.
pub fn discover_steam() -> Vec<InstalledGame> {
    discover().unwrap_or_default()
}
// Steam games for the `scan` command: the default library plus those named in
// libraryfolders.vdf. An unreadable library or manifest is skipped. A game is kept
// only if its folder exists and resolves inside its library's `common` directory.
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
    // `dedup_by` removes only adjacent duplicates, and the list is sorted by title.
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
// A flag that SIGINT and SIGTERM raise, so a pass stops between files and its
// current work directory is removed.
fn interrupt_flag() -> Result<std::sync::Arc<AtomicBool>> {
    let flag = std::sync::Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let _id = signal_hook::flag::register(signal, std::sync::Arc::clone(&flag))
            .with_context(|| format!("installing the handler for signal {signal}"))?;
    }
    Ok(flag)
}
// A progress reporter that writes running totals to stderr, at most once a second.
fn stderr_progress() -> impl FnMut(Progress) {
    let mut last = std::time::Instant::now();
    move |progress| {
        if last.elapsed() >= std::time::Duration::from_secs(1) {
            last = std::time::Instant::now();
            eprintln!(
                "{} files, {} changed, {} skipped",
                progress.files, progress.changed, progress.skipped
            );
        }
    }
}
// Runs the macOS command line tool.
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
            optimize_folder_with(&folder, &interrupt_flag()?, stderr_progress())?
        ),
        Command::Decompress { folder } => println!(
            "{}",
            restore_folder_with(&folder, &interrupt_flag()?, stderr_progress())?
        ),
        Command::Recovery => println!("{}", serde_json::to_string_pretty(&recovery()?)?),
        Command::Recover { folder } => recover_folder(&folder)?,
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
        // 4 MiB of one byte is the positive control: it must compress.
        let path = root.join("compressible");
        let bytes = vec![b'a'; 4 * 1024 * 1024];
        std::fs::write(&path, &bytes).ctx("source")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ctx("mode")?;
        xattr::set(&path, "user.flummox-fixture", b"metadata").ctx("attribute")?;
        let before = std::fs::symlink_metadata(&path).ctx("original metadata")?;
        check(
            stage(&root, &path, false, &mut Seals::default()).ctx("compress")?,
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
            stage(&root, &path, true, &mut Seals::default()).ctx("restore")?,
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
            .prefix(WORK_PREFIX)
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
        // Reproduce a crash just after the swap: journal on disk, files exchanged.
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
    #[test]
    fn kernel_managed_attributes_are_left_out_of_the_comparison() -> TestResult {
        for name in [
            "com.apple.provenance",
            "com.apple.decmpfs",
            "com.apple.ResourceFork",
        ] {
            check(
                kernel_managed(std::ffi::OsStr::new(name)),
                format!("{name} is kernel-managed"),
            )?;
        }
        check(
            !kernel_managed(std::ffi::OsStr::new("com.apple.quarantine")),
            "quarantine is compared",
        )?;
        check(
            !kernel_managed(std::ffi::OsStr::new("user.flummox-fixture")),
            "user attributes are compared",
        )
    }
    #[test]
    fn too_broad_refuses_mount_points_and_state_ancestors() -> TestResult {
        let mount = Path::new("/Volumes/Games");
        let state = Path::new("/Users/a/Library/Application Support/flummox");
        check(
            too_broad(mount, mount, state).is_some(),
            "a mount point is refused",
        )?;
        check(
            too_broad(Path::new("/Users/a/Library"), mount, state).is_some(),
            "an ancestor of the state directory is refused",
        )?;
        check(
            too_broad(Path::new("/Volumes/Games/Portal"), mount, state).is_none(),
            "a game folder on the volume is accepted",
        )
    }
    #[test]
    fn work_directories_stay_outside_application_bundles() -> TestResult {
        check_eq(
            work_parent(Path::new("/g/Game/data/file.bin")),
            Some(PathBuf::from("/g/Game/data")),
            "an ordinary file stages beside itself",
        )?;
        check_eq(
            work_parent(Path::new("/g/Game/Foo.app/Contents/MacOS/foo")),
            Some(PathBuf::from("/g/Game")),
            "a bundle file stages beside the bundle",
        )?;
        check_eq(
            work_parent(Path::new(
                "/g/Foo.app/Contents/Frameworks/Bar.app/Contents/x",
            )),
            Some(PathBuf::from("/g")),
            "a nested bundle stages beside the outermost one",
        )
    }
    #[test]
    fn staging_locations_accept_both_layouts_and_nothing_else() -> TestResult {
        let root = Path::new("/g/Game");
        let bundle_file = Path::new("/g/Game/Foo.app/Contents/MacOS/foo");
        check(
            staging_expected(
                root,
                bundle_file,
                Path::new("/g/Game/.flummox-work-a/candidate"),
            ),
            "current layout beside the bundle",
        )?;
        check(
            staging_expected(
                root,
                bundle_file,
                Path::new("/g/Game/Foo.app/Contents/MacOS/.flummox-work-a/candidate"),
            ),
            "older layout beside the file",
        )?;
        check(
            !staging_expected(
                root,
                bundle_file,
                Path::new("/elsewhere/.flummox-work-a/candidate"),
            ),
            "a location outside the root is refused",
        )
    }
    #[test]
    fn orphaned_work_directories_are_removed_and_other_folders_kept() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let orphan = fixture.path().join(".flummox-work-orphan");
        let other = fixture.path().join("saves");
        std::fs::create_dir(&orphan).ctx("orphan")?;
        std::fs::write(orphan.join("candidate"), b"partial").ctx("partial copy")?;
        std::fs::create_dir(&other).ctx("other")?;
        let removed =
            remove_orphans(&[orphan.clone(), other.clone()], &[]).ctx("remove orphans")?;
        check_eq(removed, 1, "one directory removed")?;
        check(!orphan.exists(), "the orphan is gone")?;
        check(other.exists(), "a folder without the prefix is kept")
    }
    #[test]
    fn survey_lists_files_and_work_directories_separately() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let work = fixture.path().join(".flummox-work-a");
        std::fs::create_dir(&work).ctx("work")?;
        std::fs::write(work.join("candidate"), b"partial").ctx("partial copy")?;
        std::fs::write(fixture.path().join("game.bin"), b"data").ctx("game file")?;
        let found = survey(fixture.path());
        check_eq(
            found.files,
            vec![fixture.path().join("game.bin")],
            "only the game file is listed",
        )?;
        check_eq(found.work_dirs, vec![work], "the work directory is listed")?;
        check_eq(found.unreadable, 0, "nothing was unreadable")
    }
}
