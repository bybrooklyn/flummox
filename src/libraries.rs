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

/// A game as last discovered, with the volume it was on at that time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RememberedGame {
    pub game: Game,
    /// Compared by identity later to tell whether the same drive is mounted.
    pub volume: storage::Volume,
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    games: Vec<RememberedGame>,
}

/// The per-user `flummox` state directory for this platform. It is not created
/// here. Callers use `private_dir` before writing.
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

/// Creates the directory and its parents. On Unix the mode is 0700, and an existing
/// directory is tightened to 0700 as well.
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

/// Largest `libraries.json` that is read, in bytes.
const CACHE_LIMIT: u64 = 64 * 1024 * 1024;

/// Reads the cache. A file that is too large, or does not parse, is renamed to
/// `libraries.json.unreadable` and an empty cache is used, so one bad write or
/// a schema change cannot fail every later scan.
fn read_cache(path: &Path) -> Result<(Cache, Option<Vec<u8>>)> {
    use std::io::Read;
    let mut bytes = Vec::new();
    match std::fs::File::open(path) {
        Ok(file) => {
            file.take(CACHE_LIMIT + 1).read_to_end(&mut bytes)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Cache::default(), None));
        }
        Err(error) => return Err(error.into()),
    }
    let parsed = if bytes.len() as u64 > CACHE_LIMIT {
        Err(anyhow::anyhow!("larger than {CACHE_LIMIT} bytes"))
    } else {
        serde_json::from_slice::<Cache>(&bytes).map_err(Into::into)
    };
    match parsed {
        Ok(cache) => Ok((cache, Some(bytes))),
        Err(error) => {
            let aside = path.with_extension("json.unreadable");
            tracing::warn!(error = %error, moved_to = %aside.display(), "remembered libraries were unreadable");
            std::fs::rename(path, &aside)
                .context("Setting aside unreadable remembered libraries")?;
            Ok((Cache::default(), None))
        }
    }
}

/// The cache as bytes with free space zeroed, which changes on every reading
/// and says nothing about which games are remembered.
fn stable_bytes(games: &[RememberedGame]) -> Result<Vec<u8>> {
    let mut games = games.to_vec();
    for record in &mut games {
        record.volume.available = 0;
    }
    Ok(serde_json::to_vec(&Cache { games })?)
}

