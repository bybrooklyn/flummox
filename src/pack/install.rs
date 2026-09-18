//! Transactional launcher-path activation for writable pack stores.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Durable state for a store mounted at a launcher's existing install path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Install {
    #[serde(with = "crate::path_serde")]
    pub game_path: PathBuf,
    #[serde(with = "crate::path_serde")]
    pub store_path: PathBuf,
    #[serde(with = "crate::path_serde")]
    pub writes_path: PathBuf,
    #[serde(default, with = "crate::path_serde::option")]
    pub backup_path: Option<PathBuf>,
    #[serde(default, with = "crate::path_serde::option")]
    pub previous_store_path: Option<PathBuf>,
    #[serde(default, with = "crate::path_serde::option")]
    pub previous_writes_path: Option<PathBuf>,
    #[serde(default)]
    pub summary: Option<crate::pack::Summary>,
    pub phase: InstallPhase,
    pub message: String,
}

/// Recovery state for an activated install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallPhase {
    Switching,
    Mounted,
    Reclaiming,
    Compacting,
    Pruning,
    Restoring,
    Attention,
}

impl InstallPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Switching => "Activating",
            Self::Mounted => "Ready",
            Self::Reclaiming => "Reclaiming space",
            Self::Compacting => "Compacting updates",
            Self::Pruning => "Reclaiming previous version",
            Self::Restoring => "Restoring files",
            Self::Attention => "Needs attention",
        }
    }
}

#[cfg(feature = "pack-mount")]
mod enabled {
    use super::*;
    use crate::pack::{Reader, mount};
    use anyhow::{Context, Result, ensure};
    use std::{
        fs::Permissions,
        os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        path::Path,
        sync::atomic::AtomicBool,
    };

    pub(crate) struct MountedInstall {
        pub path: PathBuf,
        session: mount::Session,
    }

    impl MountedInstall {
        pub fn finished(&self) -> bool {
            self.session.is_finished()
        }

        pub fn writes(&self) -> Option<mount::WriteController> {
            self.session.writes()
        }

        pub fn stop(self) -> Result<()> {
            self.session.umount_and_join()?;
            Ok(())
        }
    }

    fn canonical_new_dir(path: &Path) -> Result<PathBuf> {
        if path.exists() {
            let path = path.canonicalize()?;
            ensure!(path.is_dir(), "The update layer must be a directory");
            return Ok(path);
        }
        let parent = path
            .parent()
            .context("The update layer has no parent")?
            .canonicalize()?;
        let name = path.file_name().context("The update layer has no name")?;
        let path = parent.join(name);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .context("Creating the update layer")?;
        Ok(path.canonicalize()?)
    }

    fn backup_for(game: &Path) -> Result<PathBuf> {
        let parent = game.parent().context("The game folder has no parent")?;
        let name = game.file_name().context("The game folder has no name")?;
        Ok(parent.join(format!(".{}.flummox-original", name.to_string_lossy())))
    }

    fn validate_paths(game: &Path, store: &Path, writes: &Path) -> Result<()> {
        ensure!(
            game != store && game != writes && store != writes,
            "The game, store, and update layer must use different paths"
        );
        ensure!(
            !store.starts_with(game) && !writes.starts_with(game),
            "Keep the store and update layer outside the game folder"
        );
        ensure!(
            !game.starts_with(writes),
            "The game folder cannot be inside its update layer"
        );
        Ok(())
    }

    fn mount_record(install: &Install) -> Result<MountedInstall> {
        let session = mount::mount(
            &install.store_path,
            &install.game_path,
            Some(&install.writes_path),
        )?;
        let _entry = std::fs::read_dir(&install.game_path)
            .context("The mounted game folder cannot be read")?
            .next()
            .transpose()
            .context("The mounted game folder cannot be read")?;
        Ok(MountedInstall {
            path: install.game_path.clone(),
            session,
        })
    }

