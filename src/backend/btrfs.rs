//! The btrfs backend: transparent compression through the kernel's defrag
//! ioctl.
//!
//! This is the Linux counterpart of what the Windows original does with NTFS
//! LZX. Files are rewritten in place as compressed extents, so games keep
//! working with no mount, no daemon and no change to how Steam sees them.
//!
//! Three kernel interfaces are used:
//! - `BTRFS_IOC_DEFRAG_RANGE` with `COMPRESS|COMPRESS_LEVEL` to rewrite a file
//!   compressed, and with `NOCOMPRESS` to undo it.
//! - the `btrfs.compression` xattr on the directory, so files Steam writes
//!   later inherit compression.
//! - `FS_IOC_FIEMAP`, whose `ENCODED` flag marks compressed extents, to report
//!   status without the privileges `compsize` needs.

// Every ioctl in the crate is here: the defrag call that compresses a file and
// the FIEMAP call that reports what is compressed.
#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::backend::{
    Backend, CompressOpts, CompressionStatus, Event, JobCtx, Outcome, free_bytes,
};
use crate::estimate::{BtrfsModel, UnitModel};
use crate::fsprobe::BackendKind;
use crate::inventory::Inventory;
use crate::safeio::Anchor;

/// zstd, as numbered by `BTRFS_COMPRESS_ZSTD`.
const BTRFS_COMPRESS_ZSTD: u8 = 3;

/// Compress while defragmenting.
const DEFRAG_RANGE_COMPRESS: u64 = 1;
/// Flush the rewritten data before returning.
const DEFRAG_RANGE_START_IO: u64 = 2;
/// `compress.level` is meaningful (kernel 6.15 and newer).
const DEFRAG_RANGE_COMPRESS_LEVEL: u64 = 4;
/// Decompress the range.
const DEFRAG_RANGE_NOCOMPRESS: u64 = 8;

/// Mirrors `struct btrfs_ioctl_defrag_range_args` from `linux/btrfs.h`.
///
/// The kernel's union of `__u32 compress_type` with `{ __u8 type; __s8 level }`
/// is spelled out as its three fields here, which has the same layout.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct DefragRangeArgs {
    start: u64,
    len: u64,
    flags: u64,
    extent_thresh: u32,
    compress_type: u8,
    compress_level: i8,
    _pad: u16,
    unused: [u32; 4],
}

nix::ioctl_write_ptr!(btrfs_defrag_range, 0x94, 16, DefragRangeArgs);

/// A compressed extent, per `FIEMAP_EXTENT_ENCODED`.
const FIEMAP_EXTENT_ENCODED: u32 = 0x0000_0008;
/// The last extent of the file.
const FIEMAP_EXTENT_LAST: u32 = 0x0000_0001;
/// Ask the kernel to flush delalloc before mapping.
const FIEMAP_FLAG_SYNC: u32 = 1;
/// How many extents to fetch per ioctl.
const FIEMAP_BATCH: u32 = 128;

/// Mirrors `struct fiemap`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Fiemap {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
}

/// Mirrors `struct fiemap_extent`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct FiemapExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

nix::ioctl_readwrite!(fs_ioc_fiemap, b'f', 11, Fiemap);

/// Rewrites an already-open file as compressed extents at `level`.
///
/// Takes a handle, not a path. The caller got it from an [`Anchor`], so the
/// path could not have been swapped for a symlink between the walk and the
/// rewrite. No path-taking version exists, because that would be a route
/// around the check.
///
/// The handle may be read-only. The kernel checks write *permission*, not the
/// open mode, so this still works on a game executable that is running.
pub fn compress_fd(file: &File, level: i32) -> io::Result<i32> {
    // Defrag must see extents for recently installed or patched dirty pages.
    file.sync_all()?;
    compress_range(file, level, 0, u64::MAX)
}

fn compress_range(file: &File, level: i32, start: u64, len: u64) -> io::Result<i32> {
    let applied = applied_level(level);
    let args = DefragRangeArgs {
        start,
        len,
        flags: DEFRAG_RANGE_COMPRESS | DEFRAG_RANGE_START_IO | DEFRAG_RANGE_COMPRESS_LEVEL,
        extent_thresh: 0,
        compress_type: BTRFS_COMPRESS_ZSTD,
        compress_level: applied as i8,
        ..DefragRangeArgs::default()
    };
    // SAFETY: `file` is open for the call and `args` is a correctly laid out
    // `btrfs_ioctl_defrag_range_args` that outlives it. A file on another
    // filesystem makes the kernel return ENOTTY.
    let result = unsafe { btrfs_defrag_range(file.as_raw_fd(), &args) };
    match result {
        Ok(_) => Ok(applied),
        // Kernels before 6.15 do not know the level flag. Retrying at the
        // filesystem's default level beats failing the job, but the caller
        // has to learn which level actually landed: recording the level that
        // was asked for tells the estimator this file is done at 15 when it
        // is compressed at the mount default, and it then refuses to offer
        // the saving that is still there.
        Err(nix::errno::Errno::EOPNOTSUPP | nix::errno::Errno::EINVAL) => {
            let fallback = DefragRangeArgs {
                flags: DEFRAG_RANGE_COMPRESS | DEFRAG_RANGE_START_IO,
                compress_level: 0,
                ..args
            };
            // SAFETY: as above.
            unsafe { btrfs_defrag_range(file.as_raw_fd(), &fallback) }
                .map(|_| DEFAULT_LEVEL)
                .map_err(errno_to_io)
        }
        Err(e) => Err(errno_to_io(e)),
    }
}

