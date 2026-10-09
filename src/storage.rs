//! Read-only storage planning shared by desktop clients and workers.
#![allow(unsafe_code)]

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Size of a folder tree as storage planning counts it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Footprint {
    /// Regular files.
    pub files: u64,
    /// Sum of their apparent sizes.
    pub bytes: u64,
    /// Size of the largest file.
    pub largest: u64,
    /// Allowance for store metadata: per entry, 16 KiB plus eight times its
    /// path length and, on Linux, eight times the size of its xattrs.
    pub metadata_bytes: u64,
}

/// Walks `root` without following symlinks and totals its footprint.
pub fn inventory(root: &Path) -> Result<Footprint> {
    ensure!(
        root.is_dir(),
        "{} is not available. Check that its drive is connected.",
        root.display()
    );
    let mut result = Footprint::default();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let path_bytes = entry.path().as_os_str().as_encoded_bytes().len() as u64;
        result.metadata_bytes = result
            .metadata_bytes
            .checked_add(path_bytes.checked_mul(8).context("Path size overflow")?)
            .and_then(|value| value.checked_add(16384))
            .context("Metadata size overflow")?;
        #[cfg(target_os = "linux")]
        for name in xattr::list(entry.path())? {
            let bytes =
                xattr::get(entry.path(), &name)?.context("Metadata changed during planning")?;
            let attribute_size = (bytes.len() as u64)
                .checked_add(name.as_encoded_bytes().len() as u64)
                .and_then(|value| value.checked_mul(8))
                .context("Attribute size overflow")?;
            result.metadata_bytes = result
                .metadata_bytes
                .checked_add(attribute_size)
                .context("Metadata size overflow")?;
        }
        if entry.file_type().is_file() {
            let length = entry.metadata()?.len();
            result.files = result.files.checked_add(1).context("Too many files")?;
            result.bytes = result
                .bytes
                .checked_add(length)
                .context("The folder is too large to measure")?;
            result.largest = result.largest.max(length);
        }
    }
    Ok(result)
}

/// One filesystem and its free space at the time of the reading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    /// Stable name for the filesystem. Two paths on the same volume give the
    /// same identity.
    pub identity: String,
    /// The mount point.
    #[serde(with = "crate::path_serde")]
    pub path: PathBuf,
    /// Free bytes available to this user.
    pub available: u64,
}

/// The volume holding `path`, or its nearest existing ancestor when `path`
/// does not exist yet. On Linux and macOS a read-only volume is an error.
pub fn volume(path: &Path) -> Result<Volume> {
    let existing = path
        .ancestors()
        .find(|path| path.exists())
        .context("The drive is not available. Check that it is connected.")?
        .canonicalize()?;
    volume_existing(&existing)
}

/// Identity is the filesystem type plus its UUID from `/dev/disk/by-uuid`.
/// Without a UUID it falls back to the mount source and filesystem id.
#[cfg(target_os = "linux")]
fn volume_existing(path: &Path) -> Result<Volume> {
    let fs = crate::fsprobe::probe(path)?;
    if fs.is_pack_mount() {
        // Managed mounts receive a new filesystem ID after each remount.
        // Library identity and capacity belong to the underlying drive.
        let parent = fs
            .mountpoint
            .parent()
            .context("Managed game mount has no parent folder")?;
        return volume_existing(parent);
    }
    ensure!(
        !fs.read_only,
        "The drive is read-only, so Flummox cannot change it."
    );
    Ok(Volume {
        identity: linux_identity(&fs, path, Path::new("/dev/disk/by-uuid"))?,
        path: fs.mountpoint,
        available: crate::backend::free_bytes(path)?,
    })
}

/// A btrfs asks the filesystem for its UUID, because `statfs` ids differ
/// between subvolumes of one filesystem. Others look their device up in
/// `by_uuid`, and fall back to the source and `statfs` id.
#[cfg(target_os = "linux")]
fn linux_identity(fs: &crate::fsprobe::FsInfo, path: &Path, by_uuid: &Path) -> Result<String> {
    let own = (fs.fstype == "btrfs")
        .then(|| crate::backend::btrfs::filesystem_uuid(path).ok())
        .flatten();
    let uuid = match own {
        Some(uuid) => Some(uuid),
        None => {
            let source = Path::new(&fs.source).canonicalize().ok();
            std::fs::read_dir(by_uuid).ok().and_then(|entries| {
                entries.flatten().find_map(|entry| {
                    (source.is_some() && entry.path().canonicalize().ok() == source)
                        .then(|| entry.file_name().to_string_lossy().into_owned())
                })
            })
        }
    };
    let uuid = match uuid {
        Some(uuid) => uuid,
        None => format!(
            "{}:{:?}",
            fs.source,
            nix::sys::statfs::statfs(path)?.filesystem_id()
        ),
    };
    Ok(format!("{}:{uuid}", fs.fstype))
}

