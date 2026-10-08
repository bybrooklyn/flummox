//! Allocated storage for files, as the filesystem accounts for it.
//!
//! Qualification reports compare allocated bytes before and after compression.
//! A file's length says nothing about either, so this reads block counts and
//! refuses where the block count is known not to be storage.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;

/// What a set of files occupies. `allocated_bytes` is storage, not length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Allocation {
    pub files: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
}

/// Sums the regular files at and under each root.
///
/// A file reached twice, through a hard link or an overlapping root, counts
/// once. Fails for a mounted Maximum Space view and for btrfs files holding
/// compressed extents, because the kernel reports uncompressed blocks there.
pub fn measure(roots: &[PathBuf]) -> Result<Allocation> {
    let mut canonical = Vec::with_capacity(roots.len());
    for root in roots {
        canonical.push(
            root.canonicalize()
                .with_context(|| format!("Finding {}", root.display()))?,
        );
    }
    platform::measure(&canonical)
}

#[cfg(unix)]
mod platform {
    use super::Allocation;
    use anyhow::{Context, Result, ensure};
    use std::{collections::HashSet, os::unix::fs::MetadataExt, path::PathBuf};

    pub fn measure(roots: &[PathBuf]) -> Result<Allocation> {
        let mut seen = HashSet::new();
        let mut total = Allocation::default();
        let mut encoded = 0u64;
        for root in roots {
            let extents = reports_encoded_extents(root)?;
            for entry in walkdir::WalkDir::new(root).follow_links(false) {
                let entry = entry.with_context(|| format!("Reading {}", root.display()))?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let stat = entry
                    .metadata()
                    .with_context(|| format!("Reading {}", entry.path().display()))?;
                if !seen.insert((stat.dev(), stat.ino())) {
                    continue;
                }
                if extents && holds_encoded_extents(entry.path())? {
                    encoded += 1;
                }
                total.files += 1;
                total.logical_bytes = total.logical_bytes.saturating_add(stat.len());
                total.allocated_bytes = total
                    .allocated_bytes
                    .saturating_add(stat.blocks().saturating_mul(512));
            }
        }
        ensure!(
            encoded == 0,
            "{encoded} of {} files hold btrfs-compressed extents, and the kernel reports their \
             uncompressed size. Decompress the copy first, or enter Disk Usage from \
             `sudo compsize -b`",
            total.files
        );
        Ok(total)
    }

    /// Whether files under `root` need an extent check before their block
    /// counts can be used.
    #[cfg(target_os = "linux")]
    fn reports_encoded_extents(root: &std::path::Path) -> Result<bool> {
        use crate::fsprobe::{magic, statfs_magic};
        let found = statfs_magic(root)
            .with_context(|| format!("Reading the filesystem of {}", root.display()))?;
        ensure!(
            found != magic::FUSE,
            "{} is a mounted view, so its block counts describe the view. Measure the store and \
             the update layer",
            root.display()
        );
        Ok(found == magic::BTRFS)
    }

    #[cfg(not(target_os = "linux"))]
    fn reports_encoded_extents(_: &std::path::Path) -> Result<bool> {
        Ok(false)
    }

    #[cfg(target_os = "linux")]
    fn holds_encoded_extents(path: &std::path::Path) -> Result<bool> {
        let (compressed, _) = crate::backend::btrfs::compressed_bytes(path)
            .with_context(|| format!("Reading extents of {}", path.display()))?;
        Ok(compressed > 0)
    }

    #[cfg(not(target_os = "linux"))]
    fn holds_encoded_extents(_: &std::path::Path) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(windows)]
mod platform {
    use super::Allocation;
    use anyhow::{Context, Result};
    use std::path::PathBuf;

