//! Read-only Heroic/Lutris adapters and user-added folders.
//!
//! Installed manifests identify game directories; credentials and launcher
//! executables are never read or executed. Corrupt sources produce warnings.

use super::{DetectError, Env, Scan};
use crate::model::{Game, GameId, InstallState, Launcher};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Largest launcher manifest that is read, in bytes.
const MANIFEST_LIMIT: u64 = 16 * 1024 * 1024;

/// Parses a JSON manifest, refusing one larger than [`MANIFEST_LIMIT`].
fn json(path: &Path) -> anyhow::Result<Value> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MANIFEST_LIMIT + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MANIFEST_LIMIT,
        "Launcher manifest exceeds 16 MiB"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
/// Builds a game record for an absolute install path. A path that is not a
/// directory now is kept and marked `Broken`, so a game on a disconnected
/// drive stays listed. So is a path [`crate::model::refuse_install_path_with`]
/// refuses. A relative path gives `None`.
fn game(
    home: Option<&Path>,
    launcher: Launcher,
    key: String,
    title: String,
    path: PathBuf,
    build: Option<String>,
    size_hint: Option<u64>,
) -> Option<Game> {
    if !path.is_absolute() {
        return None;
    }
    let state = if let Err(detail) = crate::model::refuse_install_path_with(&path, home) {
        InstallState::Broken { detail }
    } else if path.is_dir() {
        InstallState::Idle
    } else {
        InstallState::Broken {
            detail: "Drive or folder unavailable".into(),
        }
    };
    Some(Game {
        id: GameId::new(launcher, key),
        also: vec![],
        title,
        install_dir: path,
        build,
        size_hint,
        state,
        is_tool: false,
    })
}
/// Reads the games out of an `installed.json`. The list may be an array or
/// an object keyed by app name, at the top level or under `installed`.
/// Entries without an `install_path` are skipped.
fn parse_installed(value: &Value, launcher: Launcher, home: Option<&Path>) -> Vec<Game> {
    let mut games = vec![];
    let list = value.get("installed").unwrap_or(value);
    let positional = list.is_array();
    let entries: Vec<(String, &Value)> = match list {
        Value::Array(entries) => entries.iter().map(|v| (String::new(), v)).collect(),
        Value::Object(entries) => entries.iter().map(|(k, v)| (k.clone(), v)).collect(),
        _ => vec![],
    };
    for (key, item) in entries {
        let Some(path) = item.get("install_path").and_then(Value::as_str) else {
            continue;
        };
        // Heroic has written both spellings. The array position is never an
        // id, since it shifts when an earlier game is uninstalled.
        let named = ["app_name", "appName"]
            .iter()
            .find_map(|name| item.get(*name).and_then(Value::as_str))
            .filter(|name| !name.is_empty());
        let key = match (named, positional) {
            (Some(name), _) => name.to_owned(),
            (None, false) => key,
            (None, true) => path.to_owned(),
        };
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                Path::new(path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| key.clone());
        let build = item
            .get("version")
            .or_else(|| item.get("build_id"))
            .or_else(|| item.get("buildId"))
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            });
        if let Some(mut game) = game(
            home,
            launcher,
            key,
            title,
            path.into(),
            build,
            item.get("install_size").and_then(Value::as_u64),
        ) {
            // Either marker puts the game in `UpdatePending`, which keeps
            // jobs from starting on it.
            if Path::new(path).join(".gogdl-resume").exists()
                || Path::new(path).join(".egstore/bps").exists()
            {
                game.state = InstallState::UpdatePending;
            }
            games.push(game);
        }
    }
    games
}
/// Lists installed games from a Lutris `pga.db`, opened read-only.
fn lutris(path: &Path, home: Option<&Path>) -> anyhow::Result<Vec<Game>> {
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_millis(500))?;
    let mut stmt = db.prepare("SELECT id,name,directory FROM games WHERE installed=1 AND directory IS NOT NULL LIMIT 100000")?;
    let mut games = vec![];
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        // One row that cannot be read must not hide the others.
        let Ok((id, title, path)) = row else { continue };
        let title = title.filter(|t| !t.is_empty()).unwrap_or_else(|| {
            Path::new(&path)
                .file_name()
                .map_or_else(|| id.to_string(), |s| s.to_string_lossy().into_owned())
        });
        if let Some(game) = game(
            home,
            Launcher::Lutris,
            id.to_string(),
            title,
            path.into(),
            None,
            None,
        ) {
            games.push(game);
        }
    }
    Ok(games)
}