    fn clear_disconnected_mount(path: &Path) -> Result<()> {
        let managed = crate::fsprobe::mounts()?.iter().any(|mount| {
            mount.mountpoint == path
                && mount.fstype.starts_with("fuse")
                && mount.source == "flummox-pack"
        });
        if !managed {
            return Ok(());
        }
        let helper = [
            "/usr/bin/fusermount3",
            "/bin/fusermount3",
            "/usr/bin/fusermount",
            "/bin/fusermount",
        ]
        .into_iter()
        .find(|helper| Path::new(helper).is_file())
        .context("A disconnected FUSE mount needs fusermount3 to recover")?;
        let status = std::process::Command::new(helper)
            .arg("-u")
            .arg(path)
            .status()
            .context("Starting the FUSE unmount helper")?;
        ensure!(
            status.success(),
            "The disconnected FUSE mount could not be cleared"
        );
        Ok(())
    }

    /// Validates paths and returns a record before the first path mutation.
    pub(crate) fn prepare(
        game: &Path,
        store: &Path,
        writes: &Path,
        cancel: &AtomicBool,
    ) -> Result<Install> {
        let game = game.canonicalize().context("Finding the installed game")?;
        ensure!(game.is_dir(), "The game path must be a directory");
        let parent = game.parent().context("The game folder has no parent")?;
        ensure!(
            std::fs::symlink_metadata(&game)?.dev() == std::fs::symlink_metadata(parent)?.dev(),
            "The game path is already a mount point"
        );
        let store = store.canonicalize().context("Finding the pack store")?;
        ensure!(
            store.is_file() || store.is_dir(),
            "The pack store must be a file or directory"
        );
        let writes = canonical_new_dir(writes)?;
        validate_paths(&game, &store, &writes)?;
        let reader = Reader::open(&store).context("Validating the pack store")?;
        reader
            .verify_directory(&game, cancel)
            .context("The store no longer matches the installed game")?;
        let backup = backup_for(&game)?;
        ensure!(
            !backup.exists(),
            "The rollback folder already exists: {}",
            backup.display()
        );
        Ok(Install {
            game_path: game,
            store_path: store,
            writes_path: writes,
            backup_path: Some(backup),
            previous_store_path: None,
            previous_writes_path: None,
            summary: Some(reader.summary().clone()),
            phase: InstallPhase::Switching,
            message: "Activation recorded; preparing the launcher path".into(),
        })
    }

