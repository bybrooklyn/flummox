//! Optional owner-only view of a store, with a persistent copy-on-write layer.

use super::{Entry, Kind, Reader, overlay::Overlay};
use anyhow::{Context, Result, ensure};
use fuser::{Errno, FileAttr, FileType, Filesystem, INodeNo, ReplyXattr};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{FileTimes, OpenOptions, Permissions},
    io::{Seek, SeekFrom, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

// How long the kernel may cache an attribute or lookup reply before asking again.
const TTL: Duration = Duration::from_secs(1);

// Inode numbers for the life of one mount. A store entry's inode is its index
// plus one, which makes the root inode 1 as FUSE expects. Hard-link aliases
// share their target's number. Paths created later take numbers from `next`.
// Nothing is ever removed: a deleted path keeps its number, and a new file at
// the same path reuses it.
struct Nodes {
    paths: HashMap<PathBuf, u64>,
    inodes: HashMap<u64, Vec<PathBuf>>,
    next: u64,
}

impl Nodes {
    // Relies on a hard-link target preceding its aliases, which
    // `Index::validate` enforces. The target is therefore first in its list.
    fn new(entries: &[Entry]) -> Self {
        let mut paths = HashMap::new();
        let mut inodes: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            let inode = entry
                .hardlink_to
                .as_ref()
                .and_then(|target| paths.get(target))
                .copied()
                .unwrap_or(index as u64 + 1);
            paths.insert(entry.path.clone(), inode);
            inodes.entry(inode).or_default().push(entry.path.clone());
        }
        Self {
            paths,
            inodes,
            next: entries.len() as u64 + 1,
        }
    }

    fn inode(&mut self, path: &Path) -> u64 {
        if let Some(inode) = self.paths.get(path) {
            return *inode;
        }
        let inode = self.next;
        self.next = self.next.saturating_add(1);
        self.paths.insert(path.to_path_buf(), inode);
        self.inodes.insert(inode, vec![path.to_path_buf()]);
        inode
    }

    fn alias(&mut self, path: &Path, inode: u64) {
        self.paths.insert(path.to_path_buf(), inode);
        self.inodes
            .entry(inode)
            .or_default()
            .push(path.to_path_buf());
    }

    fn link_count(&self, inode: u64) -> u32 {
        self.inodes
            .get(&inode)
            .and_then(|paths| u32::try_from(paths.len()).ok())
            .unwrap_or(1)
    }

    // Moves `from` and every path beneath it to `to`, keeping their inode
    // numbers. A path replaced at the target loses its mapping.
    fn rename(&mut self, from: &Path, to: &Path) {
        if from == to {
            return;
        }
        let moved: Vec<_> = self
            .paths
            .iter()
            .filter(|(path, _)| path.as_path() == from || path.starts_with(from))
            .filter_map(|(path, inode)| {
                let suffix = path.strip_prefix(from).ok()?;
                Some((path.clone(), to.join(suffix), *inode))
            })
            .collect();
        for (old, _, _) in &moved {
            self.paths.remove(old);
        }
        for (old, new, inode) in moved {
            if let Some(replaced) = self.paths.insert(new.clone(), inode)
                && let Some(paths) = self.inodes.get_mut(&replaced)
            {
                paths.retain(|path| path != &new);
            }
            if let Some(paths) = self.inodes.get_mut(&inode) {
                for path in paths {
                    if path == &old {
                        *path = new.clone();
                    }
                }
            }
        }
    }
}

/// The FUSE filesystem: a store, plus an update layer when writable. With no
/// layer every mutating request fails. The overlay mutex serialises all
/// reads and writes that touch the layer.
pub struct StoreFs {
    reader: Reader,
    overlay: Option<Mutex<Overlay>>,
    nodes: Mutex<Nodes>,
    writes: Option<Arc<WriteControl>>,
}

// Shared between the filesystem and compaction. Every mutating handler holds
// the read side of `gate` while it works and adds one to `generation` when it
// finishes. Compaction takes the write side to stop mutations.
#[derive(Default)]
struct WriteControl {
    gate: RwLock<()>,
    generation: AtomicU64,
}

// Held by a handler for the length of one mutation.
struct Mutation<'a> {
    _gate: RwLockReadGuard<'a, ()>,
    generation: &'a AtomicU64,
}

impl Mutation<'_> {
    // Marks the point where a handler finished its change.
    fn committed(self) {}
}

