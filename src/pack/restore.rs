//! Restores into a private staging tree before an atomic, no-replace rename.

use super::{Reader, Summary, format::Kind};
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsStr,
    fs::{File, FileTimes, OpenOptions, Permissions},
    io::{Seek, SeekFrom, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{PermissionsExt, symlink},
    },
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) fn modified(seconds: i64, nanos: u32) -> Result<SystemTime> {
    let duration = Duration::from_secs(seconds.unsigned_abs());
    let base = if seconds < 0 {
        UNIX_EPOCH.checked_sub(duration)
    } else {
        UNIX_EPOCH.checked_add(duration)
    };
    base.and_then(|t| t.checked_add(Duration::from_nanos(u64::from(nanos))))
        .context("Timestamp out of range")
}

fn timestamp(entry: &super::Entry) -> Result<SystemTime> {
    modified(entry.modified_secs, entry.modified_nanos)
}

pub(super) fn apply_xattrs(path: &Path, attributes: &[super::format::Xattr]) -> Result<()> {
    for attribute in attributes {
        xattr::set(path, OsStr::from_bytes(&attribute.name), &attribute.value)?;
    }
    Ok(())
}

pub(super) fn copy_xattrs(source: &Path, destination: &Path) -> Result<()> {
    for name in xattr::list(source)? {
        let value = xattr::get(source, &name)?.context("Extended attribute disappeared")?;
        xattr::set(destination, &name, &value)?;
    }
    Ok(())
}

/// Restores a verified store to a new directory, preserving file modes and times.
/// Symlinks are created after file writes; source stores and existing trees survive.
pub fn restore(store: &Path, destination: &Path, cancel: &AtomicBool) -> Result<Summary> {
    let reader = Reader::open(store)?;
    let (parent, target) = super::create::destination(destination)?;
    let staged = tempfile::Builder::new()
        .prefix(".flummox-restore-")
        .tempdir_in(&parent)?;
    for entry in reader.entries() {
        ensure!(!cancel.load(Ordering::Relaxed), "Restore cancelled");
        let path = staged.path().join(&entry.path);
        match &entry.kind {
            Kind::Directory if !entry.path.as_os_str().is_empty() => {
                std::fs::create_dir(&path)?;
            }
            Kind::File { size, chunks } if entry.hardlink_to.is_none() => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                for id in chunks {
                    ensure!(!cancel.load(Ordering::Relaxed), "Restore cancelled");
                    if let Some(length) = reader.zero_chunk_len(*id)? {
                        file.seek(SeekFrom::Current(i64::from(length)))?;
                    } else {
                        file.write_all(reader.chunk(*id)?.as_slice())?;
                    }
                }
                file.set_len(*size)?;
                ensure!(file.metadata()?.len() == *size, "Restored length mismatch");
                file.set_times(FileTimes::new().set_modified(timestamp(entry)?))?;
                file.set_permissions(Permissions::from_mode(entry.mode))?;
                apply_xattrs(&path, &entry.xattrs)?;
                file.sync_all()?;
            }
            Kind::SlicedFile { size, .. } if entry.hardlink_to.is_none() => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                file.write_all(&reader.read(&entry.path, 0, usize::try_from(*size)?)?)?;
                file.set_times(FileTimes::new().set_modified(timestamp(entry)?))?;
                file.set_permissions(Permissions::from_mode(entry.mode))?;
                apply_xattrs(&path, &entry.xattrs)?;
                file.sync_all()?;
            }
            _ => {}
        }
    }
    for entry in reader.entries() {
        if let Some(target) = &entry.hardlink_to {
            std::fs::hard_link(staged.path().join(target), staged.path().join(&entry.path))?;
        }
    }
    for entry in reader.entries() {
        if let Kind::Symlink { target } = &entry.kind {
            symlink(target, staged.path().join(&entry.path))?;
        }
    }
    for entry in reader.entries().iter().rev() {
        if matches!(entry.kind, Kind::Directory) {
            let directory = File::open(staged.path().join(&entry.path))?;
            directory.set_times(FileTimes::new().set_modified(timestamp(entry)?))?;
            directory.set_permissions(Permissions::from_mode(entry.mode))?;
            apply_xattrs(&staged.path().join(&entry.path), &entry.xattrs)?;
            directory.sync_all()?;
        }
    }
    ensure!(!cancel.load(Ordering::Relaxed), "Restore cancelled");
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        staged.path(),
        rustix::fs::CWD,
        &target,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    File::open(parent)?.sync_all()?;
    Ok(reader.summary().clone())
}