/// The level the kernel applies when we cannot ask for one.
///
/// btrfs uses zstd level 3 unless the mount says otherwise, so this is a
/// floor rather than a promise.
pub const DEFAULT_LEVEL: i32 = 3;

/// The level the kernel is sent for a request: clamped to its range, with 0
/// meaning the default.
fn applied_level(requested: i32) -> i32 {
    match requested.clamp(-15, 15) {
        0 => DEFAULT_LEVEL,
        level => level,
    }
}

/// Whether a kernel release string names a kernel that takes a zstd level in
/// the defrag ioctl (6.15 and newer).
fn release_accepts_level(release: &str) -> bool {
    let mut parts = release.split(|c: char| !c.is_ascii_digit());
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor) >= (6, 15)
}

/// The best level this kernel can apply when `wanted` is requested.
///
/// A kernel before 6.15 ignores the level and applies [`DEFAULT_LEVEL`], so a
/// receipt at that level is as good as a pass can get there. Callers that
/// compare a receipt's level with the plan's floor should compare against
/// this instead, or every pass on such a kernel rewrites the whole game.
pub fn attainable_level(wanted: i32) -> i32 {
    let wanted = applied_level(wanted);
    match nix::sys::utsname::uname() {
        Ok(name) if !release_accepts_level(&name.release().to_string_lossy()) => {
            wanted.min(DEFAULT_LEVEL)
        }
        _ => wanted,
    }
}

/// Rewrites an already-open file as uncompressed extents.
///
/// Anchored for the same reason as [`compress_fd`].
pub fn decompress_fd(file: &File) -> io::Result<()> {
    // Materialize dirty pages first. Otherwise a defrag can finish before
    // those pages become extents, then mount compression encodes them later.
    file.sync_all()?;
    decompress_range(file, 0, u64::MAX)
}

fn decompress_range(file: &File, start: u64, len: u64) -> io::Result<()> {
    let args = DefragRangeArgs {
        start,
        len,
        flags: DEFRAG_RANGE_NOCOMPRESS | DEFRAG_RANGE_START_IO,
        ..DefragRangeArgs::default()
    };
    // SAFETY: `file` is open for the call and `args` is a correctly laid out
    // `btrfs_ioctl_defrag_range_args` that outlives it. A file on another
    // filesystem makes the kernel return ENOTTY.
    match unsafe { btrfs_defrag_range(file.as_raw_fd(), &args) } {
        Ok(_) => Ok(()),
        Err(nix::errno::Errno::EOPNOTSUPP | nix::errno::Errno::EINVAL) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this kernel cannot decompress with the defrag ioctl; a newer kernel is needed",
        )),
        Err(e) => Err(errno_to_io(e)),
    }
}

/// Files this small can keep a compressed inline extent that defrag never
/// rewrites, so the check after a decompress leaves them out.
const INLINE_LIMIT: u64 = 16 * 1024;

/// Fails when a file is still stored compressed after a decompress.
///
/// A kernel that ignores the `NOCOMPRESS` flag returns success and rewrites
/// the file as it was.
fn ensure_decompressed(file: &File) -> io::Result<()> {
    if file.metadata()?.len() <= INLINE_LIMIT {
        return Ok(());
    }
    let (compressed, _) = compressed_bytes_fd(file)?;
    if compressed == 0 {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "still stored compressed after the rewrite; this kernel may not support decompressing",
    ))
}

/// Text of the error when a kernel leaves a test file compressed.
pub const PROBE_FAILED: &str = "This kernel did not decompress a test file, so it cannot undo btrfs compression in place. Update the kernel to one that supports the defrag no-compress flag. Nothing was changed.";

/// Checks that the kernel honours the decompress flag before a job changes any
/// state. Writes a small compressible file in a scratch folder on the game's
/// filesystem, beside the install or in `state_dir`, and removes it again.
/// Returns `Ok` when no scratch folder is available, since nothing was learned.
pub fn probe_decompress(install_dir: &Path, state_dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(install_dir) else {
        return Ok(());
    };
    let device = meta.dev();
    for base in install_dir.parent().into_iter().chain([state_dir]) {
        let Ok(scratch) = tempfile::Builder::new()
            .prefix(".flummox-probe-")
            .tempdir_in(base)
        else {
            continue;
        };
        if scratch.path().metadata().map(|meta| meta.dev()).ok() == Some(device) {
            return probe_decompress_in(scratch.path(), decompress_fd);
        }
    }
    Ok(())
}

/// [`probe_decompress`] in `dir`, with `decompress` standing in for the
/// kernel call so a test can supply a kernel that ignores the flag.
fn probe_decompress_in(
    dir: &Path,
    decompress: impl Fn(&File) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Write;
    // Anonymous, so nothing is left behind whatever happens next.
    let file = tempfile::tempfile_in(dir)?;
    let line = b"flummox decompress probe, compressible text.\n";
    let mut writer = io::BufWriter::new(&file);
    for _ in 0..(4 * 1024 * 1024 / line.len()) {
        writer.write_all(line)?;
    }
    writer.flush()?;
    drop(writer);
    file.sync_all()?;
    // If the file will not compress, the probe cannot tell anything about
    // decompressing, and the pass itself still checks every file.
    if compress_fd(&file, 3).is_err() || compressed_bytes_fd(&file)?.0 == 0 {
        return Ok(());
    }
    let unsupported = |_: io::Error| io::Error::new(io::ErrorKind::Unsupported, PROBE_FAILED);
    decompress(&file).map_err(unsupported)?;
    ensure_decompressed(&file).map_err(unsupported)
}

