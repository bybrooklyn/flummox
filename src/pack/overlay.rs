//! Persistent copy-on-write data for writable compressed-store mounts.

use super::{Kind, Reader, format::safe_path};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};

// Layout of an update layer folder:
//   state.json  the journal: every whiteout, rewritten whole on each change
//   files/      the upper tree, mirroring game paths; an entry here wins
//               over the store's entry at the same path
//   trash/      where a removed upper entry is moved before it is deleted
//   staging/    where a copy-up is built before it is published into files/
//   owner.lock  locked for as long as an `Overlay` is open
const STATE: &str = "state.json";
const FILES: &str = "files";
const TRASH: &str = "trash";
const STAGING: &str = "staging";

#[derive(Serialize, Deserialize)]
struct Deleted(#[serde(with = "crate::path_serde")] PathBuf);

// On-disk form of `state.json`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    #[serde(default)]
    deleted: Vec<Deleted>,
}

/// One open update layer. Paths passed in are relative to the game folder.
/// Methods taking a `Reader` must always be given the store this layer was
/// written over: whiteouts and copy-up refer to that store's entries.
pub(super) struct Overlay {
    root: PathBuf,
    files: PathBuf,
    /// Whiteouts: store paths hidden from the merged view, with everything
    /// beneath them. Checked before the upper tree, so a whiteout wins.
    deleted: HashSet<PathBuf>,
    _owner: File,
}

impl Overlay {
    /// Whether `path` is a real folder laid out as an update layer.
    pub fn is_layer(path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
            && path.join(STATE).is_file()
            && path.join(FILES).is_dir()
    }

    /// Opens or creates a layer and takes its lock, failing if another mount
    /// or commit holds it. A new layer is created only in an empty folder.
    /// `store` is the store the layer was written over. With it, a whiteout
    /// dropped because its upper folder exists gets whiteouts over the store
    /// children it hid.
    pub fn open(path: &Path, store: Option<&Reader>) -> Result<Self> {
        if !path.exists() {
            std::fs::create_dir(path)?;
        }
        let root = path.canonicalize()?;
        ensure!(root.is_dir(), "The update layer must be a directory");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(root.join("owner.lock"))?;
        ensure!(
            owner.try_lock().is_ok(),
            "This update layer is already mounted or being committed"
        );
        let files = root.join(FILES);
        let trash = root.join(TRASH);
        let state = root.join(STATE);
        let journal = if state.exists() {
            serde_json::from_reader::<_, Journal>(File::open(&state)?)?
        } else {
            ensure!(
                std::fs::read_dir(&root)?
                    .all(|entry| entry.is_ok_and(|entry| entry.file_name() == "owner.lock")),
                "Choose an empty folder or an existing Flummox update layer"
            );
            std::fs::create_dir(&files)?;
            std::fs::create_dir(&trash)?;
            let journal = Journal {
                version: 1,
                deleted: Vec::new(),
            };
            write_journal(&root, &journal)?;
            journal
        };
        ensure!(
            journal.version == 1 && files.is_dir(),
            "Unsupported update layer"
        );
        // Both hold only work in flight. The layer lock is held, so anything
        // found here was left by a process that died. Removal is best effort.
        let staging = root.join(STAGING);
        for scratch in [&trash, &staging] {
            if !scratch.exists() {
                std::fs::create_dir(scratch)?;
            }
            for entry in std::fs::read_dir(scratch)? {
                let path = entry?.path();
                let _removed = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
            }
        }
        let mut deleted = HashSet::new();
        for Deleted(path) in journal.deleted {
            ensure!(safe_path(&path), "Unsafe path in update journal");
            deleted.insert(path);
        }
        let mut overlay = Self {
            root,
            files,
            deleted,
            _owner: owner,
        };
        // A whiteout whose path exists in the upper tree is dropped. That
        // pair is what a crash leaves between an upper change and the journal
        // write that follows it, and the upper entry is taken as current.
        overlay.drop_stale(store);
        overlay.persist()?;
        Ok(overlay)
    }

    /// Location of `path` in the upper tree, with no checks. Prefer `checked_upper`.
    pub fn upper(&self, path: &Path) -> PathBuf {
        self.files.join(path)
    }