/// Adds Heroic and Lutris games from their native and Flatpak locations
/// under `env.home`. A source that exists but cannot be read adds a warning.
pub(super) fn discover(env: &Env, scan: &mut Scan) {
    let configs = [
        env.home.join(".config"),
        env.home.join(".var/app/com.heroicgameslauncher.hgl/config"),
    ];
    for config in configs {
        for (relative, launcher) in [
            ("legendary/installed.json", Launcher::HeroicLegendary),
            (
                "heroic/legendaryConfig/legendary/installed.json",
                Launcher::HeroicLegendary,
            ),
            ("heroic/gog_store/installed.json", Launcher::HeroicGog),
            ("heroic/nile_config/installed.json", Launcher::HeroicNile),
        ] {
            let path = config.join(relative);
            if !path.exists() {
                continue;
            }
            match json(&path) {
                Ok(value) => scan
                    .games
                    .extend(parse_installed(&value, launcher, Some(&env.home))),
                Err(e) => scan.warnings.push(DetectError::new(
                    format!("Reading {}", path.display()),
                    std::io::Error::other(e.to_string()),
                )),
            }
        }
    }
    for path in [
        env.home.join(".local/share/lutris/pga.db"),
        env.home
            .join(".var/app/net.lutris.Lutris/data/lutris/pga.db"),
    ] {
        if !path.exists() {
            continue;
        }
        match lutris(&path, Some(&env.home)) {
            Ok(games) => scan.games.extend(games),
            Err(e) => scan.warnings.push(DetectError::new(
                format!("Reading {}", path.display()),
                std::io::Error::other(e.to_string()),
            )),
        }
    }
}

/// Adds games from the custom locations saved in the coordinator's queue
/// database. Reads the real user's state, so fixture scans must not call it.
pub(super) fn custom(scan: &mut Scan) {
    match crate::jobs::configured_libraries() {
        Ok(libraries) => {
            for library in libraries.into_iter().filter(|l| l.custom) {
                add_custom(scan, &library);
            }
        }
        Err(e) => scan.warnings.push(DetectError::new(
            "Reading custom folders",
            std::io::Error::other(e.to_string()),
        )),
    }
}