    /// Completes a prepared path switch and starts its writable mount.
    pub(crate) fn activate(install: &mut Install) -> Result<MountedInstall> {
        ensure!(
            install.phase == InstallPhase::Switching,
            "Install is not awaiting activation"
        );
        let backup = install
            .backup_path
            .as_ref()
            .context("The rollback path is missing")?;
        if !backup.exists() {
            ensure!(
                install.game_path.is_dir(),
                "The original game folder is missing"
            );
            std::fs::rename(&install.game_path, backup)
                .context("Moving the original game to its rollback path")?;
        }
        if !install.game_path.exists() {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&install.game_path)
                .context("Creating the launcher mount point")?;
        }
        let mounted = mount_record(install)?;
        install.phase = InstallPhase::Mounted;
        install.message = "Writable compressed install is mounted; rollback copy retained".into();
        Ok(mounted)
    }

    /// Restarts an existing mount or completes an interrupted transaction.
    pub(crate) fn recover(install: &mut Install) -> Result<Option<MountedInstall>> {
        if install.phase == InstallPhase::Reclaiming {
            finish_reclaim(install)?;
        }
        if install.phase == InstallPhase::Pruning {
            finish_prune(install)?;
        }
        if install.phase == InstallPhase::Compacting {
            install.phase = InstallPhase::Mounted;
            install.message = "Compaction was interrupted; using the previous store".into();
        }
        if install.phase == InstallPhase::Switching {
            return activate(install).map(Some);
        }
        if install.summary.is_none() {
            install.summary = Some(Reader::open(&install.store_path)?.summary().clone());
        }
        ensure!(
            matches!(
                install.phase,
                InstallPhase::Mounted | InstallPhase::Attention
            ),
            "Install needs manual attention"
        );
        clear_disconnected_mount(&install.game_path)?;
        if !install.game_path.exists() {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&install.game_path)?;
        }
        let parent = install
            .game_path
            .parent()
            .context("The game folder has no parent")?;
        ensure!(
            std::fs::symlink_metadata(&install.game_path)?.dev()
                == std::fs::symlink_metadata(parent)?.dev(),
            "The launcher path is already mounted by another process"
        );
        ensure!(
            std::fs::read_dir(&install.game_path)?.next().is_none(),
            "The launcher mount point contains unexpected files"
        );
        let mounted = mount_record(install)?;
        install.phase = InstallPhase::Mounted;
        install.message = if install.backup_path.is_some() {
            "Writable compressed install is mounted; rollback copy retained".into()
        } else {
            "Writable compressed install is mounted".into()
        };
        Ok(Some(mounted))
    }

    pub(crate) fn reclaim(install: &mut Install) -> Result<()> {
        ensure!(
            install.phase == InstallPhase::Mounted,
            "Install is not ready"
        );
        ensure!(
            install.backup_path.is_some(),
            "The rollback copy was already reclaimed"
        );
        install.phase = InstallPhase::Reclaiming;
        install.message = "Removing the retained original files".into();
        Ok(())
    }

    pub(crate) fn finish_reclaim(install: &mut Install) -> Result<()> {
        ensure!(
            install.phase == InstallPhase::Reclaiming,
            "Install is not reclaiming space"
        );
        if let Some(backup) = &install.backup_path
            && backup.exists()
        {
            std::fs::remove_dir_all(backup).context("Removing the rollback copy")?;
        }
        install.backup_path = None;
        install.phase = InstallPhase::Mounted;
        install.message = "Writable compressed install is mounted".into();
        Ok(())
    }

    pub(crate) fn begin_prune(install: &mut Install) -> Result<()> {
        ensure!(
            install.phase == InstallPhase::Mounted,
            "Install is not ready"
        );
        ensure!(
            install.previous_store_path.is_some(),
            "There is no previous store to reclaim"
        );
        install.phase = InstallPhase::Pruning;
        install.message = "Removing the retained previous store".into();
        Ok(())
    }

    pub(crate) fn finish_prune(install: &mut Install) -> Result<()> {
        ensure!(
            install.phase == InstallPhase::Pruning,
            "Install is not reclaiming a previous store"
        );
        if let Some(previous) = &install.previous_store_path {
            ensure!(
                previous != &install.store_path,
                "The previous and current stores use the same path"
            );
            let removed = if previous.is_dir() {
                std::fs::remove_dir_all(previous)
            } else {
                std::fs::remove_file(previous)
            };
            match removed {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Removing the previous store"),
            }
        }
        if let Some(previous) = &install.previous_writes_path {
            ensure!(
                previous != &install.writes_path,
                "The previous and current update layers use the same path"
            );
            match std::fs::remove_dir_all(previous) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Removing the previous update layer"),
            }
        }
        install.previous_store_path = None;
        install.previous_writes_path = None;
        install.phase = InstallPhase::Mounted;
        install.message = if install.backup_path.is_some() {
            "Writable compressed install is mounted; rollback copy retained".into()
        } else {
            "Writable compressed install is mounted".into()
        };
        Ok(())
    }

    pub(crate) fn rollback(
        install: &Install,
        mounted: Option<MountedInstall>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        ensure!(
            matches!(
                install.phase,
                InstallPhase::Mounted | InstallPhase::Restoring
            ),
            "Install is not ready to restore"
        );
        if let Some(mounted) = mounted {
            mounted
                .stop()
                .context("Unmounting the compressed install")?;
        }
        if !install.game_path.exists() {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&install.game_path)?;
        }
        ensure!(
            install.game_path.is_dir(),
            "The launcher mount point is missing"
        );
        ensure!(
            std::fs::read_dir(&install.game_path)?.next().is_none(),
            "The launcher path did not unmount cleanly"
        );
        std::fs::remove_dir(&install.game_path)?;
        if let Some(backup) = &install.backup_path {
            crate::pack::overlay::Overlay::open(&install.writes_path)?.apply_to(backup)?;
            match std::fs::rename(backup, &install.game_path) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    std::fs::DirBuilder::new()
                        .mode(0o700)
                        .create(&install.game_path)?;
                    return Err(error).context("Restoring the original game folder");
                }
            }
        }
        let parent = install
            .game_path
            .parent()
            .context("The game folder has no parent")?;
        let staging = tempfile::Builder::new()
            .prefix(".flummox-restore-")
            .tempdir_in(parent)?;
        let restored = staging.path().join("game");
        crate::pack::restore(&install.store_path, &restored, cancel)?;
        crate::pack::overlay::Overlay::open(&install.writes_path)?.apply_to(&restored)?;
        std::fs::rename(&restored, &install.game_path)
            .context("Publishing the restored game folder")?;
        std::fs::set_permissions(&install.game_path, Permissions::from_mode(0o755))?;
        Ok(())
    }
}