#[cfg(target_os = "macos")]
fn volume_existing(path: &Path) -> Result<Volume> {
    use std::{ffi::CString, mem::MaybeUninit, os::unix::ffi::OsStrExt};
    let path_c = CString::new(path.as_os_str().as_bytes())?;
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path_c is terminated and stat has writable space for statfs.
    let result = unsafe { libc::statfs(path_c.as_ptr(), stat.as_mut_ptr()) };
    ensure!(
        result == 0,
        "Could not read the drive: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: a successful statfs initialized the whole structure.
    let stat = unsafe { stat.assume_init() };
    ensure!(
        stat.f_flags & libc::MNT_RDONLY as u32 == 0,
        "The drive is read-only, so Flummox cannot change it."
    );
    let mount: Vec<u8> = stat
        .f_mntonname
        .iter()
        .take_while(|value| **value != 0)
        .map(|value| *value as u8)
        .collect();
    let kind: Vec<u8> = stat
        .f_fstypename
        .iter()
        .take_while(|value| **value != 0)
        .map(|value| *value as u8)
        .collect();
    use std::os::unix::ffi::OsStringExt;
    let mount = PathBuf::from(std::ffi::OsString::from_vec(mount));
    let mount_c = CString::new(mount.as_os_str().as_bytes())?;
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    #[repr(C)]
    struct VolumeUuid {
        length: u32,
        bytes: [u8; 16],
    }
    let mut uuid = VolumeUuid {
        length: 0,
        bytes: [0; 16],
    };
    // SAFETY: mount_c is terminated, attributes describes the fixed UUID output,
    // and uuid has room for the returned length prefix and 16-byte UUID.
    let result = unsafe {
        libc::getattrlist(
            mount_c.as_ptr(),
            std::ptr::from_mut(&mut attributes).cast(),
            std::ptr::from_mut(&mut uuid).cast(),
            std::mem::size_of::<VolumeUuid>(),
            0,
        )
    };
    ensure!(
        result == 0 && uuid.length as usize == std::mem::size_of::<VolumeUuid>(),
        "Could not identify the drive: {}",
        std::io::Error::last_os_error()
    );
    let uuid: String = uuid
        .bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(Volume {
        identity: format!("{}:{}", String::from_utf8(kind)?, uuid),
        path: mount,
        available: stat.f_bavail.saturating_mul(stat.f_bsize as u64),
    })
}

#[cfg(windows)]
fn volume_existing(path: &Path) -> Result<Volume> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut mount = vec![0u16; 32768];
    // SAFETY: path is terminated and mount has the supplied writable capacity.
    let result = unsafe {
        GetVolumePathNameW(
            path.as_ptr(),
            mount.as_mut_ptr(),
            u32::try_from(mount.len())?,
        )
    };
    ensure!(result != 0, "Could not find the drive");
    let mut name = vec![0u16; 128];
    // SAFETY: mount is terminated by GetVolumePathNameW and name has the supplied capacity.
    let result = unsafe {
        GetVolumeNameForVolumeMountPointW(
            mount.as_ptr(),
            name.as_mut_ptr(),
            u32::try_from(name.len())?,
        )
    };
    ensure!(result != 0, "Could not identify the drive");
    let mut available = 0u64;
    // SAFETY: mount is terminated, available is writable, and unused outputs are null.
    let result = unsafe {
        GetDiskFreeSpaceExW(
            mount.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    ensure!(result != 0, "Cannot inspect free space");
    Ok(Volume {
        identity: String::from_utf16(
            &name
                .into_iter()
                .take_while(|value| *value != 0)
                .collect::<Vec<_>>(),
        )?,
        path: PathBuf::from(std::ffi::OsString::from_wide(
            &mount
                .into_iter()
                .take_while(|value| *value != 0)
                .collect::<Vec<_>>(),
        )),
        available,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn volume_existing(_path: &Path) -> Result<Volume> {
    anyhow::bail!("Checking free space is not available on this platform.")
}

/// Free space one volume must have before a job starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    pub volume: Volume,
    /// Bytes the job may write to this volume, summed over every reason.
    pub additional: u64,
    /// Margin on top: 5 percent of `additional`, at least 64 MiB.
    pub headroom: u64,
    /// One line per allocation counted into `additional`.
    pub reasons: Vec<String>,
}

/// The free-space requirements of one job, one row per volume.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpacePlan {
    pub requirements: Vec<Requirement>,
    /// The job leaves the original files on disk beside the new copy.
    pub retained_original: bool,
}

impl SpacePlan {
    /// Adds `bytes` to the requirement for `volume`, merging with an existing
    /// row for the same identity and keeping the lower free-space reading.
    pub fn add(&mut self, volume: Volume, bytes: u64, reason: &str) -> Result<()> {
        if let Some(existing) = self
            .requirements
            .iter_mut()
            .find(|row| row.volume.identity == volume.identity)
        {
            existing.additional = existing
                .additional
                .checked_add(bytes)
                .context("Space requirement overflow")?;
            existing.volume.available = existing.volume.available.min(volume.available);
            existing.headroom = (existing.additional / 20).max(64 * 1024 * 1024);
            existing.reasons.push(reason.into());
        } else {
            self.requirements.push(Requirement {
                volume,
                additional: bytes,
                headroom: (bytes / 20).max(64 * 1024 * 1024),
                reasons: vec![reason.into()],
            });
        }
        Ok(())
    }

    /// Fails when any volume's recorded free space is below its requirement
    /// plus headroom. Uses the readings stored in the plan.
    pub fn check(&self) -> Result<()> {
        for requirement in &self.requirements {
            let needed = requirement
                .additional
                .checked_add(requirement.headroom)
                .context("Space requirement overflow")?;
            ensure!(
                requirement.volume.available >= needed,
                "Not enough space on {}: {} needed including a safety margin, {} available. Free up space or choose another drive.",
                requirement.volume.path.display(),
                humansize::format_size(needed, humansize::DECIMAL),
                humansize::format_size(requirement.volume.available, humansize::DECIMAL)
            );
        }
        Ok(())
    }

    /// Reads each volume's free space again and checks the plan against it.
    /// Fails when a mount point now belongs to a different volume.
    pub fn recheck(&self) -> Result<()> {
        let mut current = self.clone();
        for row in &mut current.requirements {
            let volume = volume(&row.volume.path)?;
            ensure!(
                volume.identity == row.volume.identity,
                "The drive changed since the storage plan was made. Review the storage plan again."
            );
            row.volume = volume;
        }
        current.check()
    }
}

/// Plan for a native compress or, with `restore`, decompress of `root`: the
/// whole install plus its largest file, on the install's own volume.
pub fn native_plan(root: &Path, restore: bool) -> Result<SpacePlan> {
    let footprint = inventory(root)?;
    let mut plan = SpacePlan::default();
    // Snapshots and shared extents can pin old blocks throughout a rewrite.
    let additional = footprint
        .bytes
        .checked_add(footprint.largest)
        .context("Native space requirement overflow")?;
    plan.add(
        volume(root)?,
        additional,
        if restore {
            "The decompressed files and one temporary file, in case snapshots keep the old version"
        } else {
            "The whole game and its largest file, in case snapshots keep the old version"
        },
    )?;
    Ok(plan)
}

/// Plan for a backend that rewrites one file at a time and keeps no copy of the
/// rest: Windows compression and macOS compressed files.
///
/// Compressing needs room for the largest file while it is rewritten. A
/// restore needs room for the whole install, since every file grows back. Use
/// [`native_plan`] for btrfs, where snapshots can pin the old blocks.
pub fn per_file_plan(root: &Path, restore: bool) -> Result<SpacePlan> {
    let footprint = inventory(root)?;
    let mut plan = SpacePlan::default();
    let (additional, reason) = per_file_requirement(&footprint, restore);
    plan.add(volume(root)?, additional, reason)?;
    Ok(plan)
}

/// The bytes and the reason behind [`per_file_plan`].
fn per_file_requirement(footprint: &Footprint, restore: bool) -> (u64, &'static str) {
    if restore {
        (
            footprint.bytes,
            "Every file returns to its full size when decompressed",
        )
    } else {
        (
            footprint.largest,
            "Room for the largest file while it is rewritten",
        )
    }
}

/// Upper bound on a new store's size: the content, plus 1/32 of it, plus the
/// metadata allowance, plus 16 MiB.
pub fn pack_bound(footprint: &Footprint) -> Result<u64> {
    footprint
        .bytes
        .checked_add(footprint.bytes / 32)
        .and_then(|value| value.checked_add(footprint.metadata_bytes))
        .and_then(|value| value.checked_add(16 * 1024 * 1024))
        .context("Store size bound overflow")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    #[test]
    fn shared_volume_requirements_accumulate_and_block() -> TestResult {
        let mut plan = SpacePlan::default();
        let volume = Volume {
            identity: "fixture".into(),
            path: "/fixture".into(),
            available: 80 * 1024 * 1024,
        };
        plan.add(volume.clone(), 10 * 1024 * 1024, "store")
            .ctx("store requirement")?;
        plan.add(volume, 10 * 1024 * 1024, "updates")
            .ctx("update requirement")?;
        check_eq(plan.requirements.len(), 1, "same volume")?;
        check(
            plan.check().is_err(),
            "shared volume must include both allocations and headroom",
        )
    }
    #[test]
    fn per_file_plans_need_the_largest_file_or_the_whole_install() -> TestResult {
        let footprint = Footprint {
            files: 3,
            bytes: 100,
            largest: 60,
            metadata_bytes: 0,
        };
        check_eq(per_file_requirement(&footprint, false).0, 60, "compress")?;
        check_eq(per_file_requirement(&footprint, true).0, 100, "restore")?;
        let root = tempfile::tempdir().ctx("fixture")?;
        std::fs::write(root.path().join("big"), vec![0u8; 4096]).ctx("big")?;
        std::fs::write(root.path().join("small"), vec![0u8; 16]).ctx("small")?;
        let plan = per_file_plan(root.path(), false).ctx("per-file plan")?;
        let native = native_plan(root.path(), false).ctx("native plan")?;
        let wanted = |plan: &SpacePlan| plan.requirements.first().map(|row| row.additional);
        check_eq(wanted(&plan), Some(4096), "the largest file only")?;
        check_eq(
            wanted(&native),
            Some(4096 + 16 + 4096),
            "control: the native plan still wants the whole install and more",
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn subvolumes_of_one_btrfs_are_one_volume_without_by_uuid() -> TestResult {
        let here = std::env::current_dir().ctx("working directory")?;
        let tmp = tempfile::TempDir::new_in(here).ctx("temp directory")?;
        let fs = crate::fsprobe::probe(tmp.path()).ctx("probe")?;
        if fs.fstype != "btrfs" {
            check(
                std::env::var_os("FLUMMOX_REQUIRE_BTRFS").is_none(),
                "the test directory is not btrfs, and FLUMMOX_REQUIRE_BTRFS is set",
            )?;
            eprintln!("skipped: subvolume identity requires btrfs");
            return Ok(());
        }
        let mut paths = Vec::new();
        for name in ["a", "b"] {
            let path = tmp.path().join(name);
            let made = std::process::Command::new("btrfs")
                .args(["subvolume", "create"])
                .arg(&path)
                .output()
                .ctx("run btrfs subvolume create")?;
            check(
                made.status.success(),
                format!(
                    "btrfs subvolume create: {}",
                    String::from_utf8_lossy(&made.stderr)
                ),
            )?;
            paths.push(path);
        }
        let ids: Vec<_> = paths
            .iter()
            .map(|path| nix::sys::statfs::statfs(path).map(|s| format!("{:?}", s.filesystem_id())))
            .collect::<Result<_, _>>()
            .ctx("statfs")?;
        check(
            ids.first() != ids.get(1),
            "control: statfs gives the two subvolumes different ids",
        )?;
        let missing = tmp.path().join("no-by-uuid");
        let identity = |path: &Path| -> Result<String, String> {
            let fs = crate::fsprobe::probe(path).ctx("probe a subvolume")?;
            linux_identity(&fs, path, &missing).ctx("identity")
        };
        let first = identity(paths.first().ctx("first subvolume")?)?;
        let second = identity(paths.get(1).ctx("second subvolume")?)?;
        check_eq(first.clone(), second, "one filesystem, one identity")?;
        let uuid = first.strip_prefix("btrfs:").ctx("a btrfs identity")?;
        check(
            Path::new("/sys/fs/btrfs").join(uuid).is_dir(),
            format!("{uuid} is the UUID sysfs knows"),
        )
    }

    #[test]
    fn inventory_does_not_follow_external_symlinks() -> TestResult {
        let root = tempfile::tempdir().ctx("fixture")?;
        std::fs::write(root.path().join("data"), b"fixture").ctx("write")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink("/", root.path().join("outside")).ctx("symlink")?;
        let footprint = inventory(root.path()).ctx("inventory")?;
        check_eq(footprint.bytes, 7, "external files excluded")
    }
}