    // Hard links are not folded together here, so a linked file counts once
    // for each name under the roots.
    pub fn measure(roots: &[PathBuf]) -> Result<Allocation> {
        let mut total = Allocation::default();
        for root in roots {
            for entry in walkdir::WalkDir::new(root).follow_links(false) {
                let entry = entry.with_context(|| format!("Reading {}", root.display()))?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let length = entry
                    .metadata()
                    .with_context(|| format!("Reading {}", entry.path().display()))?
                    .len();
                total.files += 1;
                total.logical_bytes = total.logical_bytes.saturating_add(length);
                total.allocated_bytes = total
                    .allocated_bytes
                    .saturating_add(crate::windows::allocation_size(entry.path())?);
            }
        }
        Ok(total)
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    pub fn measure(_: &[std::path::PathBuf]) -> anyhow::Result<super::Allocation> {
        anyhow::bail!("Allocated storage cannot be measured on this platform")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    use std::{fs::File, io::Write, path::Path};

    const MIB: u64 = 1024 * 1024;

    /// Bytes no filesystem compresses, written through to storage.
    fn incompressible(path: &Path, length: usize) -> TestResult {
        let mut bytes = vec![0u8; length];
        blake3::Hasher::new()
            .update(path.as_os_str().as_encoded_bytes())
            .finalize_xof()
            .fill(&mut bytes);
        let mut file = File::create(path).ctx("fixture file")?;
        file.write_all(&bytes).ctx("fixture bytes")?;
        file.sync_all().ctx("fixture sync")
    }

    #[test]
    fn a_sparse_file_adds_length_and_no_storage() -> TestResult {
        let tmp = tempfile::tempdir().ctx("temp directory")?;
        incompressible(&tmp.path().join("data.bin"), MIB as usize)?;
        let dense = measure(&[tmp.path().to_path_buf()]).ctx("dense measurement")?;
        check_eq(dense.files, 1, "one file")?;
        check_eq(dense.logical_bytes, MIB, "logical bytes")?;
        check(
            dense.allocated_bytes >= MIB,
            format!("written bytes occupy storage: {dense:?}"),
        )?;

        let hole = File::create(tmp.path().join("hole.bin")).ctx("sparse file")?;
        hole.set_len(64 * MIB).ctx("sparse length")?;
        hole.sync_all().ctx("sparse sync")?;
        let sparse = measure(&[tmp.path().to_path_buf()]).ctx("sparse measurement")?;
        check_eq(sparse.files, 2, "two files")?;
        check_eq(sparse.logical_bytes, 65 * MIB, "length includes the hole")?;
        check_eq(
            sparse.allocated_bytes,
            dense.allocated_bytes,
            "a hole occupies nothing",
        )
    }

    #[test]
    fn a_file_reached_twice_counts_once() -> TestResult {
        let tmp = tempfile::tempdir().ctx("temp directory")?;
        let store = tmp.path().join("store");
        let other = tmp.path().join("other");
        std::fs::create_dir(&store).ctx("store")?;
        std::fs::create_dir(&other).ctx("other")?;
        incompressible(&store.join("object"), MIB as usize)?;
        let single = measure(std::slice::from_ref(&store)).ctx("single")?;
        std::fs::hard_link(store.join("object"), other.join("link")).ctx("hard link")?;
        let both = measure(&[store.clone(), other, store.join("object")]).ctx("both")?;
        check_eq(both, single, "links and repeated roots add nothing")?;
        check(
            measure(&[tmp.path().join("missing")]).is_err(),
            "a missing root is an error",
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn btrfs_compressed_extents_are_refused() -> TestResult {
        use crate::backend::btrfs::{DEFAULT_LEVEL, compress_fd, decompress_fd};
        let here = std::env::current_dir().ctx("working directory")?;
        let tmp = tempfile::TempDir::new_in(here).ctx("temp directory")?;
        if crate::fsprobe::probe(tmp.path()).ctx("probe")?.fstype != "btrfs" {
            eprintln!("skipped: the extent refusal requires btrfs");
            return Ok(());
        }
        let path = tmp.path().join("zeros.bin");
        std::fs::write(&path, vec![b'A'; 4 * MIB as usize]).ctx("fixture")?;
        let file = File::open(&path).ctx("fixture handle")?;
        compress_fd(&file, DEFAULT_LEVEL).ctx("compress")?;
        file.sync_all().ctx("sync")?;
        let refused = measure(&[tmp.path().to_path_buf()]);
        let message = refused.err().map(|error| error.to_string());
        check(
            message
                .as_deref()
                .is_some_and(|text| text.contains("1 of 1 files hold btrfs-compressed")),
            format!("compressed extents must be refused: {message:?}"),
        )?;
        decompress_fd(&file).ctx("decompress")?;
        file.sync_all().ctx("sync")?;
        let ordinary = measure(&[tmp.path().to_path_buf()]).ctx("ordinary measurement")?;
        check(
            ordinary.allocated_bytes >= 4 * MIB,
            format!("ordinary extents are measured: {ordinary:?}"),
        )
    }
}
