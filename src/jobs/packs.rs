//! Maximum Space transactions the coordinator runs: activation, rollback,
//! reclaim, compaction, pruning and recovery at start.

use super::*;
use anyhow::{Result, bail};
use rusqlite::Connection;
use std::path::Path;
// Without mounting, each transaction below is a stub that needs none of these.
#[cfg(feature = "pack-mount")]
use {
    super::{client::*, service::PackControl},
    anyhow::{Context, ensure},
    std::path::PathBuf,
};

/// The live mount session of one activated install.
#[cfg(feature = "pack-mount")]
pub(super) type PackMount = crate::pack::MountedInstall;

#[cfg(not(feature = "pack-mount"))]
pub(super) struct PackMount;

/// Writes every install record to settings row 4. Each transaction calls
/// this before and after a step that changes paths on disk.
#[cfg(feature = "pack-mount")]
pub(super) fn save_packs(db: &Connection, snapshot: &Snapshot) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO settings(id,data) VALUES(4,?1)",
        [serde_json::to_string(&snapshot.packs)?],
    )?;
    Ok(())
}

/// Re-reads the summary of each store that opens into its install record.
#[cfg(feature = "pack-mount")]
pub(super) fn refresh_pack_summaries(snapshot: &mut Snapshot) {
    // Shared-byte accounting depends on how many stores currently link each
    // pool object. Refresh every readable store after that population changes.
    // A broken install keeps its last useful summary and is handled by the
    // normal recovery path instead of failing an otherwise successful action.
    for install in &mut snapshot.packs {
        if let Ok(reader) = crate::pack::Reader::open(&install.store_path) {
            install.summary = Some(reader.summary().clone());
        }
    }
}

/// Removes and returns the mount at `path`, if the coordinator holds one.
#[cfg(feature = "pack-mount")]
pub(super) fn take_mount(mounts: &mut Vec<PackMount>, path: &Path) -> Option<PackMount> {
    mounts
        .iter()
        .position(|mounted| mounted.path == path)
        .map(|position| mounts.remove(position))
}

/// Prepares an existing store for `game_path` and switches the game to it.
/// Refused while any process uses the game folder.
#[cfg(feature = "pack-mount")]
pub(super) fn pack_activate(
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
    game_path: &Path,
    store_path: &Path,
    writes_path: &Path,
) -> Result<()> {
    // Activation moves this folder aside and mounts over it, so it gets the
    // same refusals as any other job target.
    let game_path = &super::validate_folder(game_path)?;
    ensure!(
        crate::busy::process_using(game_path, &crate::busy::ProcFs::new()).is_none(),
        "Close the game and its launcher, then switch this game to Maximum again."
    );
    let install = crate::pack::prepare(
        game_path,
        store_path,
        writes_path,
        &std::sync::atomic::AtomicBool::new(false),
    )?;
    activate_prepared(snapshot, db, mounts, install)
}

/// Saves the install record, then switches paths and mounts the store. The
/// record is on disk before the switch, so `recover_packs` finds an
/// interrupted one. A game with an existing record is refused.
#[cfg(feature = "pack-mount")]
pub(super) fn activate_prepared(
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
    install: crate::pack::Install,
) -> Result<()> {
    let game_path = &install.game_path;
    ensure!(
        crate::busy::process_using(game_path, &crate::busy::ProcFs::new()).is_none(),
        "The game or its launcher started while the store was being checked. Close it and try again."
    );
    ensure!(
        !snapshot
            .packs
            .iter()
            .any(|current| current.game_path == install.game_path),
        "This game already uses Maximum."
    );
    snapshot.packs.push(install);
    save_packs(db, snapshot)?;
    let install = snapshot
        .packs
        .last_mut()
        .context("Flummox has no record of this game using Maximum.")?;
    match crate::pack::activate(install) {
        Ok(mounted) => mounts.push(mounted),
        Err(error) => {
            install.message =
                format!("Switching to Maximum did not finish: {error}. Review it in Jobs.");
            save_packs(db, snapshot)?;
            return Err(error);
        }
    }
    refresh_pack_summaries(snapshot);
    save_packs(db, snapshot)
}

