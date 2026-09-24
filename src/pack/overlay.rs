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

const STATE: &str = "state.json";
const FILES: &str = "files";
const TRASH: &str = "trash";

#[derive(Serialize, Deserialize)]
struct Deleted(#[serde(with = "crate::path_serde")] PathBuf);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    #[serde(default)]
    deleted: Vec<Deleted>,
}

pub(super) struct Overlay {
    root: PathBuf,
    files: PathBuf,
    deleted: HashSet<PathBuf>,
    _owner: File,
}

impl Overlay {
    pub fn open(path: &Path) -> Result<Self> {
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
        if !trash.exists() {
            std::fs::create_dir(&trash)?;
        }
        let mut deleted = HashSet::new();
        for Deleted(path) in journal.deleted {
            ensure!(safe_path(&path), "Unsafe path in update journal");
            deleted.insert(path);
        }
        deleted.retain(|path| std::fs::symlink_metadata(files.join(path)).is_err());
        let overlay = Self {
            root,
            files,
            deleted,
            _owner: owner,
        };
        overlay.persist()?;
        Ok(overlay)
    }

    pub fn upper(&self, path: &Path) -> PathBuf {
        self.files.join(path)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

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

    pub fn hidden(&self, path: &Path) -> bool {
        path.ancestors()
            .any(|ancestor| self.deleted.contains(ancestor))
    }

    pub fn visible(&self, reader: &Reader, path: &Path) -> bool {
        !self.hidden(path)
            && (self
                .checked_upper(path)
                .is_ok_and(|upper| std::fs::symlink_metadata(upper).is_ok())
                || reader.entry(path).is_some())
    }

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
                let parent = output.parent().context("Missing update parent")?;
                let mut staged = tempfile::NamedTempFile::new_in(parent)?;
                for id in chunks {
                    if let Some(length) = reader.zero_chunk_len(*id)? {
                        staged.seek(SeekFrom::Current(i64::from(length)))?;
                    } else {
                        staged.write_all(reader.chunk(*id)?.as_slice())?;
                    }
                }
                staged.as_file().set_len(*size)?;
                staged
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(entry.mode))?;
                super::restore::apply_xattrs(staged.path(), &entry.xattrs)?;
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
            Kind::SlicedFile { size, .. } => {
                let parent = output.parent().context("Missing update parent")?;
                let mut staged = tempfile::NamedTempFile::new_in(parent)?;
                staged.write_all(&reader.read(path, 0, usize::try_from(*size)?)?)?;
                staged
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(entry.mode))?;
                super::restore::apply_xattrs(staged.path(), &entry.xattrs)?;
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
            Kind::Symlink { target } => symlink(target, &output)?,
            Kind::Directory => {
                std::fs::create_dir(&output)?;
                std::fs::set_permissions(&output, std::fs::Permissions::from_mode(entry.mode))?;
                super::restore::apply_xattrs(&output, &entry.xattrs)?;
            }
        }
        Ok(output)
    }

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
        self.reveal(path);
        self.persist()?;
        Ok(file)
    }

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
        self.reveal(path);
        self.persist()?;
        Ok(())
    }

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
        self.reveal(path);
        self.persist()?;
        Ok(())
    }

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
        let staged = if std::fs::symlink_metadata(&upper).is_ok() {
            let staged = tempfile::Builder::new()
                .prefix("removed-")
                .tempdir_in(self.root.join(TRASH))?;
            std::fs::rename(&upper, staged.path().join("entry"))?;
            Some(staged)
        } else {
            None
        };
        if reader.entry(path).is_some() {
            self.deleted.insert(path.to_path_buf());
            if let Err(error) = self.persist() {
                self.deleted.remove(path);
                if let Some(staged) = &staged {
                    let _restored = std::fs::rename(staged.path().join("entry"), &upper);
                }
                return Err(error);
            }
        }
        Ok(())
    }

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
        self.reveal(to);
        self.persist()
    }

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
        self.reveal(to);
        self.persist()
    }

    pub fn apply_to(&self, destination: &Path) -> Result<()> {
        let destination = destination.canonicalize()?;
        let mut deleted: Vec<_> = self.deleted.iter().collect();
        deleted.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for rel in deleted {
            let path = destination.join(rel);
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
            if metadata.is_dir() {
                std::fs::create_dir_all(&output)?;
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
                    std::fs::copy(entry.path(), &output)?;
                    std::fs::set_permissions(&output, metadata.permissions())?;
                    super::restore::copy_xattrs(entry.path(), &output)?;
                    File::open(&output)?
                        .set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
                    hardlinks.insert((metadata.dev(), metadata.ino()), output.clone());
                }
            }
        }
        for (path, metadata) in directories.into_iter().rev() {
            std::fs::set_permissions(&path, metadata.permissions())?;
            super::restore::copy_xattrs(&self.files.join(path.strip_prefix(&destination)?), &path)?;
            File::open(path)?
                .set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
        }
        Ok(())
    }

    fn reveal(&mut self, path: &Path) {
        self.deleted.remove(path);
    }

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

pub(super) fn commit(
    store: &Path,
    writes: &Path,
    output: &Path,
    scratch: Option<&Path>,
    options: super::Options,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<super::Summary> {
    let overlay = Overlay::open(writes)?;
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
