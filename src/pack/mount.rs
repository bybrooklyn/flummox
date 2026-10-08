//! Optional owner-only view of a store, with a persistent copy-on-write layer.

use super::{Entry, Kind, Reader, overlay::Overlay};
use anyhow::{Context, Result, ensure};
use fuser::{Errno, FileAttr, FileType, Filesystem, INodeNo, ReplyXattr};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{File, FileTimes, OpenOptions, Permissions},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
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
// Removing a name drops it from the table, so a file created at that path
// later gets a number of its own.
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

    // Drops a removed name. Other names of the same inode stay.
    fn forget(&mut self, path: &Path) {
        if let Some(inode) = self.paths.remove(path)
            && let Some(paths) = self.inodes.get_mut(&inode)
        {
            paths.retain(|name| name != path);
        }
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
                // Joining an empty suffix would leave a trailing slash, which
                // no file path resolves through.
                let moved = if suffix.as_os_str().is_empty() {
                    to.to_path_buf()
                } else {
                    to.join(suffix)
                };
                Some((path.clone(), moved, *inode))
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
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    next_handle: AtomicU64,
}

// State of one open file. `file` is the upper file the handle was opened on,
// so reads and writes keep reaching that inode after its name is removed or
// replaced. `base` is the store path of a handle opened on a store file that
// had no upper copy, which stays readable after its name is gone.
struct Handle {
    file: Option<File>,
    writable: bool,
    base: Option<PathBuf>,
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
            handles: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
        })
    }

    fn add_handle(&self, handle: Handle) -> Result<fuser::FileHandle> {
        let number = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.handles
            .lock()
            .map_err(|_| anyhow::anyhow!("Handle lock poisoned"))?
            .insert(number, Arc::new(handle));
        Ok(fuser::FileHandle(number))
    }

    fn handle(&self, handle: fuser::FileHandle) -> Option<Arc<Handle>> {
        self.handles.lock().ok()?.get(&handle.0).cloned()
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

    // Shared body of unlink and rmdir.
    fn remove(&self, parent: INodeNo, name: &OsStr, directory: bool, reply: fuser::ReplyEmpty) {
        let result = self.child(parent, name).and_then(|path| {
            let mutation = self.mutation()?;
            self.overlay
                .as_ref()
                .context("Read-only store")?
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .remove(&self.reader, &path, directory)?;
            self.nodes
                .lock()
                .map_err(|_| anyhow::anyhow!("Node lock poisoned"))?
                .forget(&path);
            mutation.committed();
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(Self::errno(&error)),
        }
    }
}

// Opens an upper file with the access the caller asked for, never following
// a link. A mode that forbids an access is the kernel's to refuse.
fn open_upper(upper: &Path, access: fuser::OpenAccMode) -> std::io::Result<File> {
    OpenOptions::new()
        .read(access != fuser::OpenAccMode::O_WRONLY)
        .write(access != fuser::OpenAccMode::O_RDONLY)
        .custom_flags(libc::O_NOFOLLOW)
        .open(upper)
}

// A time as seconds and nanoseconds from the epoch, counting forward from a
// whole second when the time is before it.
fn timespec(time: SystemTime) -> rustix::fs::Timespec {
    let (seconds, nanos) = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => (
            i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
            i64::from(after.subsec_nanos()),
        ),
        Err(error) => {
            let before = error.duration();
            let back = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            match i64::from(before.subsec_nanos()) {
                0 => (back.saturating_neg(), 0),
                nanos => (back.saturating_add(1).saturating_neg(), 1_000_000_000 - nanos),
            }
        }
    };
    rustix::fs::Timespec {
        tv_sec: seconds,
        tv_nsec: nanos,
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
// never writes to the store. A handle opened on an upper file holds that file
// open. Any other request resolves its inode to a path again.
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
    // any byte is written. A file with an upper copy is held open from here
    // on. A read-only open of a store file touches nothing.
    fn open(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        flags: fuser::OpenFlags,
        reply: fuser::ReplyOpen,
    ) {
        let result = (|| -> Result<fuser::FileHandle> {
            let path = self.path(ino)?;
            ensure!(
                matches!(self.attr_path(&path)?.kind, FileType::RegularFile),
                "Not a file"
            );
            let access = flags.acc_mode();
            let writable = access != fuser::OpenAccMode::O_RDONLY;
            let mut file = None;
            if writable || flags.0 & libc::O_TRUNC != 0 {
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
                        .open(&upper)?
                        .sync_all()?;
                }
                file = Some(open_upper(&upper, access)?);
                mutation.committed();
            } else if self.upper_metadata(&path)?.is_some()
                && let Some(overlay) = &self.overlay
            {
                let upper = overlay
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                    .checked_upper(&path)?;
                file = Some(open_upper(&upper, access)?);
            }
            self.add_handle(Handle {
                file,
                writable,
                base: self.reader.entry(&path).map(|_| path.clone()),
            })
        })();
        match result {
            Ok(handle) => reply.opened(handle, fuser::FopenFlags::FOPEN_KEEP_CACHE),
            Err(error) => reply.error(if self.overlay.is_none() {
                Errno::EROFS
            } else {
                Self::errno(&error)
            }),
        }
    }

    fn release(
        &self,
        _: &fuser::Request,
        _: INodeNo,
        fh: fuser::FileHandle,
        _: fuser::OpenFlags,
        _: Option<fuser::LockOwner>,
        _: bool,
        reply: fuser::ReplyEmpty,
    ) {
        if let Ok(mut handles) = self.handles.lock() {
            handles.remove(&fh.0);
        }
        reply.ok();
    }

    // Takes no write gate, so reads continue while writes are frozen. Every
    // failure, a chunk that fails its hash included, is logged and
    // returned as EIO. A handle on an upper file reads that file. A handle on
    // a store file whose name is gone reads the store entry it opened.
    fn read(
        &self,
        _: &fuser::Request,
        ino: INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _: fuser::OpenFlags,
        _: Option<fuser::LockOwner>,
        reply: fuser::ReplyData,
    ) {
        let handle = self.handle(fh);
        let result = (|| -> Result<Vec<u8>> {
            if let Some(file) = handle.as_ref().and_then(|handle| handle.file.as_ref()) {
                let mut bytes = vec![0; size as usize];
                let count = file.read_at(&mut bytes, offset)?;
                bytes.truncate(count);
                return Ok(bytes);
            }
            let current = self
                .path(ino)
                .ok()
                .filter(|path| self.visible(path).unwrap_or(false));
            match (current, handle.as_ref().and_then(|handle| handle.base.as_ref())) {
                (Some(path), _) => match &self.overlay {
                    Some(overlay) => overlay
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                        .read(&self.reader, &path, offset, size as usize),
                    None => self.reader.read(&path, offset, size as usize),
                },
                (None, Some(base)) => self.reader.read(base, offset, size as usize),
                (None, None) => anyhow::bail!("File was deleted"),
            }
        })();
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
        fh: fuser::FileHandle,
        offset: u64,
        data: &[u8],
        _: fuser::WriteFlags,
        _: fuser::OpenFlags,
        _: Option<fuser::LockOwner>,
        reply: fuser::ReplyWrite,
    ) {
        let result = (|| -> Result<usize> {
            let mutation = self.mutation()?;
            let handle = self.handle(fh);
            if let Some(file) = handle.as_ref().and_then(|handle| handle.file.as_ref()) {
                file.write_all_at(data, offset)?;
            } else {
                let path = self.path(ino)?;
                let overlay = self.overlay.as_ref().context("Read-only store")?;
                let mut overlay = overlay
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?;
                let upper = overlay.copy_up(&self.reader, &path)?;
                OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(upper)?
                    .write_all_at(data, offset)?;
            }
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
        fh: fuser::FileHandle,
        _: fuser::LockOwner,
        reply: fuser::ReplyEmpty,
    ) {
        if let Some(handle) = self.handle(fh)
            && let Some(file) = &handle.file
        {
            match file.sync_all() {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(Errno::from_i32(error.raw_os_error().unwrap_or(libc::EIO))),
            }
            return;
        }
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
        fh: Option<fuser::FileHandle>,
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
            let link = std::fs::symlink_metadata(&upper)?.is_symlink();
            if let Some(size) = size {
                match fh.and_then(|fh| self.handle(fh)) {
                    Some(handle) if handle.writable && handle.file.is_some() => {
                        if let Some(file) = &handle.file {
                            file.set_len(size)?;
                        }
                    }
                    _ => OpenOptions::new().write(true).open(&upper)?.set_len(size)?,
                }
            }
            if let Some(mode) = mode
                && !link
            {
                std::fs::set_permissions(&upper, Permissions::from_mode(mode & 0o777))?;
            }
            if let Some(mtime) = mtime {
                let time = match mtime {
                    fuser::TimeOrNow::SpecificTime(time) => time,
                    fuser::TimeOrNow::Now => SystemTime::now(),
                };
                if link {
                    // Opening a link follows it, which would date its target.
                    rustix::fs::utimensat(
                        rustix::fs::CWD,
                        &upper,
                        &rustix::fs::Timestamps {
                            last_access: rustix::fs::Timespec {
                                tv_sec: 0,
                                tv_nsec: rustix::fs::UTIME_OMIT,
                            },
                            last_modification: timespec(time),
                        },
                        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                    )
                    .map_err(std::io::Error::from)?;
                } else {
                    OpenOptions::new()
                        .read(true)
                        .open(&upper)?
                        .set_times(FileTimes::new().set_modified(time))?;
                }
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
        let result = (|| -> Result<(FileAttr, fuser::FileHandle)> {
            let mutation = self.mutation()?;
            let path = self.child(parent, name)?;
            let overlay = self.overlay.as_ref().context("Read-only store")?;
            let file = overlay
                .lock()
                .map_err(|_| anyhow::anyhow!("Update lock poisoned"))?
                .create_file(&self.reader, &path, mode & !umask)?;
            file.sync_all()?;
            let attr = self.attr_path(&path)?;
            let handle = self.add_handle(Handle {
                file: Some(file),
                writable: true,
                base: None,
            })?;
            mutation.committed();
            Ok((attr, handle))
        })();
        match result {
            Ok((attr, handle)) => reply.created(
                &TTL,
                &attr,
                fuser::Generation(0),
                handle,
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
            if flags.intersects(
                fuser::RenameFlags::RENAME_EXCHANGE | fuser::RenameFlags::RENAME_WHITEOUT,
            ) {
                return Err(std::io::Error::from_raw_os_error(libc::EINVAL).into());
            }
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
    if let Some(writes) = writes {
        // A layer under the mount point would be covered by the mount, and
        // every request to it would wait on the thread serving the mount.
        let layer = canonical_new(writes)?;
        ensure!(
            !layer.starts_with(&target) && !target.starts_with(&layer),
            "Keep the update layer outside the mount folder"
        );
    }
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

// Canonical form of a path whose last component may not exist yet.
fn canonical_new(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let name = path.file_name().context("The update layer has no name")?;
            Ok(parent.canonicalize()?.join(name))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check_eq};

    fn names(nodes: &Nodes, inode: u64) -> Vec<std::ffi::OsString> {
        nodes
            .inodes
            .get(&inode)
            .map(|paths| paths.iter().map(|p| p.as_os_str().to_os_string()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_renamed_file_is_known_by_exactly_its_new_name() -> TestResult {
        let mut nodes = Nodes::new(&[]);
        let moved = nodes.inode(Path::new("a"));
        let child = nodes.inode(Path::new("d/x"));
        nodes.rename(Path::new("a"), Path::new("b"));
        check_eq(
            names(&nodes, moved),
            vec![std::ffi::OsString::from("b")],
            "a file keeps no trailing separator",
        )?;
        nodes.rename(Path::new("d"), Path::new("e"));
        check_eq(
            names(&nodes, child),
            vec![std::ffi::OsString::from("e/x")],
            "a path under a moved folder follows it",
        )
    }

    #[test]
    fn a_removed_name_is_not_handed_to_the_next_file_created_there() -> TestResult {
        let mut nodes = Nodes::new(&[]);
        let first = nodes.inode(Path::new("f.tmp"));
        nodes.alias(Path::new("f"), first);
        nodes.forget(Path::new("f.tmp"));
        check_eq(nodes.link_count(first), 1, "only the other name counts")?;
        let second = nodes.inode(Path::new("f.tmp"));
        check_eq(
            second != first,
            true,
            "the new file at the reused name has its own number",
        )?;
        check_eq(
            names(&nodes, first),
            vec![std::ffi::OsString::from("f")],
            "the surviving name stays with the first inode",
        )
        .ctx("names")
    }
}