#[cfg(not(feature = "pack-mount"))]
pub(super) fn pack_activate(
    _snapshot: &mut Snapshot,
    _db: &Connection,
    _mounts: &mut Vec<PackMount>,
    _game_path: &Path,
    _store_path: &Path,
    _writes_path: &Path,
) -> Result<()> {
    bail!("This build of Flummox cannot switch games to Maximum.")
}

/// Restores ordinary files at the game's path and removes its install
/// record. The `Restoring` phase is saved first. `recover_packs` turns a
/// record left in that phase into one needing attention.
#[cfg(feature = "pack-mount")]
pub(super) fn pack_rollback(
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
    game_path: &Path,
) -> Result<()> {
    let canonical = game_path
        .canonicalize()
        .context("Finding the launcher path")?;
    ensure!(
        crate::busy::process_using(&canonical, &crate::busy::ProcFs::new()).is_none(),
        "Close the game and its launcher, then decompress this game again."
    );
    let position = snapshot
        .packs
        .iter()
        .position(|install| install.game_path == canonical)
        .context("This game is not using Maximum.")?;
    let install = snapshot
        .packs
        .get_mut(position)
        .context("Flummox has no record of this game using Maximum.")?;
    install.phase = crate::pack::InstallPhase::Restoring;
    install.message = "Decompressing to ordinary files…".into();
    save_packs(db, snapshot)?;
    let install = snapshot
        .packs
        .get(position)
        .cloned()
        .context("Flummox has no record of this game using Maximum.")?;
    let mounted = take_mount(mounts, &canonical);
    crate::pack::rollback(
        &install,
        mounted,
        &std::sync::atomic::AtomicBool::new(false),
    )?;
    snapshot.packs.remove(position);
    save_packs(db, snapshot)?;
    // The login entry exists while any install is mounted. This may have been
    // the last one.
    super::autostart::configure(
        &binary()?,
        snapshot.libraries.iter().any(|library| library.automatic) || !snapshot.packs.is_empty(),
    )?;
    Ok(())
}

#[cfg(not(feature = "pack-mount"))]
pub(super) fn pack_rollback(
    _snapshot: &mut Snapshot,
    _db: &Connection,
    _mounts: &mut Vec<PackMount>,
    _game_path: &Path,
) -> Result<()> {
    bail!("This build of Flummox cannot decompress games that use Maximum.")
}

/// Verifies the whole store, then deletes the original kept at activation.
/// The `Reclaiming` phase is saved between the two, so recovery finishes a
/// delete that was interrupted.
#[cfg(feature = "pack-mount")]
pub(super) fn pack_reclaim(
    snapshot: &mut Snapshot,
    db: &Connection,
    game_path: &Path,
) -> Result<()> {
    let canonical = game_path
        .canonicalize()
        .context("Finding the launcher path")?;
    ensure!(
        crate::busy::process_using(&canonical, &crate::busy::ProcFs::new()).is_none(),
        "Close the game and its launcher, then delete the original again."
    );
    let install = snapshot
        .packs
        .iter_mut()
        .find(|install| install.game_path == canonical)
        .context("This game is not using Maximum.")?;
    // Chunks are otherwise checked only as the game reads them, and the store
    // may have been activated weeks ago. This is the last moment a damaged
    // store can still be replaced from the original.
    crate::pack::Reader::open(&install.store_path)
        .and_then(|store| store.verify(&std::sync::atomic::AtomicBool::new(false)))
        .context("The store failed verification, so the original was kept")?;
    crate::pack::reclaim(install)?;
    save_packs(db, snapshot)?;
    let install = snapshot
        .packs
        .iter_mut()
        .find(|install| install.game_path == canonical)
        .context("Flummox has no record of this game using Maximum.")?;
    crate::pack::finish_reclaim(install)?;
    save_packs(db, snapshot)
}