/// Adds one custom location as a `Manual` game keyed by its path. A
/// collection adds each direct subdirectory instead, skipping hidden names,
/// files and symlinks.
fn add_custom(scan: &mut Scan, library: &crate::jobs::Library) {
    let paths = if library.folder_kind == crate::jobs::FolderKind::Collection {
        let entries = match std::fs::read_dir(&library.path) {
            Ok(entries) => entries,
            Err(error) => {
                scan.warnings.push(DetectError::new(
                    format!("Reading {}", library.path.display()),
                    error,
                ));
                return;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            let result = (|| -> std::io::Result<_> {
                let entry = entry?;
                let hidden = entry.file_name().to_string_lossy().starts_with('.');
                Ok((!hidden && entry.file_type()?.is_dir()).then(|| entry.path()))
            })();
            match result {
                Ok(Some(path)) => paths.push(path),
                Ok(None) => {}
                Err(error) => scan.warnings.push(DetectError::new(
                    format!("Reading {}", library.path.display()),
                    error,
                )),
            }
        }
        paths.sort();
        paths
    } else {
        vec![library.path.clone()]
    };
    for path in paths {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Game folder".into());
        let key = path.to_string_lossy().into_owned();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if let Some(game) = game(
            home.as_deref(),
            Launcher::Manual,
            key,
            name,
            path,
            None,
            None,
        ) {
            scan.games.push(game);
        }
    }
}

/// Collapses games that share a canonical install folder into one record.
/// The first one found keeps its id and the others' ids go into `also`. A
/// state other than idle from any of them replaces the kept state.
pub(super) fn merge(scan: &mut Scan) {
    let mut games: Vec<Game> = vec![];
    for mut game in scan.games.drain(..) {
        let path = game
            .install_dir
            .canonicalize()
            .unwrap_or_else(|_| game.install_dir.clone());
        if let Some(existing) = games.iter_mut().find(|g| g.install_dir == path) {
            if existing.id != game.id && !existing.also.contains(&game.id) {
                existing.also.push(game.id);
            }
            for id in game.also {
                if id != existing.id && !existing.also.contains(&id) {
                    existing.also.push(id);
                }
            }
            if !game.state.is_idle() {
                existing.state = game.state;
            }
        } else {
            game.install_dir = path;
            games.push(game);
        }
    }
    crate::model::flag_swallowing_games(&mut games);
    scan.games = games;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    #[test]
    fn custom_collections_list_direct_games_and_merge_duplicates() -> TestResult {
        use crate::jobs::{FolderKind, Library};
        let temp = tempfile::tempdir().ctx("custom collection")?;
        let root = temp.path().join("My Games");
        let first = root.join("A Game");
        let second = root.join("Another Game");
        for dir in [
            &first,
            &second,
            &root.join(".flummox"),
            &first.join("assets"),
        ] {
            std::fs::create_dir_all(dir).ctx("fixture folder")?;
        }
        std::fs::write(root.join("notes.txt"), b"leave untouched").ctx("non-game file")?;
        std::os::unix::fs::symlink(&first, root.join("alias")).ctx("symlink")?;
        let library = Library {
            path: root,
            automatic: false,
            custom: true,
            folder_kind: FolderKind::Collection,
        };
        let mut scan = Scan::default();
        add_custom(&mut scan, &library);
        check_eq(
            scan.games.len(),
            2,
            "hidden folders, files, symlinks and nested assets excluded",
        )?;
        check_eq(
            scan.games.first().ctx("first game")?.title.as_str(),
            "A Game",
            "folder names become titles",
        )?;
        add_custom(
            &mut scan,
            &Library {
                path: first.clone(),
                folder_kind: FolderKind::Game,
                ..library
            },
        );
        merge(&mut scan);
        check_eq(
            scan.games.len(),
            2,
            "overlapping locations do not duplicate games",
        )?;
        std::fs::create_dir(second.parent().ctx("collection")?.join("New Game"))
            .ctx("new install")?;
        let mut refreshed = Scan::default();
        add_custom(
            &mut refreshed,
            &Library {
                path: temp.path().join("My Games"),
                automatic: false,
                custom: true,
                folder_kind: FolderKind::Collection,
            },
        );
        check_eq(
            refreshed.games.len(),
            3,
            "refresh discovers newly added games",
        )
    }
    #[test]
    fn a_collection_folder_holding_a_steam_library_is_not_one_game() -> TestResult {
        use crate::jobs::{FolderKind, Library};
        let temp = tempfile::tempdir().ctx("fixture")?;
        let root = temp.path().join("games");
        let steam_game = root.join("SteamLibrary/steamapps/common/Deep Game");
        let real = root.join("Real Game");
        let empty_library = root.join("OldLibrary");
        for dir in [&steam_game, &real, &empty_library.join("steamapps")] {
            std::fs::create_dir_all(dir).ctx("fixture folder")?;
        }
        let mut scan = Scan::default();
        scan.games.push(
            game(
                None,
                Launcher::Steam,
                "7".into(),
                "Deep Game".into(),
                steam_game,
                None,
                None,
            )
            .ctx("steam game")?,
        );
        add_custom(
            &mut scan,
            &Library {
                path: root,
                automatic: false,
                custom: true,
                folder_kind: FolderKind::Collection,
            },
        );
        merge(&mut scan);
        let state_of = |title: &str| {
            scan.games
                .iter()
                .find(|g| g.title == title)
                .map(|g| g.state.is_idle())
        };
        check_eq(state_of("Real Game"), Some(true), "a real game stays idle")?;
        check_eq(
            state_of("Deep Game"),
            Some(true),
            "the Steam game stays idle",
        )?;
        check_eq(
            state_of("SteamLibrary"),
            Some(false),
            "a folder that contains a discovered game must not be a job target",
        )?;
        check_eq(
            state_of("OldLibrary"),
            Some(false),
            "a folder that holds steamapps must not be a job target",
        )
    }
    #[test]
    fn reads_installed_manifests_and_preserves_unavailable_games() -> TestResult {
        let data = serde_json::json!({"a":{"title":"A game","install_path":"/unavailable/game","version":"2"},"bad":{"install_path":"relative"}});
        let games = parse_installed(&data, Launcher::HeroicLegendary, None);
        check_eq(games.len(), 1, "relative paths are refused")?;
        let game = games.first().ctx("one installed game")?;
        check_eq(game.title.as_str(), "A game", "title")?;
        check(!game.state.is_idle(), "a disconnected game stays visible")
    }
    #[test]
    fn heroic_ids_come_from_the_app_name_and_never_from_the_position() -> TestResult {
        let ids = |data: serde_json::Value| -> Vec<String> {
            parse_installed(&data, Launcher::HeroicGog, None)
                .into_iter()
                .map(|g| g.id.key)
                .collect()
        };
        let camel = ids(serde_json::json!({"installed":[
            {"appName":"111","install_path":"/g/one","buildId":"7"},
            {"app_name":"222","install_path":"/g/two"}]}));
        check_eq(
            camel,
            vec!["111".to_owned(), "222".to_owned()],
            "both spellings",
        )?;
        let unnamed = |paths: &[&str]| {
            ids(
                serde_json::json!({"installed": paths.iter().map(|p| serde_json::json!({"install_path": p})).collect::<Vec<_>>()}),
            )
        };
        let before = unnamed(&["/g/one", "/g/two"]);
        let after = unnamed(&["/g/two"]);
        check_eq(
            after.first(),
            before.get(1),
            "removing an earlier entry must not change a later game's id",
        )?;
        let built = parse_installed(
            &serde_json::json!([{"appName":"1","install_path":"/g/one","buildId":"9"}]),
            Launcher::HeroicGog,
            None,
        );
        check_eq(
            built.first().and_then(|g| g.build.as_deref()),
            Some("9"),
            "buildId is read",
        )
    }
    #[test]
    fn launcher_paths_that_cover_the_home_or_leave_it_are_listed_broken() -> TestResult {
        let home = Path::new("/home/someone");
        for path in ["/", "/home", "/home/someone", "/games/../etc"] {
            let games = parse_installed(
                &serde_json::json!({"a": {"install_path": path}}),
                Launcher::HeroicGog,
                Some(home),
            );
            let game = games.first().ctx(path)?;
            check(!game.state.is_idle(), format!("{path} must not be idle"))?;
        }
        let games = parse_installed(
            &serde_json::json!({"a": {"install_path": "/home/someone/Games/One"}}),
            Launcher::HeroicGog,
            Some(home),
        );
        check(
            !games
                .first()
                .ctx("game under home")?
                .state
                .to_string()
                .contains("home folder"),
            "a game folder under the home is not refused as the home",
        )
    }
    #[test]
    fn a_lutris_row_without_a_name_does_not_hide_the_others() -> TestResult {
        let temp = tempfile::tempdir().ctx("temporary library")?;
        let path = temp.path().join("pga.db");
        let db = rusqlite::Connection::open(&path).ctx("fixture database")?;
        db.execute_batch("CREATE TABLE games(id INTEGER,name TEXT,directory TEXT,installed INTEGER); INSERT INTO games VALUES(1,NULL,'/unavailable/one',1),(2,'Named','/unavailable/two',1);").ctx("fixture rows")?;
        drop(db);
        check_eq(
            lutris(&path, None).ctx("read fixture")?.len(),
            2,
            "both rows listed",
        )
    }
    #[test]
    fn reads_lutris_without_changing_its_database() -> TestResult {
        let temp = tempfile::tempdir().ctx("temporary library")?;
        let path = temp.path().join("pga.db");
        let db = rusqlite::Connection::open(&path).ctx("fixture database")?;
        db.execute_batch("CREATE TABLE games(id INTEGER,name TEXT,directory TEXT,installed INTEGER); INSERT INTO games VALUES(1,'Game','/unavailable/game',1),(2,'Not installed','/other',0);").ctx("fixture rows")?;
        drop(db);
        let before = std::fs::read(&path).ctx("before")?;
        check_eq(
            lutris(&path, None).ctx("read fixture")?.len(),
            1,
            "installed entries only",
        )?;
        check_eq(std::fs::read(&path).ctx("after")?, before, "read only")
    }
}
