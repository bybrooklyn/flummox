//! Durable discovery cache. Missing drives retain their games and volume identity.
use crate::{
    model::{Game, InstallState},
    storage,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RememberedGame {
    pub game: Game,
    pub volume: storage::Volume,
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    games: Vec<RememberedGame>,
}

pub fn data_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"));
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        // The XDG specification says to ignore a relative value.
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    Ok(base
        .context("Cannot locate local application state")?
        .join("flummox"))
}

pub fn private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)?;
    Ok(())
}

pub fn remember(root: &Path, games: Vec<Game>, keep: impl Fn(&Game) -> bool) -> Result<Vec<Game>> {
    private_dir(root)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("libraries.lock"))?;
    lock.lock()?;
    let path = root.join("libraries.json");
    let old: Cache = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Reading remembered libraries")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Cache::default(),
        Err(error) => return Err(error.into()),
    };
    let mut result = Vec::new();
    for game in games {
        if let Some(previous) = old
            .games
            .iter()
            .find(|previous| previous.game.install_dir == game.install_dir)
        {
            let online = storage::volume(&previous.volume.path)
                .is_ok_and(|volume| volume.identity == previous.volume.identity);
            if !online {
                continue;
            }
        }
        let volume = match storage::volume(&game.install_dir) {
            Ok(volume) if game.install_dir.is_dir() => volume,
            _ => continue,
        };
        result.push(RememberedGame { game, volume });
    }
    for mut previous in old.games {
        if !keep(&previous.game)
            || result
                .iter()
                .any(|current| current.game.install_dir == previous.game.install_dir)
        {
            continue;
        }
        let online = storage::volume(&previous.volume.path)
            .is_ok_and(|volume| volume.identity == previous.volume.identity);
        previous.game.state = InstallState::Broken { detail: if online { "Library unavailable: game was not rediscovered; refresh after checking its launcher" } else { "Library unavailable: reconnect its original drive" }.into() };
        result.push(previous);
    }
    let mut staged = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer(
        &mut staged,
        &Cache {
            games: result.clone(),
        },
    )?;
    staged.flush()?;
    staged.as_file().sync_all()?;
    staged.persist(&path)?;
    #[cfg(unix)]
    std::fs::File::open(root)?.sync_all()?;
    Ok(result.into_iter().map(|record| record.game).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{GameId, Launcher},
        testutil::{Ctx, TestResult, check, check_eq},
    };
    #[test]
    fn missing_libraries_survive_restart_and_reconnect() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let root = fixture.path().join("game");
        std::fs::create_dir(&root).ctx("game")?;
        let state = fixture.path().join("state");
        let game = Game {
            id: GameId::new(Launcher::Manual, "fixture"),
            also: vec![],
            title: "Fixture".into(),
            install_dir: root,
            build: None,
            size_hint: None,
            state: InstallState::Idle,
            is_tool: false,
        };
        let first = remember(&state, vec![game.clone()], |_| true).ctx("remember")?;
        check_eq(first.len(), 1, "initial game")?;
        let offline = remember(&state, vec![], |_| true).ctx("unavailable discovery")?;
        check_eq(offline.len(), 1, "remembered after restart")?;
        check(
            !offline.first().ctx("offline game")?.state.is_idle(),
            "missing game must not start jobs",
        )?;
        let online = remember(&state, vec![game], |_| true).ctx("reconnect")?;
        check(
            online.first().ctx("online game")?.state.is_idle(),
            "rediscovery resumes availability",
        )?;
        check(
            remember(&state, vec![], |_| false)
                .ctx("remove library")?
                .is_empty(),
            "explicit removal forgets cached games",
        )
    }
}