/// Merges a fresh discovery with `libraries.json` under `root` and writes the result
/// back when it changed. A cached game that was not rediscovered is returned as
/// `Broken` while its drive is absent, or present but the game was not listed, for
/// as long as `keep` accepts it. It is forgotten when `keep` refuses it, when its
/// drive is present and its folder is gone, or when its id was found elsewhere.
pub fn remember(root: &Path, games: Vec<Game>, keep: impl Fn(&Game) -> bool) -> Result<Vec<Game>> {
    private_dir(root)?;
    // The lock serialises the read, merge and write between processes. It is
    // released when `lock` drops at return.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("libraries.lock"))?;
    lock.lock()?;
    let path = root.join("libraries.json");
    let (old, old_bytes) = read_cache(&path)?;
    let mut result: Vec<RememberedGame> = Vec::new();
    // Games that cannot be placed on a volume are listed but not cached.
    let mut uncached: Vec<Game> = Vec::new();
    for mut game in games {
        // A fresh sighting is accepted whatever the cache says about the volume.
        // The launcher found the folder, so it is there now.
        if !game.install_dir.is_dir() {
            if game.state.is_idle() {
                game.state = InstallState::Broken {
                    detail: "Drive or folder unavailable".into(),
                };
            }
            uncached.push(game);
            continue;
        }
        match storage::volume(&game.install_dir) {
            Ok(volume) => result.push(RememberedGame { game, volume }),
            // Read-only or unprobeable. The game is present, so it stays listed.
            Err(_) => uncached.push(game),
        }
    }
    // Cached games missing from this scan. The detail says whether the drive is
    // gone or only the game.
    let mut online_by_mount: std::collections::HashMap<PathBuf, Option<String>> =
        std::collections::HashMap::new();
    for mut previous in old.games {
        let seen_dir = |dir: &Path| {
            result.iter().any(|current| current.game.install_dir == dir)
                || uncached.iter().any(|game| game.install_dir == dir)
        };
        let seen_id = result
            .iter()
            .map(|current| &current.game)
            .chain(uncached.iter())
            .any(|game| {
                game.ids()
                    .any(|id| previous.game.ids().any(|old| old == id))
            });
        if !keep(&previous.game) || seen_dir(&previous.game.install_dir) || seen_id {
            continue;
        }
        let identity = online_by_mount
            .entry(previous.volume.path.clone())
            .or_insert_with_key(|mount| storage::volume(mount).ok().map(|v| v.identity));
        let online = identity.as_deref() == Some(previous.volume.identity.as_str());
        if online && !previous.game.install_dir.exists() {
            continue;
        }
        previous.game.state = InstallState::Broken { detail: if online { "Library unavailable: game was not rediscovered; refresh after checking its launcher" } else { "Library unavailable: reconnect its original drive" }.into() };
        result.push(previous);
    }
    // Skip the write when nothing but free space changed. Otherwise write a temp
    // file in the same directory, fsync it, rename it over the cache, then fsync
    // the directory so the rename itself survives a crash.
    let unchanged = match (&old_bytes, stable_bytes(&result)) {
        (Some(bytes), Ok(new)) => serde_json::from_slice::<Cache>(bytes)
            .ok()
            .and_then(|cache| stable_bytes(&cache.games).ok())
            .is_some_and(|before| before == new),
        _ => false,
    };
    if !unchanged {
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
    }
    let mut games: Vec<Game> = result.into_iter().map(|record| record.game).collect();
    games.extend(uncached);
    Ok(games)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{GameId, Launcher},
        testutil::{Ctx, TestResult, check, check_eq},
    };
    fn fixture_game(key: &str, dir: &Path) -> Game {
        Game {
            id: GameId::new(Launcher::HeroicGog, key),
            also: vec![],
            title: key.into(),
            install_dir: dir.to_path_buf(),
            build: None,
            size_hint: None,
            state: InstallState::Idle,
            is_tool: false,
        }
    }
    #[test]
    fn uninstalled_and_moved_games_are_forgotten_on_an_online_drive() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let state = fixture.path().join("state");
        let (a, b, c) = (
            fixture.path().join("a"),
            fixture.path().join("b"),
            fixture.path().join("c"),
        );
        for dir in [&a, &b, &c] {
            std::fs::create_dir(dir).ctx("game folder")?;
        }
        remember(
            &state,
            vec![fixture_game("one", &a), fixture_game("two", &c)],
            |_| true,
        )
        .ctx("first scan")?;
        std::fs::remove_dir(&c).ctx("uninstall")?;
        let after = remember(&state, vec![fixture_game("one", &b)], |_| true).ctx("second scan")?;
        check_eq(
            after.len(),
            1,
            "the uninstalled game and the old path of the moved game are gone",
        )?;
        check_eq(
            after.first().map(|g| g.install_dir.clone()),
            Some(b),
            "the moved game is listed once, at its new path",
        )
    }
    #[test]
    fn a_new_volume_at_a_remembered_path_is_accepted() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let state = fixture.path().join("state");
        let dir = fixture.path().join("game");
        std::fs::create_dir(&dir).ctx("game folder")?;
        remember(&state, vec![fixture_game("one", &dir)], |_| true).ctx("first scan")?;
        let cache = state.join("libraries.json");
        let text = std::fs::read_to_string(&cache).ctx("cache")?;
        let mut value: serde_json::Value = serde_json::from_str(&text).ctx("cache json")?;
        let identity = value
            .pointer_mut("/games/0/volume/identity")
            .ctx("identity field")?;
        *identity = serde_json::Value::String("ext4:reformatted".into());
        std::fs::write(&cache, value.to_string()).ctx("tamper")?;
        let again = remember(&state, vec![fixture_game("one", &dir)], |_| true).ctx("rescan")?;
        check(
            again.first().ctx("game")?.state.is_idle(),
            "a reinstalled game on a replaced drive must not stay locked out",
        )?;
        let text = std::fs::read_to_string(&cache).ctx("cache again")?;
        check(
            !text.contains("ext4:reformatted"),
            "the cached volume is replaced",
        )
    }
    #[test]
    fn an_unreadable_cache_is_set_aside_and_the_scan_goes_on() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let state = fixture.path().join("state");
        let dir = fixture.path().join("game");
        std::fs::create_dir(&dir).ctx("game folder")?;
        private_dir(&state).ctx("state")?;
        std::fs::write(state.join("libraries.json"), b"{ not json").ctx("bad cache")?;
        let games = remember(&state, vec![fixture_game("one", &dir)], |_| true)
            .ctx("a bad cache must not fail the scan")?;
        check_eq(games.len(), 1, "the fresh scan is returned")?;
        check(
            state.join("libraries.json.unreadable").exists(),
            "the bad file is kept aside",
        )
    }
    #[test]
    fn a_game_on_a_missing_folder_is_listed_not_dropped() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let state = fixture.path().join("state");
        let games = remember(
            &state,
            vec![fixture_game("one", &fixture.path().join("unplugged"))],
            |_| true,
        )
        .ctx("scan")?;
        check_eq(games.len(), 1, "the game stays visible")?;
        check(
            !games.first().ctx("game")?.state.is_idle(),
            "and cannot start a job",
        )
    }
    #[cfg(unix)]
    #[test]
    fn an_unchanged_scan_does_not_rewrite_the_cache() -> TestResult {
        use std::os::unix::fs::MetadataExt;
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let state = fixture.path().join("state");
        let dir = fixture.path().join("game");
        std::fs::create_dir(&dir).ctx("game folder")?;
        let game = fixture_game("one", &dir);
        remember(&state, vec![game.clone()], |_| true).ctx("first")?;
        let before = std::fs::metadata(state.join("libraries.json"))
            .ctx("stat")?
            .ino();
        remember(&state, vec![game], |_| true).ctx("second")?;
        let after = std::fs::metadata(state.join("libraries.json"))
            .ctx("stat")?
            .ino();
        check_eq(
            after,
            before,
            "the file was replaced although nothing changed",
        )
    }
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
