//! Bounded local launcher manifests shared by native desktops and fixture tests.
#[cfg(target_os = "linux")]
use crate::launchers::vdf;
#[cfg(target_os = "macos")]
use crate::native_vdf as vdf;
#[cfg(windows)]
use crate::windows_vdf as vdf;
use crate::{
    desktop,
    model::{BusyReason, Game, GameId, InstallState, Launcher},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::{Path, PathBuf};
/// What discovery found. Providers append to it, and a manifest that cannot be read
/// becomes a warning while the other games are kept.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Catalog {
    pub games: Vec<Game>,
    pub warnings: Vec<String>,
    /// The Steam roots given to `steam`, which the window's artwork index searches.
    pub artwork_roots: Vec<PathBuf>,
}
/// Parses a JSON file of at most 16 MiB.
fn json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&desktop::read_bounded(path, 16 * 1024 * 1024)?)
        .with_context(|| format!("Reading {}", path.display()))
}
/// Builds a launcher game. A relative path is refused. A path that
/// [`crate::model::refuse_install_path`] refuses, or a directory that is absent,
/// yields a `Broken` game, so a game on a disconnected drive stays listed.
fn installed(
    launcher: Launcher,
    key: String,
    title: String,
    path: PathBuf,
    build: Option<String>,
) -> Result<Game> {
    ensure!(
        path.is_absolute(),
        "Launcher install directory must be an absolute game folder"
    );
    let state = if let Err(detail) = crate::model::refuse_install_path(&path) {
        InstallState::Broken { detail }
    } else if path.is_dir() {
        InstallState::Idle
    } else {
        InstallState::Broken {
            detail: "Drive or folder unavailable".into(),
        }
    };
    Ok(Game {
        id: GameId::new(launcher, key),
        also: vec![],
        title,
        install_dir: path,
        build,
        size_hint: None,
        state,
        is_tool: false,
    })
}
/// Whether paths that differ only in case name the same folder.
const FOLD_CASE: bool = cfg!(any(windows, target_os = "macos"));

