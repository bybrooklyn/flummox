//! Verifies an interrupted restoration against the retained store and update layer.
use super::{Install, Observer};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

// Per path: full st_mode, symlink target, sorted xattrs, and for a multiply
// linked file the first path in sorted order that shares its inode. The
// tuple holds no times and no ownership, so those are not compared.
type Metadata = (
    u32,
    Option<PathBuf>,
    Vec<(Vec<u8>, Vec<u8>)>,
    Option<PathBuf>,
);
// Walks `root` and collects the comparable metadata of every path in it.
fn metadata(root: &Path, observer: &dyn Observer) -> Result<BTreeMap<PathBuf, Metadata>> {
    let mut result = BTreeMap::new();
    let mut links = BTreeMap::new();
    let mut paths = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    paths.sort_by_key(|entry| entry.path().to_path_buf());
    for entry in paths {
        observer.checkpoint()?;
        let path = entry.path();
        let relative = path.strip_prefix(root)?.to_path_buf();
        let stat = std::fs::symlink_metadata(path)?;
        let mut attributes = vec![];
        for name in xattr::list(path)? {
            attributes.push((
                name.as_bytes().to_vec(),
                xattr::get(path, &name)?.context("Metadata changed during verification")?,
            ));
        }
        attributes.sort();
        let target = if stat.is_symlink() {
            Some(std::fs::read_link(path)?)
        } else {
            None
        };
        let link = if stat.is_file() && stat.nlink() > 1 {
            Some(
                links
                    .entry((stat.dev(), stat.ino()))
                    .or_insert_with(|| relative.clone())
                    .clone(),
            )
        } else {
            None
        };
        result.insert(relative, (stat.mode(), target, attributes, link));
    }
    Ok(result)
}

/// Checks that the ordinary files at the game path equal the store with its
/// update layer applied. Requires `Attention` or `Restoring`, an unmounted
/// game path and no process using it. Rebuilds the expected tree in a
/// temporary sibling folder, so it needs space for a full copy. Deletes
/// nothing but that temporary folder, whatever the outcome.
pub fn verify_restored(
    install: &Install,
    cancel: &AtomicBool,
    observer: &dyn Observer,
) -> Result<()> {
    ensure!(
        matches!(
            install.phase,
            super::InstallPhase::Attention | super::InstallPhase::Restoring
        ),
        "This install does not need restoration verification"
    );
    let fs = crate::fsprobe::probe(&install.game_path)?;
    ensure!(
        fs.magic != crate::fsprobe::magic::FUSE,
        "Unmount the store before verifying ordinary files"
    );
    ensure!(
        crate::busy::process_using(&install.game_path, &crate::busy::ProcFs::new()).is_none(),
        "Close game and launcher activity before verification"
    );
    let reader = super::Reader::open(&install.store_path)?;
    let updates = crate::storage::inventory(&install.writes_path)?;
    let parent = install
        .game_path
        .parent()
        .context("Game has no parent folder")?;
    let mut plan = crate::storage::SpacePlan::default();
    plan.add(
        crate::storage::volume(parent)?,
        reader
            .summary()
            .logical_bytes
            .saturating_add(updates.bytes)
            .saturating_add(updates.metadata_bytes),
        "Temporary restoration verification copy",
    )?;
    plan.recheck()?;
    let staging = tempfile::Builder::new()
        .prefix(".flummox-verify-")
        .tempdir_in(parent)?;
    let expected = staging.path().join("expected");
    super::restore(&install.store_path, &expected, cancel)?;
    super::overlay::Overlay::open(&install.writes_path, Some(&reader))?.apply_to(&expected)?;
    // Two comparisons: a hash over every regular file's path, size and
    // bytes, then the mode, link target, xattrs and hard links of every path.
    let expected_bytes = crate::compatibility::corpus(&expected, cancel, observer)?;
    let actual_bytes = crate::compatibility::corpus(&install.game_path, cancel, observer)?;
    ensure!(
        expected_bytes == actual_bytes,
        "Ordinary files differ from the retained store and updates; all copies retained"
    );
    ensure!(
        metadata(&expected, observer)? == metadata(&install.game_path, observer)?,
        "Ordinary metadata differs from the retained store and updates; all copies retained"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check};
    #[test]
    fn restore_verification_requires_matching_bytes_and_retains_storage() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let source = fixture.path().join("source");
        std::fs::create_dir(&source).ctx("source")?;
        std::fs::write(source.join("data"), b"base bytes").ctx("base")?;
        let store = fixture.path().join("store");
        let cancel = AtomicBool::new(false);
        let summary =
            super::super::create(&source, &store, super::super::Options::default(), &cancel)
                .ctx("store")?;
        let restored = fixture.path().join("restored");
        super::super::restore(&store, &restored, &cancel).ctx("restore")?;
        let writes = fixture.path().join("writes");
        let overlay = super::super::overlay::Overlay::open(&writes, None).ctx("updates")?;
        drop(overlay);
        let install = Install {
            game_path: restored.clone(),
            store_path: store.clone(),
            writes_path: writes.clone(),
            backup_path: None,
            previous_store_path: None,
            previous_writes_path: None,
            summary: Some(summary),
            phase: super::super::InstallPhase::Attention,
            message: "interrupted".into(),
        };
        verify_restored(&install, &cancel, &super::super::NoObserver)
            .ctx("verify completed restoration")?;
        check(
            store.exists() && writes.exists(),
            "verification must retain store and updates",
        )?;
        std::fs::write(restored.join("data"), b"changed bytes").ctx("change")?;
        check(
            verify_restored(&install, &cancel, &super::super::NoObserver).is_err(),
            "unexpected bytes must require review",
        )?;
        check(
            store.exists() && writes.exists(),
            "failed verification must retain all storage",
        )
    }
}
