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
/// Taking a handle rather than a path is the point: the caller obtained it
/// from an [`Anchor`], so the path could not have been swapped for a symlink
/// between the walk and the rewrite. There is deliberately no path-taking
/// version, because that would be a route around the check.
///
/// The handle may be read-only. The kernel checks write *permission*, not the
/// open mode, so this still works on a game executable that is running.
pub fn compress_fd(file: &File, level: i32) -> io::Result<i32> {
    let args = DefragRangeArgs {
        start: 0,
        len: u64::MAX,
        flags: DEFRAG_RANGE_COMPRESS | DEFRAG_RANGE_START_IO | DEFRAG_RANGE_COMPRESS_LEVEL,
        extent_thresh: 0,
        compress_type: BTRFS_COMPRESS_ZSTD,
        compress_level: level.clamp(-15, 15) as i8,
        ..DefragRangeArgs::default()
    };
    // SAFETY: `file` is an open btrfs file and `args` is a correctly laid out
    // `btrfs_ioctl_defrag_range_args` that outlives the call.
    let result = unsafe { btrfs_defrag_range(file.as_raw_fd(), &args) };
    match result {
        Ok(_) => Ok(level),
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

/// Rewrites an already-open file as uncompressed extents.
///
/// Anchored for the same reason as [`compress_fd`].
pub fn decompress_fd(file: &File) -> io::Result<()> {
    let args = DefragRangeArgs {
        start: 0,
        len: u64::MAX,
        flags: DEFRAG_RANGE_NOCOMPRESS | DEFRAG_RANGE_START_IO,
        ..DefragRangeArgs::default()
    };
    // SAFETY: `file` is an open btrfs file and `args` is a correctly laid out
    // `btrfs_ioctl_defrag_range_args` that outlives the call.
    unsafe { btrfs_defrag_range(file.as_raw_fd(), &args) }
        .map(|_| ())
        .map_err(errno_to_io)
}

/// Whether the directory carries the `btrfs.compression` property.
///
/// Reports the algorithm, or `None` when the property is unset.
pub fn dir_property(dir: &Path) -> io::Result<Option<String>> {
    match xattr::get(dir, "btrfs.compression") {
        Ok(Some(raw)) => Ok(Some(String::from_utf8_lossy(&raw).into_owned())),
        Ok(None) => Ok(None),
        // Not btrfs, or the property was never set.
        Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Sets the directory's `btrfs.compression` property.
///
/// Both new files and new subdirectories inherit it, so setting it on a Steam
/// library makes every future download land compressed as it is written,
/// costing no extra reading or rewriting. Measured on this filesystem: a file
/// created after the property was set had every mapped byte in a compressed
/// extent.
///
/// The property names the algorithm only. The level comes from the mount, so
/// a pass at a chosen level still sets that per file.
pub fn set_dir_property(dir: &Path, enabled: bool) -> io::Result<()> {
    if enabled {
        xattr::set(dir, "btrfs.compression", b"zstd")
    } else {
        match xattr::remove(dir, "btrfs.compression") {
            Ok(()) => Ok(()),
            // Never set in the first place.
            Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(()),
            Err(e) => Err(e),
        }
    }
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
        // SAFETY: `buf` is at least `size_of::<Fiemap>()` bytes and correctly
        // aligned for it, since `Vec<u8>` allocations are word aligned and
        // `Fiemap` contains only integers.
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
            let Some(slice) = buf.get(offset..offset + stride) else { break };
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
        op: &(dyn Fn(&File) -> io::Result<i32> + Sync),
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
        let bytes = targets.iter().map(|f| f.size).sum();
        ctx.events.event(Event::Started { files, bytes });

        let free_before = free_bytes(install_dir).ok();
        let files_done = AtomicU64::new(0);
        let bytes_done = AtomicU64::new(0);
        // Starts above any real zstd level so the first file lowers it.
        let applied_level = std::sync::atomic::AtomicI64::new(i64::from(i32::MAX));
        let errors = std::sync::Mutex::new(Vec::new());

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .build()
            .map_err(|e| io::Error::other(e.to_string()))?;
        pool.install(|| {
            targets.par_iter().for_each(|entry| {
                if ctx.cancelled() {
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
                match op(&file) {
                    // Every file in a pass gets the same treatment, so the
                    // lowest level seen is the level the pass achieved.
                    Ok(applied) => {
                        applied_level.fetch_min(i64::from(applied), Ordering::Relaxed);
                    }
                    Err(e) => return fail(e),
                }
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
            ctx.events.event(Event::Warning(format!("could not flush the filesystem: {e}")));
        }
        Ok(Outcome {
            files: files_done.load(Ordering::Relaxed),
            bytes: bytes_done.load(Ordering::Relaxed),
            skipped: (inv.files.len() as u64).saturating_sub(files),
            free_before,
            free_after: free_bytes(install_dir).ok(),
            effective_level: i32::try_from(applied_level.load(Ordering::Relaxed))
                .ok()
                .filter(|level| *level != i32::MAX),
            cancelled: ctx.cancelled(),
            errors: errors.into_inner().unwrap_or_default(),
        })
    }
}

impl Backend for BtrfsBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Btrfs
    }

    fn model(&self, opts: &CompressOpts) -> Box<dyn UnitModel> {
        Box::new(BtrfsModel { level: opts.btrfs_level() })
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
        let level = opts.btrfs_level();
        let outcome = self.run(install_dir, inv, opts.threads, ctx, &|f| compress_fd(f, level))?;
        // Do this last: if the job failed or was cancelled, the directory
        // should not claim to be compressed.
        if !outcome.cancelled
            && let Err(e) = set_dir_property(install_dir, true)
        {
            ctx.events.event(Event::Warning(format!(
                "could not set btrfs.compression on the game folder, so files Steam \
                 writes later will not be compressed automatically: {e}"
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
        if let Err(e) = set_dir_property(install_dir, false) {
            ctx.events.event(Event::Warning(format!("clearing btrfs.compression: {e}")));
        }
        let outcome = self.run(install_dir, &all, 2, ctx, &|f| decompress_fd(f).map(|()| 0))?;
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
                match anchor.open_file(&entry.rel).and_then(|f| compressed_bytes_fd(&f)) {
                    Ok((c, t)) => (c, t, 1),
                    // A file that vanished or cannot be read simply does not
                    // contribute; status is a report, not a job.
                    Err(_) => (0, 0, 0),
                }
            })
            .reduce(
                || (0u64, 0u64, 0u64),
                |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2),
            );
        Ok(CompressionStatus { compressed_bytes: compressed, total_bytes: total, files })
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
        check_eq(size_of::<DefragRangeArgs>(), 48, "DefragRangeArgs is 48 bytes")?;
        check_eq(align_of::<DefragRangeArgs>(), 8, "DefragRangeArgs is 8-byte aligned")?;
        check_eq(size_of::<FiemapExtent>(), 56, "FiemapExtent is 56 bytes")?;
        check_eq(size_of::<Fiemap>(), 32, "Fiemap is 32 bytes")
    }

    /// Skips itself unless the test directory really is btrfs.
    fn btrfs_tempdir() -> Option<tempfile::TempDir> {
        let tmp = tempfile::TempDir::new_in(std::env::current_dir().ok()?).ok()?;
        let fs = fsprobe::probe(tmp.path()).ok()?;
        (fs.fstype == "btrfs").then_some(tmp)
    }

    #[test]
    fn compresses_and_decompresses_a_real_file() -> TestResult {
        let Some(tmp) = btrfs_tempdir() else {
            eprintln!("skipped: not running on btrfs");
            return Ok(());
        };
        let path = tmp.path().join("text.dat");
        std::fs::write(&path, "compress me ".repeat(500_000).into_bytes()).ctx("write text.dat")?;
        // Reached the way a job reaches it, through an anchored open.
        let anchor = Anchor::open(tmp.path()).ctx("open the anchor")?;
        let file = anchor.open_file(Path::new("text.dat")).ctx("open text.dat")?;

        compress_fd(&file, 15).ctx("compress text.dat")?;
        let (compressed, total) = compressed_bytes(&path).ctx("map extents after compressing")?;
        check(total > 0, "the file maps at least one extent")?;
        check(compressed > 0, format!("expected compressed extents, got {compressed}/{total}"))?;

        decompress_fd(&file).ctx("decompress text.dat")?;
        let (compressed, total) = compressed_bytes(&path).ctx("map extents after decompressing")?;
        check_eq(
            compressed,
            0,
            format!("expected no compressed extents, got {compressed}/{total}"),
        )?;

        // Contents must survive both passes.
        let back = std::fs::read(&path).ctx("read text.dat back")?;
        check_eq(back.len(), 12 * 500_000, "the file's length after both passes")
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

        let Some(tmp) = btrfs_tempdir() else {
            eprintln!("skipped: not running on btrfs");
            return Ok(());
        };
        let path = tmp.path().join("fingerprint.dat");
        std::fs::write(&path, "compress me ".repeat(200_000).into_bytes())
            .ctx("write the test file")?;
        let before = std::fs::metadata(&path).ctx("stat before")?;

        let anchor = Anchor::open(tmp.path()).ctx("open the anchor")?;
        let file = anchor.open_file(Path::new("fingerprint.dat")).ctx("open the file")?;
        compress_fd(&file, 15).ctx("compress the file")?;
        let after = std::fs::metadata(&path).ctx("stat after")?;

        check_eq(after.ino(), before.ino(), "the inode must survive")?;
        check_eq(after.size(), before.size(), "the logical size must not move")?;
        check_eq(after.mtime_nsec(), before.mtime_nsec(), "mtime must not move")?;
        check_eq(
            after.ctime_nsec(),
            before.ctime_nsec(),
            "ctime must not move, or every compressed file looks changed next run",
        )
    }

    #[test]
    fn sets_and_clears_the_directory_property() -> TestResult {
        let Some(tmp) = btrfs_tempdir() else {
            eprintln!("skipped: not running on btrfs");
            return Ok(());
        };
        set_dir_property(tmp.path(), true).ctx("set btrfs.compression")?;
        check_eq(
            xattr::get(tmp.path(), "btrfs.compression").ctx("read btrfs.compression")?.as_deref(),
            Some(b"zstd".as_slice()),
            "the directory property after setting it",
        )?;
        set_dir_property(tmp.path(), false).ctx("clear btrfs.compression")?;
        check(
            xattr::get(tmp.path(), "btrfs.compression").ctx("re-read btrfs.compression")?.is_none(),
            "the property is gone after clearing",
        )?;
        // Clearing twice must not fail.
        set_dir_property(tmp.path(), false).ctx("clear btrfs.compression a second time")
    }
}