// The counter moves whenever a handler held the guard, whether or not it
// succeeded. A write that fails partway has already changed bytes, and a
// compaction that missed it would publish a store without them. The cost is
// a compaction asked to retry after a failed request that changed nothing.
impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Release);
    }
}

/// Handle on a writable mount's mutation counter and write gate.
#[derive(Clone)]
pub(crate) struct WriteController(Arc<WriteControl>);

/// Proof that no mutation is in flight. While it lives, every mutating
/// request on the mount fails with `EBUSY`. Reads are still served.
pub(crate) struct FrozenWrites<'a> {
    _gate: RwLockWriteGuard<'a, ()>,
    generation: u64,
}

impl FrozenWrites<'_> {
    /// The mutation count at the moment writes stopped.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl WriteController {
    /// Number of mutations that have succeeded since the mount started.
    pub fn generation(&self) -> u64 {
        self.0.generation.load(Ordering::Acquire)
    }

    /// Waits for mutations in flight to finish, then blocks new ones. A
    /// caller compares `generation` with a value read earlier to learn
    /// whether anything was written in between.
    pub fn freeze(&self) -> Result<FrozenWrites<'_>> {
        let gate = self
            .0
            .gate
            .write()
            .map_err(|_| anyhow::anyhow!("Write gate poisoned"))?;
        Ok(FrozenWrites {
            generation: self.generation(),
            _gate: gate,
        })
    }
}

/// A mounted store served by a background thread.
pub struct Session {
    inner: fuser::BackgroundSession,
    writes: Option<WriteController>,
}

impl Session {
    /// True once the serving thread has exited.
    pub fn is_finished(&self) -> bool {
        self.inner.guard.is_finished()
    }

    /// Unmounts and waits for the serving thread.
    pub fn umount_and_join(self) -> std::io::Result<()> {
        self.inner.umount_and_join()
    }

    /// The write controller of a writable mount. `None` when read-only.
    pub(crate) fn writes(&self) -> Option<WriteController> {
        self.writes.clone()
    }
}

impl StoreFs {
    fn open(
        store: &Path,
        writes: Option<&Path>,
        control: Option<Arc<WriteControl>>,
    ) -> Result<Self> {
        let reader = Reader::open(store)?;
        let nodes = Mutex::new(Nodes::new(reader.entries()));
        let overlay = writes.map(Overlay::open).transpose()?.map(Mutex::new);
        Ok(Self {
            reader,
            overlay,
            nodes,
            writes: control,
        })
    }