/// A string that is equal for paths naming the same folder. With `fold`,
/// case, slash direction and a trailing slash do not matter, as with the
/// registry's `c:/program files (x86)/steam` against `C:\Program Files (x86)\Steam`.
fn path_key(path: &Path, fold: bool) -> String {
    let text = path.to_string_lossy();
    if !fold {
        return text.into_owned();
    }
    text.replace('\\', "/").trim_end_matches('/').to_lowercase()
}
impl Catalog {
    /// Adds every game in the Steam libraries reachable from these install roots.
    /// Replaces `artwork_roots`.
    pub fn steam(&mut self, roots: Vec<PathBuf>) {
        self.artwork_roots = roots.clone();
        // A set, so a library listed by two roots is read once and in a fixed order.
        let mut libraries = std::collections::BTreeMap::new();
        for root in roots {
            let steamapps = root.join("steamapps");
            libraries.insert(path_key(&steamapps, FOLD_CASE), steamapps.clone());
            let manifest = steamapps.join("libraryfolders.vdf");
            if !manifest.exists() {
                continue;
            }
            // libraryfolders.vdf lists the other libraries. An entry is either the
            // path itself or an object with a `path` key. Both forms are accepted.
            let result = (|| -> Result<()> {
                let bytes = desktop::read_bounded(&manifest, 16 * 1024 * 1024)?;
                let text = String::from_utf8_lossy(&bytes);
                let object = vdf::parse(&text)?;
                for (_, value) in object.entries() {
                    let path = match value {
                        vdf::Value::Str(path) => Some(path.as_str()),
                        vdf::Value::Obj(object) => object.get_str("path"),
                    };
                    if let Some(path) = path
                        && Path::new(path).is_absolute()
                    {
                        let library = PathBuf::from(path).join("steamapps");
                        libraries
                            .entry(path_key(&library, FOLD_CASE))
                            .or_insert(library);
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                self.warnings.push(format!("Steam libraries: {error}"));
            }
        }
        for library in libraries.into_values() {
            if !library.exists() {
                continue;
            }
            let entries = match std::fs::read_dir(&library) {
                Ok(entries) => entries,
                Err(error) => {
                    self.warnings
                        .push(format!("Steam {}: {error}", library.display()));
                    continue;
                }
            };
            // One appmanifest_<appid>.acf per installed game.
            let mut paths: Vec<_> = entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.starts_with("appmanifest_") && name.ends_with(".acf")
                        })
                })
                .collect();
            paths.sort();
            for path in paths {
                match steam_game(&path, &library) {
                    Ok(game) => self.games.push(game),
                    Err(error) => self
                        .warnings
                        .push(format!("Steam {}: {error}", path.display())),
                }
            }
        }
    }
    /// Adds the games described by the Epic launcher's `.item` manifests in one
    /// directory. A directory that does not exist adds nothing.
    pub fn epic(&mut self, directory: &Path) {
        if !directory.exists() {
            return;
        }
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) => {
                self.warnings.push(format!("Epic manifests: {error}"));
                return;
            }
        };
        let mut files: Vec<_> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "item")
            })
            .collect();
        files.sort();
        for path in files {
            let result = (|| -> Result<Option<Game>> {
                let value = json(&path)?;
                // An entry that says it is not an application is not a game folder.
                if value.get("bIsApplication").and_then(Value::as_bool) == Some(false) {
                    return Ok(None);
                }
                let key = value
                    .get("AppName")
                    .and_then(Value::as_str)
                    .context("Missing AppName")?;
                let title = value
                    .get("DisplayName")
                    .and_then(Value::as_str)
                    .context("Missing DisplayName")?;
                let folder = value
                    .get("InstallLocation")
                    .and_then(Value::as_str)
                    .context("Missing InstallLocation")?;
                let mut game = installed(
                    Launcher::Epic,
                    key.into(),
                    title.into(),
                    folder.into(),
                    value
                        .get("AppVersionString")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                )?;
                if value.get("bIsIncompleteInstall").and_then(Value::as_bool) == Some(true) {
                    game.state = InstallState::Busy(BusyReason::LauncherBusy(
                        "Epic installation incomplete".into(),
                    ));
                }
                Ok(Some(game))
            })();
            match result {
                Ok(Some(game)) => self.games.push(game),
                Ok(None) => {}
                Err(error) => self
                    .warnings
                    .push(format!("Epic {}: {error}", path.display())),
            }
        }
    }
    /// Adds the games in Heroic's three `installed.json` files under its config
    /// directory, one per store backend.
    pub fn heroic(&mut self, root: &Path) {
        for (relative, launcher) in [
            (
                "legendaryConfig/legendary/installed.json",
                Launcher::HeroicLegendary,
            ),
            ("gog_store/installed.json", Launcher::HeroicGog),
            ("nile_config/installed.json", Launcher::HeroicNile),
        ] {
            let path = root.join(relative);
            if !path.exists() {
                continue;
            }
            let value = match json(&path) {
                Ok(value) => value,
                Err(error) => {
                    self.warnings
                        .push(format!("Heroic {}: {error}", path.display()));
                    continue;
                }
            };
            // The list may sit under an `installed` key or be the document itself,
            // and may be an array or an object keyed by app name. The object key is
            // the fallback id when an entry has no name. In an array the install
            // path is, since a position shifts when an earlier game is removed.
            let installed = value.get("installed").unwrap_or(&value);
            let entries: Vec<_> = match installed {
                Value::Array(entries) => {
                    entries.iter().map(|value| (String::new(), value)).collect()
                }
                Value::Object(entries) => entries
                    .iter()
                    .map(|(key, value)| (key.clone(), value))
                    .collect(),
                _ => {
                    self.warnings.push(format!(
                        "Heroic {}: expected installed games",
                        path.display()
                    ));
                    continue;
                }
            };
            for (key, item) in entries {
                let result = heroic_game(key, item, launcher);
                match result {
                    Ok(game) => self.games.push(game),
                    Err(error) => self
                        .warnings
                        .push(format!("Heroic {}: {error}", path.display())),
                }
            }
        }
    }
    /// Merges games that share an install directory. Call once, after every provider.
    pub fn finish(mut self) -> Self {
        self.games = desktop::merge(self.games);
        crate::model::flag_swallowing_games(&mut self.games);
        self
    }
}
/// Reads one `appmanifest_*.acf`. `library` is the `steamapps` directory holding it.
fn steam_game(path: &Path, library: &Path) -> Result<Game> {
    let bytes = desktop::read_bounded(path, 4 * 1024 * 1024)?;
    let manifest = vdf::parse(&String::from_utf8_lossy(&bytes))?;
    let app = manifest.get_u32("appid").context("Missing Steam appid")?;
    let title = manifest.get_str("name").context("Missing Steam name")?;
    let folder = manifest
        .get_str("installdir")
        .context("Missing Steam directory")?;
    // `installdir` must be one plain name, so it cannot be absolute, contain
    // `..` or nest. If the folder exists it must also resolve inside `common`
    // once links are followed.
    ensure!(
        {
            let mut parts = Path::new(folder).components();
            matches!(parts.next(), Some(std::path::Component::Normal(_))) && parts.next().is_none()
        },
        "Steam directory escapes its library"
    );
    let common = library.join("common");
    let install = common.join(folder);
    if install.exists() {
        ensure!(
            install.canonicalize()?.starts_with(common.canonicalize()?),
            "Steam directory escapes its library"
        );
    }
    let mut game = installed(
        Launcher::Steam,
        app.to_string(),
        title.into(),
        install,
        manifest.get_str("buildid").map(str::to_owned),
    )?;
    // The flags and tool rules are the Linux reader's, so a manifest means the
    // same thing on every platform. A missing `StateFlags` reads as 0, which is
    // not fully installed.
    let flags = manifest.get_u32("StateFlags").unwrap_or(0);
    let pending = |to: &str, done: &str| {
        manifest.get_u64(done).unwrap_or(0) < manifest.get_u64(to).unwrap_or(0)
    };
    let transfer = pending("BytesToDownload", "BytesDownloaded")
        || pending("BytesToStage", "BytesStaged")
        || library
            .parent()
            .is_some_and(|root| crate::model::steam_state::staging_in_progress(root, app));
    // A game that cannot be listed idle keeps the reason `installed` gave.
    if game.state.is_idle() {
        game.state = crate::model::steam_state::classify(flags, transfer);
    }
    game.is_tool = crate::model::steam_state::is_tool(app, title);
    game.size_hint = manifest
        .get_str("SizeOnDisk")
        .and_then(|value| value.parse().ok());
    Ok(game)
}
/// Reads one Heroic entry. Only `install_path` is required. The title falls back
/// to the folder name, then to the id.
fn heroic_game(key: String, item: &Value, launcher: Launcher) -> Result<Game> {
    let path = item
        .get("install_path")
        .and_then(Value::as_str)
        .context("Missing install_path")?;
    let key = ["app_name", "appName"]
        .iter()
        .find_map(|name| item.get(*name).and_then(Value::as_str))
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or(if key.is_empty() { path.to_owned() } else { key });
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| key.clone());
    let build = item
        .get("version")
        .or_else(|| item.get("build_id"))
        .or_else(|| item.get("buildId"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    installed(launcher, key, title, path.into(), build)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    #[test]
    fn registry_and_vdf_spellings_of_one_library_share_a_key() -> TestResult {
        let a = path_key(Path::new("c:/program files (x86)/steam/"), true);
        let b = path_key(Path::new("C:\\Program Files (x86)\\Steam"), true);
        check_eq(a, b, "case, slash direction and trailing slash fold")?;
        check(
            path_key(Path::new("/Games"), false) != path_key(Path::new("/games"), false),
            "folding is off where paths are case sensitive",
        )
    }
    #[test]
    fn steam_manifests_read_like_the_linux_reader() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let root = fixture.path().join("Steam");
        let steamapps = root.join("steamapps");
        let manifests: [(u32, &str, &str, &str); 5] = [
            (10, "Half done", "Half", "\"StateFlags\" \"0\""),
            (11, "Broken one", "Broke", "\"StateFlags\" \"36\""),
            (
                228_980,
                "Steamworks Common Redistributables",
                "Shared",
                "\"StateFlags\" \"4\"",
            ),
            (13, "Protonwar", "Protonwar", "\"StateFlags\" \"4\""),
            (
                14,
                "Fetching",
                "Fetch",
                "\"StateFlags\" \"4\" \"BytesToDownload\" \"100\" \"BytesDownloaded\" \"5\"",
            ),
        ];
        for (appid, name, dir, extra) in manifests {
            std::fs::create_dir_all(steamapps.join("common").join(dir)).ctx("game folder")?;
            std::fs::write(
                steamapps.join(format!("appmanifest_{appid}.acf")),
                format!("\"AppState\" {{ \"appid\" \"{appid}\" \"name\" \"{name}\" \"installdir\" \"{dir}\" {extra} }}"),
            )
            .ctx("manifest")?;
        }
        let mut catalog = Catalog::default();
        catalog.steam(vec![root]);
        let find = |title: &str| catalog.games.iter().find(|g| g.title == title);
        check(
            !find("Half done").ctx("half done")?.state.is_idle(),
            "StateFlags 0 is not fully installed",
        )?;
        check(
            !find("Broken one").ctx("broken")?.state.is_idle(),
            "files missing is not idle",
        )?;
        check(
            find("Steamworks Common Redistributables")
                .ctx("tool")?
                .is_tool,
            "the redistributables are a tool",
        )?;
        check(
            !find("Protonwar").ctx("protonwar")?.is_tool,
            "a game named Proton... is not a tool",
        )?;
        check(
            !find("Fetching").ctx("fetching")?.state.is_idle(),
            "pending bytes keep the game busy",
        )
    }
    #[test]
    fn heroic_array_entries_are_keyed_by_name_or_path() -> TestResult {
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let heroic = fixture.path().join("heroic/gog_store");
        std::fs::create_dir_all(&heroic).ctx("heroic")?;
        let one = fixture.path().join("games/one");
        let two = fixture.path().join("games/two");
        for dir in [&one, &two] {
            std::fs::create_dir_all(dir).ctx("game")?;
        }
        let write = |entries: serde_json::Value| {
            std::fs::write(heroic.join("installed.json"), entries.to_string())
        };
        let ids = |this: &mut Catalog| -> Vec<String> {
            this.heroic(&fixture.path().join("heroic"));
            this.games.iter().map(|g| g.id.key.clone()).collect()
        };
        write(serde_json::json!({"installed":[
            {"appName":"named","install_path":one},
            {"install_path":two}]}))
        .ctx("write")?;
        let first = ids(&mut Catalog::default());
        check_eq(first.first().map(String::as_str), Some("named"), "appName")?;
        write(serde_json::json!({"installed":[{"install_path":two}]})).ctx("rewrite")?;
        let second = ids(&mut Catalog::default());
        check_eq(
            second.first(),
            first.get(1),
            "an unnamed game keeps its id when another is uninstalled",
        )
    }
    #[test]
    fn providers_isolate_bad_manifests_keep_aliases_and_block_updates() -> TestResult {
        let fixture = tempfile::tempdir().ctx("discovery fixture")?;
        let root = fixture.path().join("Steam");
        let library = root.join("steamapps");
        let game = library.join("common/Space Game");
        std::fs::create_dir_all(&game).ctx("game")?;
        std::fs::write(library.join("appmanifest_42.acf"), "\"AppState\" { \"appid\" \"42\" \"name\" \"Space Game\" \"installdir\" \"Space Game\" \"StateFlags\" \"1028\" }").ctx("Steam manifest")?;
        std::fs::write(
            library.join("appmanifest_99.acf"),
            "\"AppState\" { \"appid\" \"99\" \"name\" \"Escape\" \"installdir\" \"../..\" }",
        )
        .ctx("escaping Steam manifest")?;
        let epic = fixture.path().join("Epic");
        std::fs::create_dir(&epic).ctx("Epic")?;
        std::fs::write(epic.join("good.item"), serde_json::to_vec(&serde_json::json!({"AppName":"epic-key", "DisplayName":"Space Game", "InstallLocation":game})).ctx("Epic JSON")?).ctx("Epic manifest")?;
        std::fs::write(epic.join("bad.item"), b"{").ctx("bad Epic manifest")?;
        let heroic = fixture.path().join("heroic/gog_store");
        std::fs::create_dir_all(&heroic).ctx("Heroic")?;
        let missing = fixture.path().join("Offline Game");
        std::fs::write(heroic.join("installed.json"), serde_json::to_vec(&serde_json::json!({"installed":[{"app_name":"77","install_path":missing,"title":"Offline"}]})).ctx("Heroic JSON")?).ctx("Heroic manifest")?;
        let mut catalog = Catalog::default();
        catalog.steam(vec![root]);
        catalog.epic(&epic);
        catalog.heroic(&fixture.path().join("heroic"));
        let catalog = catalog.finish();
        check_eq(
            catalog.games.len(),
            2,
            "aliases merge and offline game remains",
        )?;
        let steam = catalog
            .games
            .iter()
            .find(|game| game.id.launcher == Launcher::Steam)
            .ctx("Steam game")?;
        check(
            steam.ids().any(|id| id.launcher == Launcher::Epic),
            "Epic alias retained",
        )?;
        check(!steam.state.is_idle(), "Steam update prevents jobs")?;
        check_eq(
            catalog.warnings.len(),
            2,
            "provider errors do not hide other games",
        )?;
        check(
            catalog.games.iter().any(|game| !game.install_dir.exists()),
            "offline install retained",
        )
    }
    #[test]
    fn a_launcher_path_that_is_a_filesystem_root_is_listed_broken() -> TestResult {
        let root = installed(
            Launcher::Epic,
            "root".into(),
            "Root".into(),
            // `/` is not an absolute path on Windows.
            PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" }),
            None,
        )
        .ctx("a root is listed, not dropped")?;
        check(
            matches!(root.state, InstallState::Broken { .. }),
            "a launcher that names the drive root cannot be started as a game",
        )?;
        let fixture = tempfile::tempdir().ctx("fixture")?;
        let game = installed(
            Launcher::Epic,
            "ok".into(),
            "Ok".into(),
            fixture.path().to_path_buf(),
            None,
        )
        .ctx("a folder")?;
        check(game.state.is_idle(), "control: an ordinary folder is idle")
    }
}
