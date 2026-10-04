//! Read-only storage planning shared by desktop clients and workers.
#![allow(unsafe_code)]

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Footprint {
    pub files: u64,
    pub bytes: u64,
    pub largest: u64,
    pub metadata_bytes: u64,
}

pub fn inventory(root: &Path) -> Result<Footprint> {
    ensure!(
        root.is_dir(),
        "Storage location is unavailable: {}",
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
                .context("Folder is too large")?;
            result.largest = result.largest.max(length);
        }
    }
    Ok(result)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    pub identity: String,
    #[serde(with = "crate::path_serde")]
    pub path: PathBuf,
    pub available: u64,
}

pub fn volume(path: &Path) -> Result<Volume> {
    let existing = path
        .ancestors()
        .find(|path| path.exists())
        .context("Storage drive is unavailable")?
        .canonicalize()?;
    volume_existing(&existing)
}

#[cfg(target_os = "linux")]
fn volume_existing(path: &Path) -> Result<Volume> {
    let fs = crate::fsprobe::probe(path)?;
    if fs.magic == crate::fsprobe::magic::FUSE && fs.source == "flummox-pack" {
        // Managed mounts receive a new filesystem ID after each remount.
        // Library identity and capacity belong to the underlying drive.
        let parent = fs
            .mountpoint
            .parent()
            .context("Managed game mount has no parent folder")?;
        return volume_existing(parent);
    }
    ensure!(!fs.read_only, "Storage volume is read-only");
    let source = Path::new(&fs.source).canonicalize().ok();
    let uuid = std::fs::read_dir("/dev/disk/by-uuid")
        .ok()
        .and_then(|entries| {
            entries.flatten().find_map(|entry| {
                (source.is_some() && entry.path().canonicalize().ok() == source)
                    .then(|| entry.file_name().to_string_lossy().into_owned())
            })
        });
    let fallback = format!(
        "{}:{:?}",
        fs.source,
        nix::sys::statfs::statfs(path)?.filesystem_id()
    );
    Ok(Volume {
        identity: format!("{}:{}", fs.fstype, uuid.unwrap_or(fallback)),
        path: fs.mountpoint,
        available: crate::backend::free_bytes(path)?,
    })
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
        "Cannot inspect storage volume: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: a successful statfs initialized the whole structure.
    let stat = unsafe { stat.assume_init() };
    ensure!(
        stat.f_flags & libc::MNT_RDONLY as u32 == 0,
        "Storage volume is read-only"
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
        "Cannot identify the storage volume: {}",
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
    ensure!(result != 0, "Cannot locate storage volume");
    let mut name = vec![0u16; 128];
    // SAFETY: mount is terminated by GetVolumePathNameW and name has the supplied capacity.
    let result = unsafe {
        GetVolumeNameForVolumeMountPointW(
            mount.as_ptr(),
            name.as_mut_ptr(),
            u32::try_from(name.len())?,
        )
    };
    ensure!(result != 0, "Cannot identify storage volume");
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
    anyhow::bail!("Storage planning is unavailable on this platform")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    pub volume: Volume,
    pub additional: u64,
    pub headroom: u64,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpacePlan {
    pub requirements: Vec<Requirement>,
    pub retained_original: bool,
}

impl SpacePlan {
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

    pub fn check(&self) -> Result<()> {
        for requirement in &self.requirements {
            let needed = requirement
                .additional
                .checked_add(requirement.headroom)
                .context("Space requirement overflow")?;
            ensure!(
                requirement.volume.available >= needed,
                "Not enough space on {}: need {} additional bytes including safety headroom; {} available",
                requirement.volume.path.display(),
                needed,
                requirement.volume.available
            );
        }
        Ok(())
    }

    pub fn recheck(&self) -> Result<()> {
        let mut current = self.clone();
        for row in &mut current.requirements {
            let volume = volume(&row.volume.path)?;
            ensure!(
                volume.identity == row.volume.identity,
                "Storage volume changed; review the space plan again"
            );
            row.volume = volume;
        }
        current.check()
    }
}

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
            "Ordinary expansion and temporary file; old blocks may remain pinned"
        } else {
            "Worst-case native rewrite with snapshots or shared extents retaining old blocks"
        },
    )?;
    Ok(plan)
}

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
    fn inventory_does_not_follow_external_symlinks() -> TestResult {
        let root = tempfile::tempdir().ctx("fixture")?;
        std::fs::write(root.path().join("data"), b"fixture").ctx("write")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink("/", root.path().join("outside")).ctx("symlink")?;
        let footprint = inventory(root.path()).ctx("inventory")?;
        check_eq(footprint.bytes, 7, "external files excluded")
    }
}