    // Every handler that changes the layer calls this first and calls
    // `committed` on the result once the change is made. It never waits:
    // while writes are frozen it fails with EBUSY.
    fn mutation(&self) -> Result<Mutation<'_>> {
        let control = self.writes.as_ref().context("Read-only store")?;
        let gate = match control.gate.try_read() {
            Ok(gate) => gate,
            Err(TryLockError::WouldBlock) => {
                return Err(std::io::Error::from_raw_os_error(libc::EBUSY).into());
            }
            Err(TryLockError::Poisoned(_)) => anyhow::bail!("Write gate poisoned"),
        };
        Ok(Mutation {
            _gate: gate,
            generation: &control.generation,
        })
    }

    // Resolves an inode to its first visible path, or to its first path when
    // none is visible. For hard links that is the target while it exists, so
    // a write through any alias copies up the target and its aliases together.
    fn path(&self, inode: INodeNo) -> Result<PathBuf> {
        let paths = self
            .nodes
            .lock()
            .map_err(|_| anyhow::anyhow!("Node lock poisoned"))?
            .inodes
            .get(&u64::from(inode))
            .cloned()
            .context("Missing inode")?;
        for path in &paths {
            if self.visible(path)? {
                return Ok(path.clone());
            }
        }
        paths.into_iter().next().context("Missing inode path")
    }

    fn inode(&self, path: &Path) -> Result<INodeNo> {
        Ok(INodeNo(
            self.nodes
                .lock()
                .map_err(|_| anyhow::anyhow!("Node lock poisoned"))?
                .inode(path),
        ))
    }

    fn visible(&self, path: &Path) -> Result<bool> {
        match &self.overlay {
            Some(overlay) => Ok(overlay
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .visible(&self.reader, path)),
            None => Ok(self.reader.entry(path).is_some()),
        }
    }

    // Stat of the upper entry for `path`. `None` when the mount is read-only,
    // the path is hidden, or nothing has been copied up or created there.
    fn upper_metadata(&self, path: &Path) -> Result<Option<std::fs::Metadata>> {
        let Some(overlay) = &self.overlay else {
            return Ok(None);
        };
        let overlay = overlay
            .lock()
            .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?;
        if overlay.hidden(path) {
            return Ok(None);
        }
        match std::fs::symlink_metadata(overlay.checked_upper(path)?) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    // Attributes come from the upper entry when there is one, otherwise from
    // the store entry. Store entries are reported as owned by the mounting
    // user, and ctime is reported as mtime in both cases.
    fn attr_path(&self, path: &Path) -> Result<FileAttr> {
        ensure!(self.visible(path)?, "Missing path");
        let inode = self.inode(path)?;
        if let Some(metadata) = self.upper_metadata(path)? {
            let kind = if metadata.is_dir() {
                FileType::Directory
            } else if metadata.is_symlink() {
                FileType::Symlink
            } else {
                FileType::RegularFile
            };
            let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            return Ok(FileAttr {
                ino: inode,
                size: metadata.size(),
                blocks: metadata.blocks(),
                atime: metadata.accessed().unwrap_or(mtime),
                mtime,
                ctime: mtime,
                crtime: mtime,
                kind,
                perm: (metadata.mode() & 0o777) as u16,
                nlink: metadata.nlink() as u32,
                uid: metadata.uid(),
                gid: metadata.gid(),
                rdev: metadata.rdev() as u32,
                blksize: metadata.blksize() as u32,
                flags: 0,
            });
        }
        let entry = self.reader.entry(path).context("Missing base entry")?;
        let size = match &entry.kind {
            Kind::File { size, .. } | Kind::SlicedFile { size, .. } => *size,
            Kind::Symlink { target } => target.as_os_str().as_encoded_bytes().len() as u64,
            Kind::Directory => 0,
        };
        let time = super::restore::modified(entry.modified_secs, entry.modified_nanos)?;
        let link_count = self
            .nodes
            .lock()
            .map_err(|_| anyhow::anyhow!("Node lock poisoned"))?
            .link_count(u64::from(inode));
        Ok(FileAttr {
            ino: inode,
            size,
            blocks: size.div_ceil(512),
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind: kind(entry),
            perm: entry.mode as u16,
            nlink: if matches!(entry.kind, Kind::Directory) {
                2
            } else {
                link_count
            },
            uid: nix::unistd::geteuid().as_raw(),
            gid: nix::unistd::getegid().as_raw(),
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }

    // Path of `name` under a parent inode. Refuses `.`, `..` and any name
    // with more than one component.
    fn child(&self, parent: INodeNo, name: &OsStr) -> Result<PathBuf> {
        ensure!(
            name != "." && name != ".." && Path::new(name).components().count() == 1,
            "Invalid filename"
        );
        Ok(self.path(parent)?.join(name))
    }

    // An I/O error keeps its own errno. Every other failure, including the
    // `ensure!` checks in the overlay, is reported as EIO.
    fn errno(error: &anyhow::Error) -> Errno {
        error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .map(Errno::from_i32)
            .unwrap_or(Errno::EIO)
    }

    // Shared body of unlink and rmdir. The inode table is left alone.
    fn remove(&self, parent: INodeNo, name: &OsStr, directory: bool, reply: fuser::ReplyEmpty) {
        let result = self.child(parent, name).and_then(|path| {
            let mutation = self.mutation()?;
            self.overlay
                .as_ref()
                .context("Read-only store")?
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .remove(&self.reader, &path, directory)?;
            mutation.committed();
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }
}

fn kind(entry: &Entry) -> FileType {
    match entry.kind {
        Kind::Directory => FileType::Directory,
        Kind::File { .. } | Kind::SlicedFile { .. } => FileType::RegularFile,
        Kind::Symlink { .. } => FileType::Symlink,
    }
}

// Handler rules. A handler that changes the layer takes `mutation()` before
// touching it and calls `committed()` only after the change succeeded. It
// never writes to the store. File handles carry no state: each request
// resolves its inode to a path again and reopens the upper file.
impl Filesystem for StoreFs {
    fn lookup(&self, _: &fuser::Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEntry) {
        let result = self
            .child(parent, name)
            .and_then(|path| self.attr_path(&path));
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, fuser::Generation(0)),
            Err(_) => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        _: Option<fuser::FileHandle>,
        reply: fuser::ReplyAttr,
    ) {
        match self.path(ino).and_then(|path| self.attr_path(&path)) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(_) => reply.error(Errno::ENOENT),
        }
    }

    fn setxattr(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: fuser::ReplyEmpty,
    ) {
        let result = (|| -> Result<()> {
            ensure!(
                position == 0,
                "Extended attribute positions are unsupported"
            );
            ensure!(
                flags & !(libc::XATTR_CREATE | libc::XATTR_REPLACE) == 0,
                "Invalid extended attribute flags"
            );
            let path = self.path(ino)?;
            let mutation = self.mutation()?;
            let upper = self
                .overlay
                .as_ref()
                .context("Read-only store")?
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .copy_up(&self.reader, &path)?;
            let exists = xattr::get(&upper, name)?.is_some();
            if flags & libc::XATTR_CREATE != 0 && exists {
                return Err(std::io::Error::from_raw_os_error(libc::EEXIST).into());
            }
            if flags & libc::XATTR_REPLACE != 0 && !exists {
                return Err(std::io::Error::from_raw_os_error(libc::ENODATA).into());
            }
            xattr::set(upper, name, value)?;
            mutation.committed();
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(if self.overlay.is_none() {
                Errno::EROFS
            } else {
                Self::errno(&error)
            }),
        }
    }

    fn getxattr(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        let result = (|| -> Result<Vec<u8>> {
            let path = self.path(ino)?;
            if self.upper_metadata(&path)?.is_some() {
                return xattr::get(
                    self.overlay
                        .as_ref()
                        .context("Missing update layer")?
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                        .checked_upper(&path)?,
                    name,
                )?
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENODATA).into());
            }
            self.reader
                .entry(&path)
                .context("Missing base entry")?
                .xattrs
                .iter()
                .find(|attribute| attribute.name.as_slice() == name.as_bytes())
                .map(|attribute| attribute.value.clone())
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENODATA).into())
        })();
        match result {
            Ok(value) if size == 0 => match u32::try_from(value.len()) {
                Ok(length) => reply.size(length),
                Err(_) => reply.error(Errno::EOVERFLOW),
            },
            Ok(value) if value.len() <= size as usize => reply.data(&value),
            Ok(_) => reply.error(Errno::ERANGE),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }

    fn listxattr(&self, _: &fuser::Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let result = (|| -> Result<Vec<u8>> {
            let path = self.path(ino)?;
            let mut names = Vec::new();
            if self.upper_metadata(&path)?.is_some() {
                let upper = self
                    .overlay
                    .as_ref()
                    .context("Missing update layer")?
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                    .checked_upper(&path)?;
                for name in xattr::list(upper)? {
                    names.extend_from_slice(name.as_bytes());
                    names.push(0);
                }
            } else {
                for attribute in &self
                    .reader
                    .entry(&path)
                    .context("Missing base entry")?
                    .xattrs
                {
                    names.extend_from_slice(&attribute.name);
                    names.push(0);
                }
            }
            Ok(names)
        })();
        match result {
            Ok(names) if size == 0 => match u32::try_from(names.len()) {
                Ok(length) => reply.size(length),
                Err(_) => reply.error(Errno::EOVERFLOW),
            },
            Ok(names) if names.len() <= size as usize => reply.data(&names),
            Ok(_) => reply.error(Errno::ERANGE),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }

    fn removexattr(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        name: &OsStr,
        reply: fuser::ReplyEmpty,
    ) {
        let result = (|| -> Result<()> {
            let path = self.path(ino)?;
            let mutation = self.mutation()?;
            let upper = self
                .overlay
                .as_ref()
                .context("Read-only store")?
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .copy_up(&self.reader, &path)?;
            xattr::remove(upper, name)?;
            mutation.committed();
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(if self.overlay.is_none() {
                Errno::EROFS
            } else {
                Self::errno(&error)
            }),
        }
    }

    // Opening for write or with O_TRUNC copies the file up at once, before
    // any byte is written. A read-only open touches nothing.
    fn open(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        flags: fuser::OpenFlags,
        reply: fuser::ReplyOpen,
    ) {
        let result = (|| -> Result<()> {
            let path = self.path(ino)?;
            ensure!(
                matches!(self.attr_path(&path)?.kind, FileType::RegularFile),
                "Not a file"
            );
            if flags.acc_mode() != fuser::OpenAccMode::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
                let mutation = self.mutation()?;
                let overlay = self.overlay.as_ref().context("Read-only store")?;
                let mut overlay = overlay
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?;
                let upper = overlay.copy_up(&self.reader, &path)?;
                if flags.0 & libc::O_TRUNC != 0 {
                    OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(upper)?
                        .sync_all()?;
                }
                mutation.committed();
            }
            Ok(())
        })();
        match result {
            Ok(()) => reply.opened(
                fuser::FileHandle(u64::from(ino)),
                fuser::FopenFlags::FOPEN_KEEP_CACHE,
            ),
            Err(error) => reply.error(if self.overlay.is_none() {
                Errno::EROFS
            } else {
                Self::errno(&error)
            }),
        }
    }

    // Takes no write gate, so reads continue while writes are frozen. Every
    // failure, a chunk that fails its hash included, is logged and
    // returned as EIO.
    fn read(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        _: fuser::FileHandle,
        offset: u64,
        size: u32,
        _: fuser::OpenFlags,
        _: Option<fuser::LockOwner>,
        reply: fuser::ReplyData,
    ) {
        let result = self.path(ino).and_then(|path| match &self.overlay {
            Some(overlay) => overlay
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .read(&self.reader, &path, offset, size as usize),
            None => self.reader.read(&path, offset, size as usize),
        });
        match result {
            Ok(bytes) => reply.data(&bytes),
            Err(error) => {
                tracing::error!(%error,"store read failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn write(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        _: fuser::FileHandle,
        offset: u64,
        data: &[u8],
        _: fuser::WriteFlags,
        _: fuser::OpenFlags,
        _: Option<fuser::LockOwner>,
        reply: fuser::ReplyWrite,
    ) {
        let result = (|| -> Result<usize> {
            let mutation = self.mutation()?;
            let path = self.path(ino)?;
            let overlay = self.overlay.as_ref().context("Read-only store")?;
            let mut overlay = overlay
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?;
            let upper = overlay.copy_up(&self.reader, &path)?;
            let mut file = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(upper)?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(data)?;
            mutation.committed();
            Ok(data.len())
        })();
        match result {
            Ok(count) => reply.written(count as u32),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }

    // Syncs the upper copy if there is one. A file still served from the
    // store has nothing to flush. `fsync` shares this body.
    fn flush(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        _: fuser::FileHandle,
        _: fuser::LockOwner,
        reply: fuser::ReplyEmpty,
    ) {
        let result = self
            .path(ino)
            .and_then(|path| self.upper_metadata(&path).map(|metadata| (path, metadata)))
            .and_then(|(path, metadata)| {
                if metadata.is_some()
                    && let Some(overlay) = &self.overlay
                {
                    let upper = overlay
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                        .checked_upper(&path)?;
                    OpenOptions::new().read(true).open(upper)?.sync_all()?;
                }
                Ok(())
            });
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }

    fn fsync(
        &self,
        request: &fuser::Request,
        ino: INodeNo,
        handle: fuser::FileHandle,
        _: bool,
        reply: fuser::ReplyEmpty,
    ) {
        self.flush(request, ino, handle, fuser::LockOwner(0), reply);
    }

    // Applies size, mode and mtime to the upper copy. Owner, group and atime
    // requests are accepted and ignored. Any call copies the entry up and
    // counts as a mutation, even one that changes nothing.
    fn setattr(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _: Option<SystemTime>,
        _: Option<fuser::FileHandle>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        _: Option<fuser::BsdFileFlags>,
        reply: fuser::ReplyAttr,
    ) {
        let result = (|| -> Result<FileAttr> {
            let mutation = self.mutation()?;
            let path = self.path(ino)?;
            let overlay = self.overlay.as_ref().context("Read-only store")?;
            let mut overlay = overlay
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?;
            let upper = overlay.copy_up(&self.reader, &path)?;
            if let Some(size) = size {
                OpenOptions::new().write(true).open(&upper)?.set_len(size)?;
            }
            if let Some(mode) = mode {
                std::fs::set_permissions(&upper, Permissions::from_mode(mode & 0o777))?;
            }
            if let Some(mtime) = mtime {
                let time = match mtime {
                    fuser::TimeOrNow::SpecificTime(time) => time,
                    fuser::TimeOrNow::Now => SystemTime::now(),
                };
                OpenOptions::new()
                    .read(true)
                    .open(&upper)?
                    .set_times(FileTimes::new().set_modified(time))?;
            }
            drop(overlay);
            let attr = self.attr_path(&path)?;
            mutation.committed();
            Ok(attr)
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }

    fn create(
        &self,
        _: &fuser::Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let result = (|| -> Result<FileAttr> {
            let mutation = self.mutation()?;
            let path = self.child(parent, name)?;
            let overlay = self.overlay.as_ref().context("Read-only store")?;
            overlay
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .create_file(&self.reader, &path, mode & !umask)?
                .sync_all()?;
            let attr = self.attr_path(&path)?;
            mutation.committed();
            Ok(attr)
        })();
        match result {
            Ok(attr) => reply.created(
                &TTL,
                &attr,
                fuser::Generation(0),
                fuser::FileHandle(u64::from(attr.ino)),
                fuser::FopenFlags::empty(),
            ),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }

    fn mkdir(
        &self,
        _: &fuser::Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: fuser::ReplyEntry,
    ) {
        let result = (|| -> Result<FileAttr> {
            let mutation = self.mutation()?;
            let path = self.child(parent, name)?;
            self.overlay
                .as_ref()
                .context("Read-only store")?
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .mkdir(&self.reader, &path, mode & !umask)?;
            let attr = self.attr_path(&path)?;
            mutation.committed();
            Ok(attr)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, fuser::Generation(0)),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }

    fn symlink(
        &self,
        _: &fuser::Request,
        parent: INodeNo,
        name: &OsStr,
        target: &Path,
        reply: fuser::ReplyEntry,
    ) {
        let result = (|| -> Result<FileAttr> {
            let mutation = self.mutation()?;
            let path = self.child(parent, name)?;
            self.overlay
                .as_ref()
                .context("Read-only store")?
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .symlink(&self.reader, &path, target)?;
            let attr = self.attr_path(&path)?;
            mutation.committed();
            Ok(attr)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, fuser::Generation(0)),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }

    fn unlink(&self, _: &fuser::Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
        self.remove(parent, name, false, reply);
    }
    fn rmdir(&self, _: &fuser::Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
        self.remove(parent, name, true, reply);
    }

    fn rename(
        &self,
        _: &fuser::Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: fuser::RenameFlags,
        reply: fuser::ReplyEmpty,
    ) {
        let result = (|| -> Result<()> {
            let mutation = self.mutation()?;
            ensure!(
                !flags.contains(
                    fuser::RenameFlags::RENAME_EXCHANGE | fuser::RenameFlags::RENAME_WHITEOUT
                ),
                "Unsupported rename flags"
            );
            let from = self.child(parent, name)?;
            let to = self.child(newparent, newname)?;
            // The layer is renamed first. The inode table follows only on
            // success, so the kernel's inodes keep naming the moved files.
            self.overlay
                .as_ref()
                .context("Read-only store")?
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .rename(
                    &self.reader,
                    &from,
                    &to,
                    flags.contains(fuser::RenameFlags::RENAME_NOREPLACE),
                )?;
            self.nodes
                .lock()
                .map_err(|_| anyhow::anyhow!("Node lock poisoned"))?
                .rename(&from, &to);
            mutation.committed();
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }

    fn link(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: fuser::ReplyEntry,
    ) {
        let result = (|| -> Result<FileAttr> {
            let mutation = self.mutation()?;
            let from = self.path(ino)?;
            let to = self.child(newparent, newname)?;
            self.overlay
                .as_ref()
                .context("Read-only store")?
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .link(&self.reader, &from, &to)?;
            self.nodes
                .lock()
                .map_err(|_| anyhow::anyhow!("Node lock poisoned"))?
                .alias(&to, u64::from(ino));
            let attr = self.attr_path(&to)?;
            mutation.committed();
            Ok(attr)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, fuser::Generation(0)),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }

    fn readlink(&self, _: &fuser::Request, ino: INodeNo, reply: fuser::ReplyData) {
        let result = (|| -> Result<PathBuf> {
            let path = self.path(ino)?;
            if let Some(overlay) = &self.overlay {
                let overlay = overlay
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?;
                let upper = overlay.checked_upper(&path)?;
                if std::fs::symlink_metadata(&upper).is_ok() {
                    return Ok(std::fs::read_link(upper)?);
                }
            }
            match &self.reader.entry(&path).context("Missing link")?.kind {
                Kind::Symlink { target } => Ok(target.clone()),
                _ => anyhow::bail!("Not a link"),
            }
        })();
        match result {
            Ok(target) => reply.data(target.as_os_str().as_encoded_bytes()),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }

    // `offset` is a position in the row list, which is rebuilt on every call:
    // `.`, `..`, then the visible children. A listing that changes between
    // two calls can therefore skip or repeat a name.
    fn readdir(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        _: fuser::FileHandle,
        offset: u64,
        mut reply: fuser::ReplyDirectory,
    ) {
        let result = (|| -> Result<Vec<PathBuf>> {
            let path = self.path(ino)?;
            ensure!(
                matches!(self.attr_path(&path)?.kind, FileType::Directory),
                "Not a directory"
            );
            match &self.overlay {
                Some(overlay) => overlay
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                    .children(&self.reader, &path),
                None => Ok(self
                    .reader
                    .entries()
                    .iter()
                    .filter(|e| e.path.parent() == Some(path.as_path()))
                    .map(|e| e.path.clone())
                    .collect()),
            }
        })();
        let Ok(children) = result else {
            reply.error(Errno::ENOTDIR);
            return;
        };
        let Ok(path) = self.path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let parent = path
            .parent()
            .and_then(|p| self.inode(p).ok())
            .unwrap_or(INodeNo::ROOT);
        let mut rows = vec![
            (ino, FileType::Directory, ".".into()),
            (parent, FileType::Directory, "..".into()),
        ];
        for child in children {
            if let Ok(attr) = self.attr_path(&child)
                && let Some(name) = child.file_name()
            {
                rows.push((attr.ino, attr.kind, name.to_os_string()));
            }
        }
        let Ok(skip) = usize::try_from(offset) else {
            reply.error(Errno::EINVAL);
            return;
        };
        for (index, (inode, kind, name)) in rows.into_iter().enumerate().skip(skip) {
            if reply.add(inode, index as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    // A writable mount reports the filesystem holding the update layer, since
    // that is where new bytes land. A read-only mount reports no free space.
    fn statfs(&self, _: &fuser::Request, _: INodeNo, reply: fuser::ReplyStatfs) {
        if let Some(overlay) = &self.overlay
            && let Ok(overlay) = overlay.lock()
            && let Ok(stat) = nix::sys::statvfs::statvfs(overlay.root())
        {
            reply.statfs(
                stat.blocks(),
                stat.blocks_free(),
                stat.blocks_available(),
                stat.files(),
                stat.files_free(),
                stat.block_size() as u32,
                stat.name_max() as u32,
                stat.fragment_size() as u32,
            );
            return;
        }
        reply.statfs(
            self.reader.summary().archive_bytes.div_ceil(4096),
            0,
            0,
            self.reader.entries().len() as u64,
            0,
            4096,
            255,
            4096,
        );
    }
}

/// Mounts over an empty directory. `writes` enables a persistent update layer.
pub fn mount(store: &Path, target: &Path, writes: Option<&Path>) -> Result<Session> {
    ensure!(
        Path::new("/dev/fuse").exists(),
        "FUSE is unavailable: enable /dev/fuse before mounting a store"
    );
    let target = target.canonicalize()?;
    ensure!(
        target.is_dir() && std::fs::read_dir(&target)?.next().is_none(),
        "Choose an empty mount folder"
    );
    let control = writes.map(|_| Arc::new(WriteControl::default()));
    let fs = StoreFs::open(store, writes, control.clone())?;
    // No allow_other, so only the mounting user can reach the files. With
    // default_permissions the kernel enforces the reported mode bits. The
    // filesystem name is how `clear_disconnected_mount` recognises a mount
    // left behind by this tool.
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::NoSuid,
        fuser::MountOption::NoDev,
        fuser::MountOption::DefaultPermissions,
        fuser::MountOption::FSName("flummox-pack".into()),
    ];
    if writes.is_none() {
        config.mount_options.push(fuser::MountOption::RO);
    }
    let inner = fuser::spawn_mount(fs, target, &config).context("Mounting the store failed")?;
    Ok(Session {
        inner,
        writes: control.map(WriteController),
    })
}