/// Limits each in-flight ioctl to 16 MiB. An interrupted file gets no success
/// receipt, so retrying safely revisits it even if some ranges were rewritten.
///
/// `gate` is the pass-wide rate limit for the busy check, so a pass over
/// many files scans `/proc` once per interval and not once per file.
fn rewrite_ranges(
    file: &File,
    ctx: &JobCtx<'_>,
    gate: &std::sync::Mutex<std::time::Instant>,
    op: impl Fn(u64, u64) -> io::Result<i32>,
) -> io::Result<i32> {
    const RANGE: u64 = 16 * 1024 * 1024;
    file.sync_all()?;
    let length = file.metadata()?.len();
    let mut start = 0;
    let mut applied = i32::MAX;
    while start < length {
        if !ctx.wait_while_busy(gate) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Stopped between file ranges; retry to finish this file",
            ));
        }
        let size = RANGE.min(length - start);
        applied = applied.min(op(start, size)?);
        start += size;
    }
    Ok(if applied == i32::MAX { 0 } else { applied })
}

/// Whether the directory carries the `btrfs.compression` property.
///
/// Reports the algorithm, or `None` when the property is unset. A symlink to a
/// directory is followed, so the answer is about the directory it leads to.
pub fn dir_property(dir: &Path) -> io::Result<Option<String>> {
    dir_property_fd(&File::open(dir)?)
}