    /// The layer folder itself, which holds `files`, `trash` and the journal.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Location of `path` in the upper tree. Fails if any existing ancestor
    /// there is a symlink or a file, so no caller reads or writes through a
    /// link the game created. The final component is not checked.
    pub fn checked_upper(&self, path: &Path) -> Result<PathBuf> {
        ensure!(
            path.as_os_str().is_empty() || safe_path(path),
            "Invalid update path"
        );
        let mut current = self.files.clone();
        let components: Vec<_> = path.components().collect();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            current.push(component.as_os_str());
            if let Ok(metadata) = std::fs::symlink_metadata(&current) {
                ensure!(
                    metadata.is_dir() && !metadata.is_symlink(),
                    "A symlink cannot be a parent in the update layer"
                );
            }
        }
        Ok(self.upper(path))
    }

    /// True when `path` or any ancestor carries a whiteout.
    pub fn hidden(&self, path: &Path) -> bool {
        path.ancestors()
            .any(|ancestor| self.deleted.contains(ancestor))
    }

    /// True when the merged view has `path`: not hidden, and present in the
    /// upper tree or in the store.
    pub fn visible(&self, reader: &Reader, path: &Path) -> bool {
        !self.hidden(path)
            && (self
                .checked_upper(path)
                .is_ok_and(|upper| std::fs::symlink_metadata(upper).is_ok())
                || reader.entry(path).is_some())
    }

    /// Merged, sorted listing of a directory: store children without a
    /// whiteout, plus upper children. Scans every store entry on each call.
    pub fn children(&self, reader: &Reader, path: &Path) -> Result<Vec<PathBuf>> {
        let mut names = BTreeSet::new();
        for entry in reader.entries() {
            if entry.path.parent() == Some(path)
                && !self.hidden(&entry.path)
                && let Some(name) = entry.path.file_name()
            {
                names.insert(name.to_os_string());
            }
        }
        let upper = self.checked_upper(path)?;
        if upper.is_dir() {
            for entry in std::fs::read_dir(upper)? {
                let entry = entry?;
                if !self.hidden(&path.join(entry.file_name())) {
                    names.insert(entry.file_name());
                }
            }
        }
        Ok(names.into_iter().map(|name| path.join(name)).collect())
    }

    /// Reads from the upper copy when one exists, otherwise from the store.
    /// An upper copy is always a whole file, so the two are never mixed.
    pub fn read(
        &self,
        reader: &Reader,
        path: &Path,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        ensure!(!self.hidden(path), "File was deleted");
        let upper = self.checked_upper(path)?;
        if upper.exists() {
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(upper)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = vec![0; length];
            let count = file.read(&mut bytes)?;
            bytes.truncate(count);
            return Ok(bytes);
        }
        reader.read(path, offset, length)
    }

    /// Makes sure `path` exists in the upper tree and returns its location
    /// there. An existing upper entry is returned untouched. Otherwise the
    /// store's entry is copied whole, with mode, mtime and xattrs. Changes
    /// nothing in the merged view, so it writes no journal entry.
    pub fn copy_up(&mut self, reader: &Reader, path: &Path) -> Result<PathBuf> {
        ensure!(
            safe_path(path) && !self.hidden(path),
            "Invalid copy-up path"
        );
        let output = self.checked_upper(path)?;
        if std::fs::symlink_metadata(&output).is_ok() {
            return Ok(output);
        }
        let entry = reader.entry(path).context("Source file no longer exists")?;
        self.ensure_parent(reader, path)?;
        match &entry.kind {
            Kind::File { size, chunks } => {
                // The copy is built in a temporary file beside its target,
                // synced, then published without replacement, so a crash
                // never leaves a partial file under the real name. Zero
                // chunks are skipped with a seek and become holes.
                let parent = output.parent().context("Missing update parent")?;
                let mut staged = tempfile::NamedTempFile::new_in(self.root.join(STAGING))?;
                for id in chunks {
                    if let Some(length) = reader.zero_chunk_len(*id)? {
                        staged.seek(SeekFrom::Current(i64::from(length)))?;
                    } else {
                        staged.write_all(reader.chunk(*id)?.as_slice())?;
                    }
                }
                staged.as_file().set_len(*size)?;
                super::restore::apply_xattrs(staged.path(), &entry.xattrs)?;
                staged
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(entry.mode))?;
                staged
                    .as_file()
                    .set_times(
                        std::fs::FileTimes::new().set_modified(super::restore::modified(
                            entry.modified_secs,
                            entry.modified_nanos,
                        )?),
                    )?;
                staged.as_file().sync_all()?;
                staged
                    .persist_noclobber(&output)
                    .map_err(|error| error.error)?;
                File::open(parent)?.sync_all()?;
                // Copying a hard-link target also links each visible alias
                // to the new upper file, so a write still reaches every name.
                // Copying an alias path brings up that one name alone.
                if entry.hardlink_to.is_none() {
                    for alias in reader.hardlink_aliases(path) {
                        if self.hidden(&alias) {
                            continue;
                        }
                        self.ensure_parent(reader, &alias)?;
                        let alias_output = self.checked_upper(&alias)?;
                        if std::fs::symlink_metadata(&alias_output).is_err() {
                            std::fs::hard_link(&output, alias_output)?;
                        }
                    }
                }
            }
            Kind::SlicedFile { size, .. } => {
                let parent = output.parent().context("Missing update parent")?;
                let mut staged = tempfile::NamedTempFile::new_in(self.root.join(STAGING))?;
                staged.write_all(&reader.read(path, 0, usize::try_from(*size)?)?)?;
                super::restore::apply_xattrs(staged.path(), &entry.xattrs)?;
                staged
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(entry.mode))?;
                staged
                    .as_file()
                    .set_times(
                        std::fs::FileTimes::new().set_modified(super::restore::modified(
                            entry.modified_secs,
                            entry.modified_nanos,
                        )?),
                    )?;
                staged.as_file().sync_all()?;
                staged
                    .persist_noclobber(&output)
                    .map_err(|error| error.error)?;
                File::open(parent)?.sync_all()?;
                if entry.hardlink_to.is_none() {
                    for alias in reader.hardlink_aliases(path) {
                        if self.hidden(&alias) {
                            continue;
                        }
                        self.ensure_parent(reader, &alias)?;
                        let alias_output = self.checked_upper(&alias)?;
                        if std::fs::symlink_metadata(&alias_output).is_err() {
                            std::fs::hard_link(&output, alias_output)?;
                        }
                    }
                }
            }
            Kind::Symlink { target } => {
                symlink(target, &output)?;
                File::open(output.parent().context("Missing update parent")?)?.sync_all()?;
            }
            Kind::Directory => {
                std::fs::create_dir(&output)?;
                super::restore::apply_xattrs(&output, &entry.xattrs)?;
                std::fs::set_permissions(&output, std::fs::Permissions::from_mode(entry.mode))?;
                File::open(&output)?.sync_all()?;
                File::open(output.parent().context("Missing update parent")?)?.sync_all()?;
            }
        }
        Ok(output)
    }

    /// Copies up `path` and, for a directory, everything visible beneath it,
    /// so the whole subtree can then move with one rename in the upper tree.
    /// The directory's mode and mtime are put back after its children land.
    fn copy_up_tree(&mut self, reader: &Reader, path: &Path) -> Result<PathBuf> {
        let upper = self.checked_upper(path)?;
        let upper_metadata = std::fs::symlink_metadata(&upper).ok();
        let base = reader.entry(path);
        let directory = upper_metadata
            .as_ref()
            .is_some_and(std::fs::Metadata::is_dir)
            || base.is_some_and(|entry| matches!(entry.kind, Kind::Directory));
        let output = self.copy_up(reader, path)?;
        if !directory {
            return Ok(output);
        }
        let children = self.children(reader, path)?;
        for child in children {
            let _copied = self.copy_up_tree(reader, &child)?;
        }
        let (permissions, modified) = match upper_metadata {
            Some(metadata) => (metadata.permissions(), metadata.modified()?),
            None => {
                let entry = base.context("Source directory no longer exists")?;
                (
                    std::fs::Permissions::from_mode(entry.mode),
                    super::restore::modified(entry.modified_secs, entry.modified_nanos)?,
                )
            }
        };
        std::fs::set_permissions(&output, permissions)?;
        File::open(&output)?.set_times(std::fs::FileTimes::new().set_modified(modified))?;
        Ok(output)
    }

    /// Creates the missing upper directories above `path`, each with its
    /// store entry's mode. Fails if a missing ancestor is not a store directory.
    fn ensure_parent(&mut self, reader: &Reader, path: &Path) -> Result<()> {
        let parent = path.parent().context("Missing parent")?;
        if parent.as_os_str().is_empty() {
            return Ok(());
        }
        let output = self.checked_upper(parent)?;
        if output.is_dir() {
            return Ok(());
        }
        self.ensure_parent(reader, parent)?;
        let entry = reader
            .entry(parent)
            .context("Parent directory does not exist")?;
        ensure!(
            matches!(entry.kind, Kind::Directory),
            "Parent is not a directory"
        );
        std::fs::create_dir(&output)?;
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(entry.mode))?;
        Ok(())
    }

    /// Creates a new empty file where the merged view has nothing. Like
    /// `mkdir` and `symlink`, it makes the upper entry first and then clears
    /// any whiteout at `path` in the journal.
    pub fn create_file(&mut self, reader: &Reader, path: &Path, mode: u32) -> Result<File> {
        ensure!(
            safe_path(path) && !self.visible(reader, path),
            "File already exists or path is invalid"
        );
        self.ensure_parent(reader, path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(mode & 0o777)
            .open(self.checked_upper(path)?)?;
        self.reveal(reader, path);
        self.persist()?;
        Ok(file)
    }

    /// Creates a directory where the merged view has nothing.
    pub fn mkdir(&mut self, reader: &Reader, path: &Path, mode: u32) -> Result<()> {
        ensure!(
            safe_path(path) && !self.visible(reader, path),
            "Directory already exists or path is invalid"
        );
        self.ensure_parent(reader, path)?;
        std::fs::create_dir(self.checked_upper(path)?)?;
        std::fs::set_permissions(
            self.checked_upper(path)?,
            std::fs::Permissions::from_mode(mode & 0o777),
        )?;
        self.reveal(reader, path);
        self.persist()?;
        Ok(())
    }

    /// Creates a symlink where the merged view has nothing. Absolute targets
    /// are refused. A relative target is stored as given, without resolving it.
    pub fn symlink(&mut self, reader: &Reader, path: &Path, target: &Path) -> Result<()> {
        ensure!(
            safe_path(path) && !self.visible(reader, path),
            "Link already exists or path is invalid"
        );
        ensure!(
            !target.as_os_str().is_empty() && !target.is_absolute(),
            "Only relative links are supported"
        );
        self.ensure_parent(reader, path)?;
        symlink(target, self.checked_upper(path)?)?;
        self.reveal(reader, path);
        self.persist()?;
        Ok(())
    }

    /// Removes one visible path. With `directory` set it must have no visible
    /// children. An upper entry is moved to `trash` and deleted when this
    /// returns. A path the store also holds gets a whiteout, and if the
    /// journal write fails the upper entry is moved back.
    pub fn remove(&mut self, reader: &Reader, path: &Path, directory: bool) -> Result<()> {
        ensure!(
            safe_path(path) && self.visible(reader, path),
            "Path does not exist"
        );
        if directory {
            ensure!(
                self.children(reader, path)?.is_empty(),
                "Directory is not empty"
            );
        }
        let upper = self.checked_upper(path)?;
        // Order: write the whiteout, then move the upper entry to trash,
        // which deletes it as `staged` drops. A crash between the two leaves
        // a whiteout beside an upper entry, and `open` drops the whiteout, so
        // the entry is still there. The other order would show the store's
        // old version of the path after a crash.
        let in_store = reader.entry(path).is_some();
        // Made before the whiteout so that nothing after it can fail except
        // the move itself, which is undone below.
        let staged = if std::fs::symlink_metadata(&upper).is_ok() {
            Some(
                tempfile::Builder::new()
                    .prefix("removed-")
                    .tempdir_in(self.root.join(TRASH))?,
            )
        } else {
            None
        };
        if in_store {
            self.deleted.insert(path.to_path_buf());
            if let Err(error) = self.persist() {
                self.deleted.remove(path);
                return Err(error);
            }
        }
        if let Some(staged) = &staged
            && let Err(error) = std::fs::rename(&upper, staged.path().join("entry"))
        {
            if in_store {
                self.deleted.remove(path);
                self.persist()?;
            }
            return Err(error.into());
        }
        Ok(())
    }

    /// Renames a visible file or directory. A replaced target must be the
    /// same type, and a replaced directory must be empty. The source subtree
    /// is copied up in full first, so renaming a large store directory writes
    /// all of it to the update layer.
    pub fn rename(
        &mut self,
        reader: &Reader,
        from: &Path,
        to: &Path,
        no_replace: bool,
    ) -> Result<()> {
        ensure!(
            safe_path(from) && safe_path(to) && self.visible(reader, from),
            "Invalid rename source"
        );
        if from == to {
            return Ok(());
        }
        let source_upper = self.checked_upper(from)?;
        let source_is_dir = std::fs::symlink_metadata(&source_upper)
            .map(|metadata| metadata.is_dir())
            .unwrap_or_else(|_| {
                reader
                    .entry(from)
                    .is_some_and(|entry| matches!(entry.kind, Kind::Directory))
            });
        ensure!(
            !source_is_dir || !to.starts_with(from),
            "A directory cannot be moved inside itself"
        );
        if no_replace {
            ensure!(!self.visible(reader, to), "Rename destination exists");
        }
        if self.visible(reader, to) {
            let target_upper = self.checked_upper(to)?;
            let target_is_dir = std::fs::symlink_metadata(&target_upper)
                .map(|metadata| metadata.is_dir())
                .unwrap_or_else(|_| {
                    reader
                        .entry(to)
                        .is_some_and(|entry| matches!(entry.kind, Kind::Directory))
                });
            ensure!(
                source_is_dir == target_is_dir,
                "Rename source and destination types differ"
            );
            if target_is_dir {
                ensure!(
                    self.children(reader, to)?.is_empty(),
                    "Rename destination directory is not empty"
                );
            }
        }
        // Order: copy the subtree up, write a whiteout for a source the store
        // holds, rename in the upper tree, then clear any whiteout at the
        // target. A crash after the whiteout and before the rename leaves
        // both the whiteout and the upper source, and `open` drops the
        // whiteout, so the source is still there.
        let source = self.copy_up_tree(reader, from)?;
        self.ensure_parent(reader, to)?;
        let target = self.checked_upper(to)?;
        let base_source = reader.entry(from).is_some();
        if base_source {
            self.deleted.insert(from.to_path_buf());
            self.persist()?;
        }
        if let Err(error) = std::fs::rename(source, target) {
            if base_source {
                self.deleted.remove(from);
                self.persist()?;
            }
            return Err(error.into());
        }
        self.reveal(reader, to);
        self.persist()
    }

    /// Hard-links a visible regular file to a new name. The source is copied
    /// up first, because a link can only be made between two upper files.
    pub fn link(&mut self, reader: &Reader, from: &Path, to: &Path) -> Result<()> {
        ensure!(
            safe_path(from)
                && safe_path(to)
                && self.visible(reader, from)
                && !self.visible(reader, to),
            "Invalid hard-link request"
        );
        let source = self.copy_up(reader, from)?;
        ensure!(
            std::fs::symlink_metadata(&source)?.is_file(),
            "Hard-link source is not a file"
        );
        self.ensure_parent(reader, to)?;
        std::fs::hard_link(source, self.checked_upper(to)?)?;
        self.reveal(reader, to);
        self.persist()
    }

    /// Replays this layer onto an ordinary copy of the store's tree: removes
    /// every whited-out path, deepest first, then copies the upper tree over
    /// it. Edits `destination` in place and is not atomic. The layer itself
    /// is not changed.
    pub fn apply_to(&self, destination: &Path) -> Result<()> {
        let destination = destination.canonicalize()?;
        let mut deleted: Vec<_> = self.deleted.iter().collect();
        deleted.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for rel in deleted {
            let path = destination.join(rel);
            // A parent that is now a file holds nothing to remove.
            if !parents_are_directories(&destination, rel, true)? {
                continue;
            }
            if let Ok(metadata) = std::fs::symlink_metadata(&path) {
                if metadata.is_dir() {
                    std::fs::remove_dir_all(path)?;
                } else {
                    std::fs::remove_file(path)?;
                }
            }
        }
        let mut directories = Vec::new();
        let mut hardlinks = HashMap::<(u64, u64), PathBuf>::new();
        for entry in walkdir::WalkDir::new(&self.files)
            .follow_links(false)
            .sort_by_file_name()
        {
            let entry = entry?;
            let rel = entry.path().strip_prefix(&self.files)?;
            if rel.as_os_str().is_empty() {
                continue;
            }
            ensure!(safe_path(rel), "Unsafe path in update files");
            let output = destination.join(rel);
            let metadata = std::fs::symlink_metadata(entry.path())?;
            ensure!(
                parents_are_directories(&destination, rel, false)?,
                "The folder for {} is missing from the destination",
                rel.display()
            );
            if metadata.is_dir() {
                // A link or file where the layer holds a folder is replaced.
                // Creating through it would write wherever the link points.
                match std::fs::symlink_metadata(&output) {
                    // The final pass puts the layer's mode back. Until then
                    // the owner needs write access to add attributes.
                    Ok(old) if old.is_dir() => std::fs::set_permissions(
                        &output,
                        std::fs::Permissions::from_mode(old.permissions().mode() | 0o700),
                    )?,
                    Ok(_) => {
                        std::fs::remove_file(&output)?;
                        std::fs::create_dir(&output)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        std::fs::create_dir(&output)?;
                    }
                    Err(error) => return Err(error.into()),
                }
                directories.push((output, metadata));
            } else {
                if let Ok(old) = std::fs::symlink_metadata(&output) {
                    if old.is_dir() {
                        std::fs::remove_dir_all(&output)?;
                    } else {
                        std::fs::remove_file(&output)?;
                    }
                }
                if metadata.is_symlink() {
                    symlink(std::fs::read_link(entry.path())?, &output)?;
                } else if let Some(target) = hardlinks.get(&(metadata.dev(), metadata.ino())) {
                    std::fs::hard_link(target, &output)?;
                } else {
                    // Created owner-writable: `fs::copy` would give it the
                    // source's mode, and a read-only file takes no attribute.
                    let mut copy = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&output)?;
                    std::io::copy(&mut File::open(entry.path())?, &mut copy)?;
                    super::restore::copy_xattrs(entry.path(), &output)?;
                    std::fs::set_permissions(&output, metadata.permissions())?;
                    File::open(&output)?
                        .set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
                    hardlinks.insert((metadata.dev(), metadata.ino()), output.clone());
                }
            }
        }
        for (path, metadata) in directories.into_iter().rev() {
            super::restore::copy_xattrs(&self.files.join(path.strip_prefix(&destination)?), &path)?;
            std::fs::set_permissions(&path, metadata.permissions())?;
            File::open(path)?
                .set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
        }
        Ok(())
    }

    // Drops every whiteout that has an upper entry. The store children of a
    // dropped folder get whiteouts, which are checked in turn.
    fn drop_stale(&mut self, store: Option<&Reader>) {
        loop {
            let stale: Vec<PathBuf> = self
                .deleted
                .iter()
                .filter(|hidden| std::fs::symlink_metadata(self.files.join(hidden)).is_ok())
                .cloned()
                .collect();
            if stale.is_empty() {
                return;
            }
            for hidden in stale {
                self.deleted.remove(&hidden);
                if let Some(store) = store {
                    self.hide_children(store, &hidden);
                }
            }
        }
    }

    /// Makes `path` visible again after something was created or moved there.
    /// Store children the old whiteout hid get whiteouts of their own. A
    /// whiteout below `path` that now has an upper entry is dropped and
    /// replaced by whiteouts over that entry's store children.
    fn reveal(&mut self, reader: &Reader, path: &Path) {
        if self.deleted.remove(path) {
            self.hide_children(reader, path);
        }
        loop {
            let stale: Vec<PathBuf> = self
                .deleted
                .iter()
                .filter(|hidden| {
                    hidden.starts_with(path)
                        && std::fs::symlink_metadata(self.files.join(hidden)).is_ok()
                })
                .cloned()
                .collect();
            if stale.is_empty() {
                return;
            }
            for hidden in stale {
                self.deleted.remove(&hidden);
                self.hide_children(reader, &hidden);
            }
        }
    }

    // Whiteouts for every store child of `path`.
    fn hide_children(&mut self, reader: &Reader, path: &Path) {
        for entry in reader.entries() {
            if entry.path.parent() == Some(path) {
                self.deleted.insert(entry.path.clone());
            }
        }
    }

    // Writes the whole whiteout set to the journal, sorted.
    fn persist(&self) -> Result<()> {
        let mut deleted: Vec<_> = self.deleted.iter().cloned().map(Deleted).collect();
        deleted.sort_by(|a, b| a.0.cmp(&b.0));
        write_journal(
            &self.root,
            &Journal {
                version: 1,
                deleted,
            },
        )
    }
}

