//! Read-only Heroic/Lutris adapters and user-added folders.
//!
//! Installed manifests identify game directories; credentials and launcher
//! executables are never read or executed. Corrupt sources produce warnings.

use super::{DetectError, Env, Scan};
use crate::model::{Game, GameId, InstallState, Launcher};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

const MANIFEST_LIMIT: u64 = 16 * 1024 * 1024;

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
fn game(
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
    let state = if path.is_dir() {
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
fn parse_installed(value: &Value, launcher: Launcher) -> Vec<Game> {
    let mut games = vec![];
    let entries: Vec<(String, &Value)> = match value.get("installed").unwrap_or(value) {
        Value::Array(entries) => entries
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v))
            .collect(),
        Value::Object(entries) => entries.iter().map(|(k, v)| (k.clone(), v)).collect(),
        _ => vec![],
    };
    for (key, item) in entries {
        let Some(path) = item.get("install_path").and_then(Value::as_str) else {
            continue;
        };
        let key = item
            .get("app_name")
            .and_then(Value::as_str)
            .unwrap_or(&key)
            .to_owned();
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
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            });
        if let Some(mut game) = game(
            launcher,
            key,
            title,
            path.into(),
            build,
            item.get("install_size").and_then(Value::as_u64),
        ) {
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
fn lutris(path: &Path) -> anyhow::Result<Vec<Game>> {
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_millis(500))?;
    let mut stmt = db.prepare("SELECT id,name,directory FROM games WHERE installed=1 AND directory IS NOT NULL LIMIT 100000")?;
    let mut games = vec![];
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (id, title, path) = row?;
        if let Some(game) = game(
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
                Ok(value) => scan.games.extend(parse_installed(&value, launcher)),
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
        match lutris(&path) {
            Ok(games) => scan.games.extend(games),
            Err(e) => scan.warnings.push(DetectError::new(
                format!("Reading {}", path.display()),
                std::io::Error::other(e.to_string()),
            )),
        }
    }
}

pub(super) fn custom(scan: &mut Scan) {
    match crate::jobs::configured_libraries() {
        Ok(libraries) => {
            for library in libraries.into_iter().filter(|l| l.custom) {
                let path = library.path;
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Game folder".into());
                let key = path.to_string_lossy().into_owned();
                if let Some(game) = game(Launcher::Manual, key, name, path, None, None) {
                    scan.games.push(game);
                }
            }
        }
        Err(e) => scan.warnings.push(DetectError::new(
            "Reading custom folders",
            std::io::Error::other(e.to_string()),
        )),
    }
}

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
    scan.games = games;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    #[test]
    fn reads_installed_manifests_and_preserves_unavailable_games() -> TestResult {
        let data = serde_json::json!({"a":{"title":"A game","install_path":"/unavailable/game","version":"2"},"bad":{"install_path":"relative"}});
        let games = parse_installed(&data, Launcher::HeroicLegendary);
        check_eq(games.len(), 1, "relative paths are refused")?;
        let game = games.first().ctx("one installed game")?;
        check_eq(game.title.as_str(), "A game", "title")?;
        check(!game.state.is_idle(), "a disconnected game stays visible")
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
            lutris(&path).ctx("read fixture")?.len(),
            1,
            "installed entries only",
        )?;
        check_eq(std::fs::read(&path).ctx("after")?, before, "read only")
    }
}