/// [`dir_property`], for a directory that is already open.
pub fn dir_property_fd(dir: &File) -> io::Result<Option<String>> {
    use xattr::FileExt;
    match dir.get_xattr("btrfs.compression") {
        Ok(Some(raw)) => Ok(Some(String::from_utf8_lossy(&raw).into_owned())),
        Ok(None) => Ok(None),
        // Not btrfs, or the property was never set.
        Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Sets the directory's `btrfs.compression` property.
///
/// Files and subdirectories created in the directory afterwards inherit it, so
/// a Steam library whose folders all carry it receives every future download
/// compressed as it is written, costing no extra reading or rewriting.
///
/// The property names the algorithm only. The level comes from the mount, so
/// a pass at a chosen level still sets that per file.
pub fn set_dir_property(dir: &Path, enabled: bool) -> io::Result<()> {
    set_dir_property_fd(&File::open(dir)?, enabled)
}

/// [`set_dir_property`], for a directory that is already open.
pub fn set_dir_property_fd(dir: &File, enabled: bool) -> io::Result<()> {
    use xattr::FileExt;
    if enabled {
        dir.set_xattr("btrfs.compression", b"zstd")
    } else {
        remove_property(dir)
    }
}

/// Removes the property, treating "never set" as success.
fn remove_property(file: &File) -> io::Result<()> {
    use xattr::FileExt;
    match file.remove_xattr("btrfs.compression") {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Sets or clears the property on the anchor's directory and on every
/// directory below it.
///
/// btrfs copies the property from the parent only when an inode is created,
/// so folders that already exist must be marked one by one. Directories are
/// reached through the anchor, so a symlink cannot redirect the change.
/// Attempts every directory and reports the first failure.
pub fn set_tree_property(anchor: &Anchor, enabled: bool) -> io::Result<()> {
    let mut first_error = None;
    let walker = walkdir::WalkDir::new(anchor.path())
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != std::ffi::OsStr::new(crate::inventory::STORE_DIR));
    for entry in walker.flatten() {
        if !entry.file_type().is_dir() {
            continue;
        }
        let opened = match entry.path().strip_prefix(anchor.path()) {
            Ok(rel) if rel.as_os_str().is_empty() => {
                anchor.as_fd().try_clone_to_owned().map(File::from)
            }
            Ok(rel) => anchor.open_with(
                rel,
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            ),
            Err(_) => continue,
        };
        if let Err(e) = opened.and_then(|dir| set_dir_property_fd(&dir, enabled)) {
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Logical bytes stored in compressed extents, and bytes mapped in total.
///
/// `compsize` reports the *compressed* size, but that needs `CAP_SYS_ADMIN`.
/// FIEMAP is unprivileged and still says which extents are compressed, which
/// is enough to show how much of a game is done.
pub fn compressed_bytes(path: &Path) -> io::Result<(u64, u64)> {
    compressed_bytes_fd(&File::open(path)?)
}

/// [`compressed_bytes`], for a file that is already open.
pub fn compressed_bytes_fd(file: &File) -> io::Result<(u64, u64)> {
    let fd = file.as_raw_fd();
    let mut compressed = 0u64;
    let mut total = 0u64;
    let mut start = 0u64;

    // struct fiemap followed by its flexible array of extents.
    let header = size_of::<Fiemap>();
    let stride = size_of::<FiemapExtent>();
    let mut buf = vec![0u8; header + stride * FIEMAP_BATCH as usize];

    loop {
        let query = Fiemap {
            start,
            length: u64::MAX,
            flags: FIEMAP_FLAG_SYNC,
            mapped_extents: 0,
            extent_count: FIEMAP_BATCH,
            reserved: 0,
        };
        let ptr = buf.as_mut_ptr();
        // SAFETY: `buf` is at least `size_of::<Fiemap>()` bytes, and an
        // unaligned write needs no more than that.
        unsafe { std::ptr::write_unaligned(ptr.cast::<Fiemap>(), query) };
        // SAFETY: `fd` is open and `buf` is a fiemap header followed by room
        // for `FIEMAP_BATCH` extents, as the ioctl requires.
        unsafe { fs_ioc_fiemap(fd, ptr.cast::<Fiemap>()) }.map_err(errno_to_io)?;
        // SAFETY: the ioctl succeeded, so it wrote the header back.
        let out = unsafe { std::ptr::read_unaligned(ptr.cast::<Fiemap>()) };

        if out.mapped_extents == 0 {
            break;
        }
        let mut last_seen = false;
        let mut next_start = start;
        for i in 0..out.mapped_extents as usize {
            let offset = header + i * stride;
            let Some(slice) = buf.get(offset..offset + stride) else {
                break;
            };
            // SAFETY: `slice` is exactly one extent's worth of bytes the
            // kernel just wrote, read back without assuming alignment.
            let ext = unsafe { std::ptr::read_unaligned(slice.as_ptr().cast::<FiemapExtent>()) };
            total = total.saturating_add(ext.length);
            if ext.flags & FIEMAP_EXTENT_ENCODED != 0 {
                compressed = compressed.saturating_add(ext.length);
            }
            next_start = ext.logical.saturating_add(ext.length);
            if ext.flags & FIEMAP_EXTENT_LAST != 0 {
                last_seen = true;
            }
        }
        if last_seen || next_start <= start {
            break;
        }
        start = next_start;
    }
    Ok((compressed, total))
}

fn errno_to_io(e: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

/// Reports what share of a file btrfs already stores compressed.
#[derive(Debug, Clone, Copy)]
pub struct FiemapProbe;

impl crate::estimate::DiskProbe for FiemapProbe {
    fn measure(&self, path: &Path) -> Option<(u64, u64)> {
        compressed_bytes(path).ok()
    }
}

/// Mirrors `struct btrfs_ioctl_fs_info_args`. The kernel fills every field and
/// only `fsid` is read back.
#[repr(C)]
#[allow(dead_code)]
struct FsInfoArgs {
    max_id: u64,
    num_devices: u64,
    fsid: [u8; 16],
    nodesize: u32,
    sectorsize: u32,
    clone_alignment: u32,
    csum_type: u16,
    csum_size: u16,
    flags: u64,
    generation: u64,
    metadata_uuid: [u8; 16],
    reserved: [u8; 944],
}

nix::ioctl_read!(btrfs_fs_info, 0x94, 31, FsInfoArgs);

/// The UUID of the btrfs filesystem holding `path`, in the usual dashed form.
///
/// Every subvolume and device of one filesystem gives the same answer, which
/// `statfs` does not: its filesystem id mixes in the subvolume.
pub fn filesystem_uuid(path: &Path) -> io::Result<String> {
    let dir = File::open(path)?;
    let mut args = FsInfoArgs {
        max_id: 0,
        num_devices: 0,
        fsid: [0; 16],
        nodesize: 0,
        sectorsize: 0,
        clone_alignment: 0,
        csum_type: 0,
        csum_size: 0,
        flags: 0,
        generation: 0,
        metadata_uuid: [0; 16],
        reserved: [0; 944],
    };
    // SAFETY: `dir` is open and `args` is a correctly laid out
    // `btrfs_ioctl_fs_info_args` that the kernel fills in. A path on another
    // filesystem makes the kernel return ENOTTY.
    unsafe { btrfs_fs_info(dir.as_raw_fd(), &mut args) }.map_err(errno_to_io)?;
    let hex: Vec<String> = args.fsid.iter().map(|b| format!("{b:02x}")).collect();
    let part = |from: usize, to: usize| hex.get(from..to).unwrap_or_default().concat();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        part(0, 4),
        part(4, 6),
        part(6, 8),
        part(8, 10),
        part(10, 16)
    ))
}

nix::ioctl_read!(fs_ioc_getflags, b'f', 1, libc::c_long);

/// `FS_NOCOW_FL`: the inode is excluded from copy-on-write, and so from
/// compression.
const FS_NOCOW_FL: libc::c_long = 0x0080_0000;

/// Whether the file carries the no-copy-on-write attribute. btrfs never
/// compresses such a file, so a pass skips it and an estimate should not
/// count it. Fails on a filesystem without the ioctl.
pub fn is_nocow(file: &File) -> io::Result<bool> {
    let mut flags: libc::c_long = 0;
    // SAFETY: `file` is open and `flags` is writable for the whole call. A
    // filesystem without the ioctl makes the kernel return ENOTTY.
    unsafe { fs_ioc_getflags(file.as_raw_fd(), &mut flags) }.map_err(errno_to_io)?;
    Ok(flags & FS_NOCOW_FL != 0)
}

/// Why a job should not rewrite this file, if there is a reason.
///
/// btrfs never compresses a no-copy-on-write file, yet the defrag ioctl
/// still returns success for it, so a pass would record it as done.
fn leave_alone_reason(file: &File) -> Option<&'static str> {
    is_nocow(file)
        .ok()?
        .then_some("btrfs does not compress files marked no-copy-on-write")
}

/// Compresses games in place on btrfs.
#[derive(Debug, Clone, Copy)]
pub struct BtrfsBackend;

impl BtrfsBackend {
    /// Runs one pass over the inventory, calling `op` per file.
    fn run(
        &self,
        install_dir: &Path,
        inv: &Inventory,
        threads: usize,
        ctx: &JobCtx<'_>,
        op: &(dyn Fn(&File, &std::sync::Mutex<std::time::Instant>) -> io::Result<i32> + Sync),
        after: &(dyn Fn(&File) -> io::Result<()> + Sync),
    ) -> io::Result<Outcome> {
        use rayon::prelude::*;

        // Hold the install directory open for the whole job and reach every
        // file through it, so nothing can substitute a symlink for a path
        // between the walk that chose these files and the rewrite of each one.
        let anchor = Anchor::open(install_dir)?;
        if !anchor.fully_resolved() {
            ctx.events.event(Event::Warning(
                "this kernel has no openat2, so only the last part of each path is \
                 checked against symlinks"
                    .to_owned(),
            ));
        }
        let targets: Vec<_> = inv.to_compress().collect();
        let files = targets.len() as u64;
        let bytes = targets
            .iter()
            .fold(0u64, |total, f| total.saturating_add(f.size));
        ctx.events.event(Event::Started { files, bytes });
        if files == 0 {
            return Ok(Outcome {
                skipped: inv.files.len() as u64,
                cancelled: ctx.cancelled(),
                ..Outcome::default()
            });
        }

        let free_before = free_bytes(install_dir).ok();
        let files_done = AtomicU64::new(0);
        let bytes_done = AtomicU64::new(0);
        // Starts above any real zstd level so the first file lowers it.
        let applied_level = std::sync::atomic::AtomicI64::new(i64::from(i32::MAX));
        let errors = std::sync::Mutex::new(Vec::new());
        let completed = std::sync::Mutex::new(Vec::new());
        let left_alone = AtomicU64::new(0);
        let report = std::sync::Mutex::new(());

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .build()
            .map_err(|e| io::Error::other(e.to_string()))?;
        pool.install(|| {
            let last_busy_check = std::sync::Mutex::new(
                std::time::Instant::now() - std::time::Duration::from_secs(60),
            );
            targets.par_iter().for_each(|entry| {
                if ctx.cancelled() {
                    return;
                }
                // Admission gate; large files have additional range checkpoints.
                if !ctx.wait_while_busy(&last_busy_check) {
                    return;
                }
                let fail = |e: std::io::Error| {
                    let msg = format!("{}: {e}", entry.rel.display());
                    ctx.events.event(Event::Warning(msg.clone()));
                    if let Ok(mut errs) = errors.lock() {
                        errs.push(msg);
                    }
                };
                let file = match anchor.open_file(&entry.rel) {
                    Ok(file) => file,
                    Err(e) => return fail(e),
                };
                if !entry.matches_file(&file).unwrap_or(false) {
                    return fail(io::Error::other("file changed since analysis; retry it"));
                }
                if let Some(reason) = leave_alone_reason(&file) {
                    left_alone.fetch_add(1, Ordering::Relaxed);
                    ctx.events.event(Event::Warning(format!(
                        "{}: skipped, {reason}",
                        entry.rel.display()
                    )));
                    return;
                }
                match op(&file, &last_busy_check) {
                    // Every file in a pass gets the same treatment, so the
                    // lowest level seen is the level the pass achieved.
                    Ok(applied) => {
                        if !entry.matches_file(&file).unwrap_or(false) {
                            return fail(io::Error::other(
                                "file changed during processing; retry it",
                            ));
                        }
                        if let Err(e) = after(&file) {
                            return fail(e);
                        }
                        applied_level.fetch_min(i64::from(applied), Ordering::Relaxed);
                        if let Ok(mut done) = completed.lock() {
                            done.push(((**entry).clone(), applied));
                        }
                        ctx.events.event(Event::FileCompleted {
                            entry: (**entry).clone(),
                            level: applied,
                        });
                    }
                    // The user stopped the job between ranges. That is the
                    // outcome's `cancelled` flag, not a failure of this file.
                    Err(e) if e.kind() == io::ErrorKind::Interrupted && ctx.cancelled() => {
                        return;
                    }
                    // The kernel checks write permission for the defrag ioctl
                    // even on a read-only handle, so a file the owner cannot
                    // write is refused on every pass.
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                        left_alone.fetch_add(1, Ordering::Relaxed);
                        ctx.events.event(Event::Warning(format!(
                            "{}: skipped, the file is not writable by this user",
                            entry.rel.display()
                        )));
                        return;
                    }
                    Err(e) => return fail(e),
                }
                // Counting and reporting under one lock keeps events in
                // counting order, so no later event carries a smaller total.
                let _ordered = report.lock();
                let done = files_done.fetch_add(1, Ordering::Relaxed) + 1;
                let bdone = bytes_done.fetch_add(entry.size, Ordering::Relaxed) + entry.size;
                ctx.events.event(Event::Progress {
                    files_done: done,
                    bytes_done: bdone,
                    current: entry.rel.display().to_string(),
                });
            });
        });

        // The kernel writes compressed extents back asynchronously, so the
        // free-space reading below would otherwise show the old figure.
        // syncfs covers this filesystem only; sync(2) blocks on every mounted
        // filesystem, ignores Ctrl-C, and can stall for tens of seconds on an
        // unrelated slow drive.
        if let Err(e) = rustix::fs::syncfs(anchor.as_fd()) {
            ctx.events.event(Event::Warning(format!(
                "could not flush the filesystem: {e}"
            )));
        }
        Ok(Outcome {
            files: files_done.load(Ordering::Relaxed),
            bytes: bytes_done.load(Ordering::Relaxed),
            skipped: (inv.files.len() as u64)
                .saturating_sub(files)
                .saturating_add(left_alone.load(Ordering::Relaxed)),
            free_before,
            free_after: free_bytes(install_dir).ok(),
            effective_level: i32::try_from(applied_level.load(Ordering::Relaxed))
                .ok()
                .filter(|level| *level != i32::MAX),
            cancelled: ctx.cancelled(),
            completed: completed
                .into_inner()
                .map_err(|_| io::Error::other("outcome lock poisoned"))?,
            errors: errors.into_inner().unwrap_or_default(),
        })
    }
}

impl Backend for BtrfsBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Btrfs
    }

    fn model(&self, opts: &CompressOpts) -> Box<dyn UnitModel> {
        Box::new(BtrfsModel {
            level: opts.btrfs_level(),
        })
    }

    fn walk_opts(&self) -> crate::inventory::WalkOpts {
        crate::inventory::WalkOpts::native()
    }

    fn disk_probe(&self) -> Box<dyn crate::estimate::DiskProbe> {
        Box::new(FiemapProbe)
    }

    fn compress(
        &self,
        install_dir: &Path,
        inv: &Inventory,
        opts: &CompressOpts,
        ctx: &JobCtx<'_>,
    ) -> io::Result<Outcome> {
        let plan = opts.level_plan();
        let outcome = self.run(
            install_dir,
            inv,
            opts.threads,
            ctx,
            &|f, gate| {
                let level = match plan {
                    crate::backend::LevelPlan::Fixed(level) => level,
                    crate::backend::LevelPlan::PerFile { low, high } => {
                        crate::estimate::choose_level(
                            f,
                            f.metadata()?.len(),
                            &BtrfsModel { level: low },
                            low,
                            high,
                        )?
                    }
                };
                rewrite_ranges(f, ctx, gate, |start, len| {
                    compress_range(f, level, start, len)
                })
            },
            &|_| Ok(()),
        )?;
        // Do this last: if the job failed or was cancelled, the directory
        // should not claim to be compressed.
        if !outcome.cancelled
            && outcome.errors.is_empty()
            && let Err(e) = Anchor::open(install_dir).and_then(|a| set_tree_property(&a, true))
        {
            ctx.events.event(Event::Warning(format!(
                "could not set btrfs.compression on every folder of the game, so files \
                 Steam writes there later may not be compressed automatically: {e}"
            )));
        }
        ctx.events.event(Event::Finished(Box::new(outcome.clone())));
        Ok(outcome)
    }

    fn decompress(
        &self,
        install_dir: &Path,
        inv: &Inventory,
        ctx: &JobCtx<'_>,
    ) -> io::Result<Outcome> {
        // Every file is decompressed, not just the ones a compress pass would
        // pick, so this always returns the directory to plain storage.
        let all = Inventory {
            files: inv
                .files
                .iter()
                .cloned()
                .map(|mut f| {
                    f.action = crate::inventory::Action::Compress;
                    f
                })
                .collect(),
            warnings: Vec::new(),
        };
        // Files created while the property was on carry their own copy, which
        // is cleared once the file has been rewritten, since removing it
        // changes the inode's ctime.
        let outcome = self.run(
            install_dir,
            &all,
            2,
            ctx,
            &|f, gate| {
                rewrite_ranges(f, ctx, gate, |start, len| {
                    decompress_range(f, start, len).map(|()| 0)
                })?;
                ensure_decompressed(f)?;
                Ok(0)
            },
            &remove_property,
        )?;
        // The folders go last: a kernel that cannot decompress leaves them
        // as they were, still claiming what is still true.
        if !outcome.cancelled
            && outcome.errors.is_empty()
            && let Err(e) = Anchor::open(install_dir).and_then(|a| set_tree_property(&a, false))
        {
            ctx.events
                .event(Event::Warning(format!("clearing btrfs.compression: {e}")));
        }
        ctx.events.event(Event::Finished(Box::new(outcome.clone())));
        Ok(outcome)
    }

    fn status(&self, install_dir: &Path, inv: &Inventory) -> io::Result<CompressionStatus> {
        use rayon::prelude::*;

        let anchor = Anchor::open(install_dir)?;
        let (compressed, total, files) = inv
            .files
            .par_iter()
            .map(|entry| {
                match anchor
                    .open_file(&entry.rel)
                    .and_then(|f| compressed_bytes_fd(&f))
                {
                    Ok((c, t)) => (c, t, 1),
                    // A file that vanished or cannot be read simply does not
                    // contribute; status is a report, not a job.
                    Err(_) => (0, 0, 0),
                }
            })
            .reduce(
                || (0u64, 0u64, 0u64),
                |a, b| {
                    (
                        a.0.saturating_add(b.0),
                        a.1.saturating_add(b.1),
                        a.2.saturating_add(b.2),
                    )
                },
            );
        Ok(CompressionStatus {
            compressed_bytes: compressed,
            total_bytes: total,
            files,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    use super::*;
    use crate::fsprobe;

    /// The ioctl argument layout must match `linux/btrfs.h` exactly, or the
    /// kernel reads the level from the wrong offset.
    #[test]
    fn defrag_args_match_the_kernel_layout() -> TestResult {
        check_eq(
            size_of::<DefragRangeArgs>(),
            48,
            "DefragRangeArgs is 48 bytes",
        )?;
        check_eq(
            align_of::<DefragRangeArgs>(),
            8,
            "DefragRangeArgs is 8-byte aligned",
        )?;
        check_eq(size_of::<FiemapExtent>(), 56, "FiemapExtent is 56 bytes")?;
        check_eq(size_of::<Fiemap>(), 32, "Fiemap is 32 bytes")
    }

    /// A temporary directory on btrfs, or `None` when the test should skip.
    ///
    /// Fails instead of skipping when `FLUMMOX_REQUIRE_BTRFS` is set.
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

    #[test]
    fn requested_levels_are_clamped_and_zero_means_the_default() -> TestResult {
        check_eq(applied_level(19), 15, "above the range")?;
        check_eq(applied_level(-40), -15, "below the range")?;
        check_eq(applied_level(0), DEFAULT_LEVEL, "zero is the default")?;
        check_eq(applied_level(9), 9, "in range")
    }

    #[test]
    fn only_kernels_from_6_15_take_a_level() -> TestResult {
        check(!release_accepts_level("6.14.9-arch1-1"), "6.14")?;
        check(!release_accepts_level("6.8.0-45-generic"), "6.8")?;
        check(release_accepts_level("6.15.0"), "6.15")?;
        check(release_accepts_level("7.2.9-1-cachyos"), "7.2")?;
        check(!release_accepts_level("unknown"), "unparseable means no")?;
        check(attainable_level(9) <= 9, "never above what was asked")
    }

    #[test]
    fn cancellation_between_ranges_preserves_bytes_and_leaves_no_success_receipt() -> TestResult {
        use crate::backend::{BusyCheck, EventSink};
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        let Some(tmp) = btrfs_tempdir()? else {
            return Ok(());
        };
        let path = tmp.path().join("large.dat");
        let data = vec![b'A'; 32 * 1024 * 1024];
        std::fs::write(&path, &data).ctx("large fixture")?;
        let anchor = Anchor::open(tmp.path()).ctx("anchor")?;
        decompress_fd(
            &anchor
                .open_file(Path::new("large.dat"))
                .ctx("fixture file")?,
        )
        .ctx("uncompressed baseline")?;
        let (before, _) = compressed_bytes(&path).ctx("baseline")?;
        check_eq(before, 0, "baseline starts raw")?;
        struct Sink;
        impl EventSink for Sink {
            fn event(&self, _: Event) {}
        }
        struct Stop<'a> {
            calls: AtomicUsize,
            cancel: &'a AtomicBool,
        }
        impl BusyCheck for Stop<'_> {
            fn check_interval(&self) -> std::time::Duration {
                std::time::Duration::ZERO
            }
            fn in_use_by(&self) -> Option<String> {
                if self.calls.fetch_add(1, Ordering::Relaxed) >= 2 {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                None
            }
        }
        let cancel = AtomicBool::new(false);
        let stop = Stop {
            calls: AtomicUsize::new(0),
            cancel: &cancel,
        };
        let ctx = JobCtx {
            events: &Sink,
            cancel: &cancel,
            busy: Some(&stop),
        };
        let inv = crate::inventory::walk(tmp.path(), &crate::inventory::WalkOpts::native())
            .ctx("inventory")?;
        let outcome = BtrfsBackend
            .compress(
                tmp.path(),
                &inv,
                &CompressOpts {
                    threads: 1,
                    ..Default::default()
                },
                &ctx,
            )
            .ctx("cancelled pass")?;
        check(outcome.cancelled, "cancel is reported")?;
        check_eq(
            outcome.errors.clone(),
            Vec::<String>::new(),
            "a cancel is not a per-file failure",
        )?;
        check(
            outcome.completed.is_empty(),
            "a partial file has no success receipt",
        )?;
        let (compressed, total) = compressed_bytes(&path).ctx("partial extents")?;
        check(
            compressed > 0 && compressed < total,
            "only a bounded range was compressed",
        )?;
        check_eq(
            std::fs::read(&path).ctx("bytes after cancellation")?,
            data,
            "partial compression preserves every byte",
        )
    }

    #[test]
    fn compresses_and_decompresses_a_real_file() -> TestResult {
        let Some(tmp) = btrfs_tempdir()? else {
            return Ok(());
        };
        let path = tmp.path().join("text.dat");
        std::fs::write(&path, "compress me ".repeat(500_000).into_bytes()).ctx("write text.dat")?;
        // Reached the way a job reaches it, through an anchored open.
        let anchor = Anchor::open(tmp.path()).ctx("open the anchor")?;
        let file = anchor
            .open_file(Path::new("text.dat"))
            .ctx("open text.dat")?;

        compress_fd(&file, 15).ctx("compress text.dat")?;
        let (compressed, total) = compressed_bytes(&path).ctx("map extents after compressing")?;
        check(total > 0, "the file maps at least one extent")?;
        check(
            compressed > 0,
            format!("expected compressed extents, got {compressed}/{total}"),
        )?;

        decompress_fd(&file).ctx("decompress text.dat")?;
        let (compressed, total) = compressed_bytes(&path).ctx("map extents after decompressing")?;
        check_eq(
            compressed,
            0,
            format!("expected no compressed extents, got {compressed}/{total}"),
        )?;

        // Contents must survive both passes.
        let back = std::fs::read(&path).ctx("read text.dat back")?;
        check_eq(
            back,
            "compress me ".repeat(500_000).into_bytes(),
            "every byte survives both passes",
        )
    }

    /// Compressing a file must not disturb the fields the incremental pass
    /// compares.
    ///
    /// A pass stores a fingerprint per file and recompresses only what
    /// changed. The fingerprint includes ctime, and a defrag rewrites the
    /// inode, so if the kernel bumped ctime then every file we just
    /// compressed would look changed on the next run and the whole library
    /// would be rewritten every time, without any error to notice. Measured
    /// on kernel 7.2: it does not. This test exists so that stays true.
    #[test]
    fn compressing_leaves_the_fingerprint_fields_alone() -> TestResult {
        use std::os::unix::fs::MetadataExt;

        let Some(tmp) = btrfs_tempdir()? else {
            return Ok(());
        };
        let path = tmp.path().join("fingerprint.dat");
        std::fs::write(&path, "compress me ".repeat(200_000).into_bytes())
            .ctx("write the test file")?;
        let before = std::fs::metadata(&path).ctx("stat before")?;

        let anchor = Anchor::open(tmp.path()).ctx("open the anchor")?;
        let file = anchor
            .open_file(Path::new("fingerprint.dat"))
            .ctx("open the file")?;
        compress_fd(&file, 15).ctx("compress the file")?;
        let after = std::fs::metadata(&path).ctx("stat after")?;

        check_eq(after.ino(), before.ino(), "the inode must survive")?;
        check_eq(
            after.size(),
            before.size(),
            "the logical size must not move",
        )?;
        check_eq(
            after.mtime_nsec(),
            before.mtime_nsec(),
            "mtime must not move",
        )?;
        check_eq(
            after.ctime_nsec(),
            before.ctime_nsec(),
            "ctime must not move, or every compressed file looks changed next run",
        )
    }

    #[test]
    fn sets_and_clears_the_directory_property() -> TestResult {
        let Some(tmp) = btrfs_tempdir()? else {
            return Ok(());
        };
        set_dir_property(tmp.path(), true).ctx("set btrfs.compression")?;
        check_eq(
            xattr::get(tmp.path(), "btrfs.compression")
                .ctx("read btrfs.compression")?
                .as_deref(),
            Some(b"zstd".as_slice()),
            "the directory property after setting it",
        )?;
        set_dir_property(tmp.path(), false).ctx("clear btrfs.compression")?;
        check(
            xattr::get(tmp.path(), "btrfs.compression")
                .ctx("re-read btrfs.compression")?
                .is_none(),
            "the property is gone after clearing",
        )?;
        // Clearing twice must not fail.
        set_dir_property(tmp.path(), false).ctx("clear btrfs.compression a second time")
    }

    #[test]
    fn a_kernel_that_leaves_the_probe_compressed_fails_the_job() -> TestResult {
        let Some(tmp) = btrfs_tempdir()? else {
            return Ok(());
        };
        // Control: the real call decompresses the probe file on this kernel.
        probe_decompress_in(tmp.path(), decompress_fd)
            .ctx("control: a kernel that honours the flag passes")?;
        // A kernel that ignores the flag returns success and changes nothing.
        let ignored = probe_decompress_in(tmp.path(), |_| Ok(()));
        check(
            ignored
                .as_ref()
                .err()
                .is_some_and(|error| error.to_string() == PROBE_FAILED),
            format!("a probe that stays compressed fails with the kernel message: {ignored:?}"),
        )?;
        // A kernel that rejects the flag fails the same way.
        let rejected = probe_decompress_in(tmp.path(), |_| {
            Err(io::Error::new(io::ErrorKind::Unsupported, "no"))
        });
        check(
            rejected
                .as_ref()
                .err()
                .is_some_and(|error| error.to_string() == PROBE_FAILED),
            format!("a rejected flag fails with the kernel message: {rejected:?}"),
        )
    }

    #[test]
    fn the_decompress_probe_passes_here_and_leaves_nothing_behind() -> TestResult {
        let Some(tmp) = btrfs_tempdir()? else {
            return Ok(());
        };
        let library = tmp.path().join("Library");
        let game = library.join("game");
        let state = tmp.path().join("state");
        std::fs::create_dir_all(&game).ctx("game folder")?;
        std::fs::create_dir(&state).ctx("state folder")?;
        probe_decompress(&game, &state).ctx("the probe passes on this machine")?;
        for dir in [library, state, game] {
            let leftovers: Vec<_> = std::fs::read_dir(&dir)
                .ctx("list")?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name())
                .filter(|name| name.to_string_lossy().starts_with(".flummox-probe"))
                .collect();
            check(
                leftovers.is_empty(),
                format!("{} holds {leftovers:?}", dir.display()),
            )?;
        }
        // Control: with no usable scratch folder nothing is learned and the job goes on.
        probe_decompress(
            &tmp.path().join("missing/game"),
            &tmp.path().join("missing"),
        )
        .ctx("no scratch folder is not a failure")
    }
}