#[cfg(not(feature = "pack-mount"))]
pub(super) fn pack_reclaim(
    _snapshot: &mut Snapshot,
    _db: &Connection,
    _game_path: &Path,
) -> Result<()> {
    bail!("This build of Flummox cannot delete the original of a game that uses Maximum.")
}

/// An unused hidden path beside `path` for a compacted store or update
/// layer. Its name carries a hash of `identity`, the label and this process id.
#[cfg(feature = "pack-mount")]
pub(super) fn compact_path(path: &Path, identity: &Path, label: &str) -> Result<PathBuf> {
    let parent = path.parent().context("The managed path has no parent")?;
    let prefix = crate::pack::compaction_prefix(identity, label);
    for attempt in 0..100u32 {
        let candidate = parent.join(format!("{prefix}{}-{attempt}", std::process::id()));
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => return Err(error).context("Checking the compaction destination"),
        }
    }
    bail!("Could not reserve a path for the compacted install")
}

/// Deletes a store, whether it is a directory or a single file.
#[cfg(feature = "pack-mount")]
pub(super) fn remove_store(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Compaction for the direct client command, with no pause or cancel.
#[cfg(feature = "pack-mount")]
pub(super) fn pack_compact(
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
    game_path: &Path,
) -> Result<()> {
    pack_compact_observed(snapshot, db, mounts, game_path, &PackControl::default())
}

/// Builds a new store from the mounted install, then unmounts, switches the
/// record to the new store and remounts. The build honours `control`. If the
/// game was written during the build, the new store is removed and the
/// install is left as it was.
#[cfg(feature = "pack-mount")]
pub(super) fn pack_compact_observed(
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
    game_path: &Path,
    control: &PackControl,
) -> Result<()> {
    let canonical = game_path
        .canonicalize()
        .context("Finding the launcher path")?;
    let position = snapshot
        .packs
        .iter()
        .position(|install| install.game_path == canonical)
        .context("This game is not using Maximum.")?;
    let install = snapshot
        .packs
        .get(position)
        .context("Flummox has no record of this game using Maximum.")?;
    ensure!(
        install.phase == crate::pack::InstallPhase::Mounted,
        "This game is not ready for that yet. Wait for the current step to finish."
    );
    ensure!(
        install.previous_store_path.is_none() && install.previous_writes_path.is_none(),
        "Delete the previous version before folding in updates again."
    );
    // Restoring from the retained original replays the update layer onto it.
    // Compaction empties that layer, so the original would come back without
    // the updates the new store absorbed.
    ensure!(
        install.backup_path.is_none(),
        "Delete the original before folding in updates."
    );
    let old_store = install.store_path.clone();
    let old_writes = install.writes_path.clone();
    let pool = crate::pack::Reader::open(&old_store)?
        .pool_path()
        .map(Path::to_path_buf);
    let new_store = compact_path(&old_store, &canonical, "compact-store")?;
    let new_writes = compact_path(&old_writes, &canonical, "compact-updates")?;
    let controller = mounts
        .iter()
        .find(|mounted| mounted.path == canonical)
        .and_then(PackMount::writes)
        .context("The writable install is not mounted")?;
    // Each committed write through the mount advances the generation. The
    // value is compared after the build, with writes frozen.
    let baseline = controller.generation();

    let options = crate::pack::Options::maximum();
    let summary = if let Some(pool) = pool {
        crate::pack::create_shared_observed(
            &canonical,
            &new_store,
            &pool,
            options,
            &control.cancel,
            control,
        )
    } else {
        crate::pack::create_observed(&canonical, &new_store, options, &control.cancel, control)
    }
    .context("Building the compacted store from the live install")?;
    if let Err(error) = control
        .transaction("Switching to the new store. This step finishes before the job can stop.")
    {
        let _removed = remove_store(&new_store);
        return Err(error);
    }
    let frozen = match controller.freeze() {
        Ok(frozen) => frozen,
        Err(error) => {
            let _removed = remove_store(&new_store);
            return Err(error);
        }
    };
    if frozen.generation() != baseline {
        let _removed = remove_store(&new_store);
        bail!(
            "The game changed while updates were being folded in. Try again when the launcher has finished updating."
        )
    }

    // Unmounting under a running game detaches the folder from it, and the
    // frozen mount rejects its writes until it exits.
    if let Some(user) = crate::busy::process_using(&canonical, &crate::busy::ProcFs::new()) {
        let _removed = remove_store(&new_store);
        bail!("{user} is using the game folder; close it and retry")
    }

    // From here the switch runs to the end. A record left in `Compacting` is
    // recovered onto the previous store.
    let install = snapshot
        .packs
        .get_mut(position)
        .context("Flummox has no record of this game using Maximum.")?;
    install.phase = crate::pack::InstallPhase::Compacting;
    install.message = "Switching to the new store…".into();
    save_packs(db, snapshot)?;
    let mounted = take_mount(mounts, &canonical).context("The writable install is not mounted")?;
    if let Err(error) = mounted.stop() {
        let _removed = remove_store(&new_store);
        return Err(error).context("Unmounting the previous store");
    }

    let install = snapshot
        .packs
        .get_mut(position)
        .context("Flummox has no record of this game using Maximum.")?;
    install.previous_store_path = Some(old_store);
    install.previous_writes_path = Some(old_writes);
    install.store_path = new_store;
    install.writes_path = new_writes;
    install.summary = Some(summary);
    install.phase = crate::pack::InstallPhase::Mounted;
    install.message = "Updates folded in. The previous version is kept until you delete it.".into();
    save_packs(db, snapshot)?;
    let install = snapshot
        .packs
        .get_mut(position)
        .context("Flummox has no record of this game using Maximum.")?;
    let mounted = crate::pack::recover(install)?.context("The compacted store did not remount")?;
    mounts.push(mounted);
    refresh_pack_summaries(snapshot);
    save_packs(db, snapshot)
}

#[cfg(not(feature = "pack-mount"))]
pub(super) fn pack_compact(
    _snapshot: &mut Snapshot,
    _db: &Connection,
    _mounts: &mut Vec<PackMount>,
    _game_path: &Path,
) -> Result<()> {
    bail!("This build of Flummox cannot fold in updates for games that use Maximum.")
}

/// Deletes the previous store and update layer kept by compaction, then
/// drops shared-pool objects no store links any more.
#[cfg(feature = "pack-mount")]
pub(super) fn pack_prune(snapshot: &mut Snapshot, db: &Connection, game_path: &Path) -> Result<()> {
    let canonical = game_path
        .canonicalize()
        .context("Finding the launcher path")?;
    let position = snapshot
        .packs
        .iter()
        .position(|install| install.game_path == canonical)
        .context("This game is not using Maximum.")?;
    let install = snapshot
        .packs
        .get_mut(position)
        .context("Flummox has no record of this game using Maximum.")?;
    let pool = crate::pack::Reader::open(&install.store_path)?
        .pool_path()
        .map(Path::to_path_buf);
    crate::pack::begin_prune(install)?;
    save_packs(db, snapshot)?;
    let install = snapshot
        .packs
        .get_mut(position)
        .context("Flummox has no record of this game using Maximum.")?;
    crate::pack::finish_prune(install)?;
    save_packs(db, snapshot)?;
    if let Some(pool) = pool {
        let _pruned = crate::pack::prune_shared_pool(&pool)?;
    }
    refresh_pack_summaries(snapshot);
    save_packs(db, snapshot)
}

#[cfg(not(feature = "pack-mount"))]
pub(super) fn pack_prune(
    _snapshot: &mut Snapshot,
    _db: &Connection,
    _game_path: &Path,
) -> Result<()> {
    bail!("This build of Flummox cannot delete the previous version of a game that uses Maximum.")
}

/// The title of the game at `path`, or its folder name when discovery has
/// not listed it.
pub(super) fn game_title(snapshot: &Snapshot, path: &Path) -> String {
    snapshot
        .discovered
        .iter()
        .find(|game| game.install_dir == path)
        .map(|game| game.title.clone())
        .or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| path.display().to_string())
}