/// Replaces `state.json` atomically: temporary file, sync, rename over the
/// old journal, then sync the folder. A reader sees the old or the new set.
fn write_journal(root: &Path, journal: &Journal) -> Result<()> {
    let bytes = serde_json::to_vec(journal)?;
    let mut staged = tempfile::NamedTempFile::new_in(root)?;
    staged.write_all(&bytes)?;
    staged.as_file().sync_all()?;
    staged
        .persist(root.join(STATE))
        .map_err(|error| error.error)?;
    File::open(root)?.sync_all()?;
    Ok(())
}

/// Builds a new store at `output` from `store` with the layer at `writes`
/// applied. Holds the layer's lock, so a mounted layer is refused. Needs
/// scratch space for a full uncompressed copy. The old store and the layer
/// are left as they were.
pub(super) fn commit(
    store: &Path,
    writes: &Path,
    output: &Path,
    scratch: Option<&Path>,
    options: super::Options,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<super::Summary> {
    let reader = Reader::open(store)?;
    let overlay = Overlay::open(writes, Some(&reader))?;
    let temporary = match scratch {
        Some(path) => tempfile::tempdir_in(path)?,
        None => tempfile::tempdir()?,
    };
    let merged = temporary.path().join("merged");
    super::restore(store, &merged, cancel)?;
    ensure!(
        !cancel.load(std::sync::atomic::Ordering::Relaxed),
        "Update commit cancelled"
    );
    overlay.apply_to(&merged)?;
    super::create(&merged, output, options, cancel)
}

/// Whether every folder between `root` and `rel` exists as a real directory.
///
/// Fails when one of them is a symlink, because a write or removal below it
/// would land outside `root`. A plain file there fails too, unless
/// `file_is_gone` makes it count like a missing folder. `false` means a
/// folder is missing.
fn parents_are_directories(root: &Path, rel: &Path, file_is_gone: bool) -> Result<bool> {
    let mut current = root.to_path_buf();
    for part in rel.parent().into_iter().flat_map(Path::components) {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if file_is_gone && metadata.is_file() => return Ok(false),
            Ok(metadata) => ensure!(
                metadata.is_dir(),
                "{} is not a folder, so updates below it were not applied",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    #[test]
    fn applying_updates_never_writes_through_a_destination_symlink() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let outside = temp.path().join("outside");
        let game = temp.path().join("game");
        std::fs::create_dir(&outside).ctx("outside")?;
        std::fs::create_dir(&game).ctx("game")?;
        std::fs::write(outside.join("x"), b"victim").ctx("outside file")?;
        symlink("../outside", game.join("l")).ctx("destination symlink")?;
        // The layer a game leaves after replacing that link with a folder.
        let layer = temp.path().join("layer");
        drop(Overlay::open(&layer, None).ctx("new layer")?);
        std::fs::create_dir(layer.join(FILES).join("l")).ctx("layer folder")?;
        std::fs::write(layer.join(FILES).join("l/x"), b"planted").ctx("layer file")?;

        // Another test forking a child shares the lock until that child
        // execs, so the layer can look held for an instant after the drop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let reopened = loop {
            match Overlay::open(&layer, None) {
                Ok(reopened) => break reopened,
                Err(error) => check(
                    std::time::Instant::now() < deadline,
                    format!("layer: {error}"),
                )?,
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        reopened.apply_to(&game).ctx("apply")?;
        check_eq(
            std::fs::read(outside.join("x")).ctx("outside after")?,
            b"victim".to_vec(),
            "the folder behind the link is untouched",
        )?;
        check(
            std::fs::symlink_metadata(game.join("l"))
                .ctx("replaced link")?
                .is_dir(),
            "the link became the folder the layer holds",
        )?;
        check_eq(
            std::fs::read(game.join("l/x")).ctx("applied file")?,
            b"planted".to_vec(),
            "the update landed inside the game",
        )
    }

    // A layer is reopened after another handle drops it. Another test forking
    // a child shares the lock until that child execs.
    fn reopen(layer: &Path) -> Result<Overlay, String> {
        reopen_over(layer, None)
    }

    fn reopen_over(layer: &Path, store: Option<&Reader>) -> Result<Overlay, String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match Overlay::open(layer, store) {
                Ok(reopened) => return Ok(reopened),
                Err(error) => check(
                    std::time::Instant::now() < deadline,
                    format!("layer: {error}"),
                )?,
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn store_from(temp: &Path, files: &[&str]) -> Result<Reader, String> {
        let source = temp.join("source");
        for file in files {
            let path = source.join(file);
            std::fs::create_dir_all(path.parent().ctx("parent")?).ctx("source folder")?;
            std::fs::write(&path, b"store bytes").ctx("source file")?;
        }
        let store = temp.join("game.flumpack");
        crate::pack::create(
            &source,
            &store,
            crate::pack::Options::default(),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .ctx("store")?;
        Reader::open(&store).ctx("reader")
    }

    #[test]
    fn a_folder_moved_onto_a_removed_subfolder_does_not_bring_back_store_files() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let reader = store_from(temp.path(), &["data/sub/deep.bin"])?;
        let mut overlay = Overlay::open(&temp.path().join("layer"), None).ctx("layer")?;
        overlay
            .rename(
                &reader,
                Path::new("data/sub"),
                Path::new("elsewhere"),
                false,
            )
            .ctx("move the store folder away")?;
        overlay
            .mkdir(&reader, Path::new("newdata"), 0o755)
            .ctx("new folder")?;
        overlay
            .mkdir(&reader, Path::new("newdata/sub"), 0o755)
            .ctx("new subfolder")?;
        drop(
            overlay
                .create_file(&reader, Path::new("newdata/sub/new.bin"), 0o644)
                .ctx("new file")?,
        );
        overlay
            .rename(&reader, Path::new("newdata"), Path::new("data"), false)
            .ctx("move the new folder over the emptied one")?;
        check(
            overlay.visible(&reader, Path::new("data/sub/new.bin")),
            "the new file is visible",
        )?;
        check(
            !overlay.visible(&reader, Path::new("data/sub/deep.bin")),
            "the store file moved away stays gone",
        )?;
        check_eq(
            overlay
                .children(&reader, Path::new("data/sub"))
                .ctx("listing")?,
            vec![PathBuf::from("data/sub/new.bin")],
            "only the new file is listed",
        )?;
        drop(overlay);
        let reopened = reopen(&temp.path().join("layer"))?;
        check(
            !reopened.visible(&reader, Path::new("data/sub/deep.bin")),
            "the whiteout survives a reopen",
        )
    }

    #[test]
    fn a_crash_between_mkdir_and_the_journal_keeps_store_children_hidden() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let reader = store_from(temp.path(), &["data/sub/deep.bin", "data/sub/inner/x.bin"])?;
        let layer = temp.path().join("layer");
        let mut overlay = Overlay::open(&layer, None).ctx("layer")?;
        overlay
            .rename(
                &reader,
                Path::new("data/sub"),
                Path::new("elsewhere"),
                false,
            )
            .ctx("move the store folder away")?;
        drop(overlay);
        // The state a crash leaves: the folder was created in the upper tree
        // but the journal still holds only the whiteout over the folder.
        std::fs::create_dir_all(layer.join(FILES).join("data/sub")).ctx("upper folder")?;
        let without_store = reopen_over(&layer, None)?;
        check(
            without_store.visible(&reader, Path::new("data/sub/deep.bin")),
            "control: opened without the store, the stale file reappears",
        )?;
        drop(without_store);
        // Put the crash state back, since the open above rewrote the journal.
        let mut again = reopen_over(&layer, Some(&reader))?;
        again.deleted.insert(PathBuf::from("data/sub"));
        again.persist().ctx("journal")?;
        drop(again);
        let reopened = reopen_over(&layer, Some(&reader))?;
        for stale in ["data/sub/deep.bin", "data/sub/inner", "data/sub/inner/x.bin"] {
            check(
                !reopened.visible(&reader, Path::new(stale)),
                format!("{stale} stays hidden after the crash"),
            )?;
        }
        check_eq(
            reopened
                .children(&reader, Path::new("data/sub"))
                .ctx("listing")?,
            Vec::<PathBuf>::new(),
            "the recreated folder is empty",
        )?;
        drop(reopened);
        let third = reopen_over(&layer, Some(&reader))?;
        check(
            !third.visible(&reader, Path::new("data/sub/deep.bin")),
            "the repaired journal keeps them hidden on the next open",
        )
    }

    #[test]
    fn stale_trash_is_emptied_when_a_layer_opens() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let layer = temp.path().join("layer");
        drop(Overlay::open(&layer, None).ctx("new layer")?);
        let stale = layer.join(TRASH).join("removed-left-behind");
        std::fs::create_dir(&stale).ctx("stale folder")?;
        std::fs::write(stale.join("entry"), b"half deleted").ctx("stale entry")?;
        let _reopened = reopen(&layer)?;
        check(
            std::fs::read_dir(layer.join(TRASH))
                .ctx("trash")?
                .next()
                .is_none(),
            "the trash is empty after opening",
        )
    }

    #[test]
    fn copy_up_builds_files_outside_the_visible_tree() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let reader = store_from(temp.path(), &["a", "dir/b"])?;
        let layer = temp.path().join("layer");
        let mut overlay = Overlay::open(&layer, None).ctx("layer")?;
        // A copy-up that died after staging leaves its temporary file here.
        let stale = layer.join(STAGING).join(".tmpleftover");
        std::fs::write(&stale, b"partial").ctx("stale staging file")?;
        overlay
            .copy_up(&reader, Path::new("dir/b"))
            .ctx("copy up")?;
        drop(overlay);
        let _reopened = reopen(&layer)?;
        check(!stale.exists(), "staging is emptied when the layer opens")?;
        let mut visible = Vec::new();
        for entry in walkdir::WalkDir::new(layer.join(FILES)) {
            visible.push(entry.ctx("walk")?.into_path());
        }
        check_eq(
            visible.len(),
            3,
            "the upper tree holds its root, one folder and the copied file",
        )
    }

    #[test]
    fn applying_whiteouts_skips_a_path_whose_parent_became_a_file() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let mut overlay = Overlay::open(&temp.path().join("layer"), None).ctx("layer")?;
        overlay.deleted.insert(PathBuf::from("D/a"));
        overlay.persist().ctx("journal")?;
        let destination = temp.path().join("dest");
        std::fs::create_dir(&destination).ctx("destination")?;
        std::fs::write(destination.join("D"), b"now a file").ctx("replaced folder")?;
        overlay
            .apply_to(&destination)
            .ctx("a whiteout under a file is already satisfied")?;
        check_eq(
            std::fs::read(destination.join("D")).ctx("file")?,
            b"now a file".to_vec(),
            "the file is untouched",
        )?;
        // Control: a symlink parent still stops the replay.
        std::fs::remove_file(destination.join("D")).ctx("remove file")?;
        symlink("..", destination.join("D")).ctx("link")?;
        check(
            overlay.apply_to(&destination).is_err(),
            "a symlink parent is refused",
        )
    }

    #[test]
    fn read_only_store_entries_keep_their_attributes_when_copied_up_and_applied() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let source = temp.path().join("source");
        std::fs::create_dir_all(source.join("rd")).ctx("source folder")?;
        std::fs::write(source.join("ro"), b"read only").ctx("source file")?;
        std::fs::write(source.join("rd/inner"), b"inner").ctx("source inner")?;
        for path in ["ro", "rd"] {
            xattr::set(source.join(path), "user.flummox-test", b"kept").ctx("source xattr")?;
        }
        std::fs::set_permissions(source.join("ro"), std::fs::Permissions::from_mode(0o444))
            .ctx("file mode")?;
        std::fs::set_permissions(source.join("rd"), std::fs::Permissions::from_mode(0o555))
            .ctx("folder mode")?;
        let store = temp.path().join("game.flumpack");
        let created = crate::pack::create(
            &source,
            &store,
            crate::pack::Options::default(),
            &std::sync::atomic::AtomicBool::new(false),
        );
        let layer = temp.path().join("layer");
        let destination = temp.path().join("dest");
        let outcome = (|| -> TestResult {
            created.ctx("store")?;
            let reader = Reader::open(&store).ctx("reader")?;
            let mut overlay = Overlay::open(&layer, None).ctx("layer")?;
            for path in ["ro", "rd"] {
                let upper = overlay.copy_up(&reader, Path::new(path)).ctx("copy up")?;
                check_eq(
                    xattr::get(&upper, "user.flummox-test").ctx("upper xattr")?,
                    Some(b"kept".to_vec()),
                    format!("{path} keeps its attribute in the layer"),
                )?;
            }
            crate::pack::restore(
                &store,
                &destination,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .ctx("restore")?;
            overlay.apply_to(&destination).ctx("apply")?;
            for path in ["ro", "rd"] {
                check_eq(
                    xattr::get(destination.join(path), "user.flummox-test").ctx("applied xattr")?,
                    Some(b"kept".to_vec()),
                    format!("{path} keeps its attribute after the replay"),
                )?;
            }
            Ok(())
        })();
        // Write access back, so the temporary folder can be removed.
        for root in [source.clone(), layer.join(FILES), destination.clone()] {
            let _restored =
                std::fs::set_permissions(root.join("rd"), std::fs::Permissions::from_mode(0o755));
        }
        outcome
    }
}