#[cfg(feature = "pack-mount")]
pub(crate) use enabled::*;

#[cfg(all(test, feature = "pack-mount"))]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn activation_refuses_a_store_for_different_source_bytes() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let game = temp.path().join("game");
        let store = temp.path().join("game.flumpack");
        std::fs::create_dir(&game).ctx("game folder")?;
        std::fs::write(game.join("data"), b"first").ctx("source")?;
        crate::pack::create(
            &game,
            &store,
            crate::pack::Options::default(),
            &AtomicBool::new(false),
        )
        .ctx("store")?;
        std::fs::write(game.join("data"), b"other").ctx("changed source")?;
        check(
            prepare(
                &game,
                &store,
                &temp.path().join("updates"),
                &AtomicBool::new(false),
            )
            .is_err(),
            "activation rejects stale source data",
        )?;
        check_eq(
            std::fs::read(game.join("data")).ctx("unchanged game")?,
            b"other".to_vec(),
            "failed activation does not move the game",
        )
    }

    #[test]
    fn activation_preserves_updates_across_both_rollback_paths() -> TestResult {
        if !std::path::Path::new("/dev/fuse").exists() {
            check(
                std::env::var_os("FLUMMOX_REQUIRE_FUSE").is_none(),
                "FUSE is required for this test run",
            )?;
            eprintln!("skipped: pack activation requires /dev/fuse");
            return Ok(());
        }
        let temp = tempfile::tempdir().ctx("fixture")?;
        let game = temp.path().join("game");
        let store = temp.path().join("game.flumpack");
        let writes = temp.path().join("updates");
        std::fs::create_dir(&game).ctx("game folder")?;
        std::fs::write(game.join("data"), b"base").ctx("base file")?;
        crate::pack::create(
            &game,
            &store,
            crate::pack::Options::default(),
            &AtomicBool::new(false),
        )
        .ctx("store")?;

        let mut install =
            prepare(&game, &store, &writes, &AtomicBool::new(false)).ctx("prepare")?;
        let mounted = activate(&mut install).ctx("activate")?;
        check_eq(
            std::fs::read(game.join("data")).ctx("mounted base")?,
            b"base".to_vec(),
            "launcher path reads the store",
        )?;
        std::fs::write(game.join("data"), b"patched").ctx("patch through mount")?;
        std::fs::write(game.join("new"), b"download").ctx("new mounted file")?;
        rollback(&install, Some(mounted), &AtomicBool::new(false)).ctx("safe rollback")?;
        check_eq(
            std::fs::read(game.join("data")).ctx("restored patch")?,
            b"patched".to_vec(),
            "rollback merges changed files",
        )?;
        check_eq(
            std::fs::read(game.join("new")).ctx("restored download")?,
            b"download".to_vec(),
            "rollback merges new files",
        )?;
        check(
            install
                .backup_path
                .as_ref()
                .is_some_and(|backup| !backup.exists()),
            "rollback consumes the retained copy",
        )?;

        let second_store = temp.path().join("game-second.flumpack");
        crate::pack::create(
            &game,
            &second_store,
            crate::pack::Options::default(),
            &AtomicBool::new(false),
        )
        .ctx("second store")?;
        let writes_after_reclaim = temp.path().join("updates-after-reclaim");
        let mut install = prepare(
            &game,
            &second_store,
            &writes_after_reclaim,
            &AtomicBool::new(false),
        )
        .ctx("prepare again")?;
        let mounted = activate(&mut install).ctx("activate again")?;
        std::fs::write(game.join("data"), b"second patch").ctx("second patch")?;
        reclaim(&mut install).ctx("begin reclaim")?;
        finish_reclaim(&mut install).ctx("finish reclaim")?;
        check_eq(
            install.backup_path.clone(),
            None,
            "reclaim drops the rollback path",
        )?;
        rollback(&install, Some(mounted), &AtomicBool::new(false)).ctx("materialized rollback")?;
        check_eq(
            std::fs::read(game.join("data")).ctx("materialized patch")?,
            b"second patch".to_vec(),
            "post-reclaim rollback rebuilds updated files",
        )
    }
}