/// Unmounts every store this coordinator serves, one at a time. On the first
/// failure the rest stay mounted and the error names the game.
#[cfg(feature = "pack-mount")]
pub(super) fn stop_mounts(mounts: &mut Vec<PackMount>, snapshot: &Snapshot) -> Result<()> {
    while let Some(mounted) = mounts.pop() {
        let path = mounted.path.clone();
        if let Err(error) = mounted.stop() {
            bail!(
                "Could not unmount {}: {error}. Close it and try again.",
                game_title(snapshot, &path)
            );
        }
    }
    Ok(())
}

#[cfg(not(feature = "pack-mount"))]
pub(super) fn stop_mounts(_mounts: &mut Vec<PackMount>, _snapshot: &Snapshot) -> Result<()> {
    Ok(())
}

/// Mounts every recorded install that has no live mount, finishing any
/// interrupted transaction first. One that fails is marked `Attention` and
/// the rest still mount. Runs at start and again while mounts are missing.
#[cfg(feature = "pack-mount")]
pub(super) fn recover_packs(
    snapshot: &mut Snapshot,
    db: &Connection,
    mounts: &mut Vec<PackMount>,
) -> Result<()> {
    let mut recovered = Vec::with_capacity(snapshot.packs.len());
    for mut install in std::mem::take(&mut snapshot.packs) {
        if install.phase == crate::pack::InstallPhase::Restoring {
            install.phase = crate::pack::InstallPhase::Attention;
            install.message = "Decompressing was interrupted. The kept copies are unchanged. Check the decompressed files before deleting anything.".into();
            recovered.push(install);
            continue;
        }
        if mounts
            .iter()
            .any(|mounted| mounted.path == install.game_path)
        {
            recovered.push(install);
            continue;
        }
        match crate::pack::recover(&mut install) {
            Ok(Some(mounted)) => mounts.push(mounted),
            Ok(None) => {}
            Err(error) => {
                // These phases name a step still to be finished. Keeping them
                // makes the next attempt resume it instead of mounting past it.
                if !matches!(
                    install.phase,
                    crate::pack::InstallPhase::Reclaiming
                        | crate::pack::InstallPhase::Pruning
                        | crate::pack::InstallPhase::Switching
                ) {
                    install.phase = crate::pack::InstallPhase::Attention;
                }
                install.message = format!(
                    "Could not start this game from its store automatically: {error}. Restart Flummox to try again."
                );
            }
        }
        recovered.push(install);
    }
    snapshot.packs = recovered;
    refresh_pack_summaries(snapshot);
    save_packs(db, snapshot)
}

#[cfg(not(feature = "pack-mount"))]
pub(super) fn recover_packs(
    snapshot: &mut Snapshot,
    _db: &Connection,
    _mounts: &mut Vec<PackMount>,
) -> Result<()> {
    for install in &mut snapshot.packs {
        install.phase = crate::pack::InstallPhase::Attention;
        install.message = "This build of Flummox cannot run games from a store. Install a build that supports Maximum.".into();
    }
    Ok(())
}

#[cfg(all(test, feature = "pack-mount"))]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    use std::sync::atomic::AtomicBool;

    fn database() -> Result<Connection, String> {
        let db = Connection::open_in_memory().ctx("database")?;
        db.execute_batch("CREATE TABLE settings(id INTEGER PRIMARY KEY, data TEXT NOT NULL);")
            .ctx("settings table")?;
        Ok(db)
    }

    #[test]
    fn a_failed_recovery_keeps_the_phase_of_an_unfinished_step() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let record = |phase| crate::pack::Install {
            game_path: temp.path().join("game"),
            store_path: temp.path().join("missing.flumpack"),
            writes_path: temp.path().join("updates"),
            backup_path: Some(temp.path().join(".game.flummox-original")),
            previous_store_path: None,
            previous_writes_path: None,
            summary: None,
            phase,
            message: String::new(),
        };
        let mut snapshot = Snapshot {
            packs: vec![
                record(crate::pack::InstallPhase::Reclaiming),
                record(crate::pack::InstallPhase::Mounted),
            ],
            ..Snapshot::default()
        };
        let db = database()?;
        let mut mounts = Vec::new();
        recover_packs(&mut snapshot, &db, &mut mounts).ctx("recover")?;
        let phases: Vec<_> = snapshot.packs.iter().map(|install| install.phase).collect();
        check_eq(
            phases,
            vec![
                crate::pack::InstallPhase::Reclaiming,
                // Control: a plain failure to mount still needs attention.
                crate::pack::InstallPhase::Attention,
            ],
            "an interrupted reclaim stays an interrupted reclaim",
        )?;
        check(
            snapshot
                .packs
                .first()
                .is_some_and(|install| install.backup_path.is_some()),
            "the retained original is still recorded",
        )
    }

    #[test]
    fn compaction_refuses_to_unmount_under_a_running_process() -> TestResult {
        if !std::path::Path::new("/dev/fuse").exists() {
            check(
                std::env::var_os("FLUMMOX_REQUIRE_FUSE").is_none(),
                "FUSE is required for this test run",
            )?;
            eprintln!("skipped: compaction requires /dev/fuse");
            return Ok(());
        }
        let temp = tempfile::tempdir().ctx("fixture")?;
        let game = temp.path().join("game");
        std::fs::create_dir(&game).ctx("game")?;
        std::fs::write(game.join("data"), b"bytes").ctx("source")?;
        let store = temp.path().join("game.flumpack");
        crate::pack::create(
            &game,
            &store,
            crate::pack::Options::default(),
            &AtomicBool::new(false),
        )
        .ctx("store")?;
        let install = crate::pack::prepare(
            &game,
            &store,
            &temp.path().join("updates"),
            &AtomicBool::new(false),
        )
        .ctx("prepare")?;
        let canonical = install.game_path.clone();
        let db = database()?;
        let mut snapshot = Snapshot::default();
        let mut mounts = Vec::new();
        activate_prepared(&mut snapshot, &db, &mut mounts, install).ctx("activate")?;
        pack_reclaim(&mut snapshot, &db, &canonical).ctx("reclaim the original")?;
        // Control: with nothing using the folder the same call works, below.
        let mut child = std::process::Command::new("sleep")
            .arg("20")
            .current_dir(&canonical)
            .spawn()
            .ctx("process in the game folder")?;
        let refused = pack_compact(&mut snapshot, &db, &mut mounts, &canonical);
        let _killed = child.kill();
        let _waited = child.wait();
        check(
            refused.is_err(),
            "compaction is refused while a process uses the folder",
        )?;
        check_eq(mounts.len(), 1, "the mount is still held after the refusal")?;
        check_eq(
            std::fs::read(canonical.join("data")).ctx("read through the mount")?,
            b"bytes".to_vec(),
            "the folder still serves its files",
        )?;
        pack_compact(&mut snapshot, &db, &mut mounts, &canonical)
            .ctx("compaction with the folder idle")?;
        let mounted = take_mount(&mut mounts, &canonical).ctx("mount after compaction")?;
        mounted.stop().ctx("unmount")
    }
}
