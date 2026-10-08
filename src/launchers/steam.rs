//! Steam library detection.
//!
//! Reads `libraryfolders.vdf` and the `appmanifest_*.acf` files. Nothing is
//! ever written back; Steam owns those files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::model::{BusyReason, Game, GameId, InstallState, Launcher};

use super::vdf;
use super::{DetectError, Env};

pub use crate::model::steam_state::{TOOL_APPIDS, is_working, state_flags};

/// One installed app, straight from its `appmanifest_*.acf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// Steam appid.
    pub appid: u32,
    /// Display name.
    pub name: String,
    /// Raw `StateFlags`.
    pub state_flags: u32,
    /// Absolute install directory (`steamapps/common/<installdir>`).
    pub install_dir: PathBuf,
    /// Installed build id.
    pub build: Option<String>,
    /// The build Steam wants to reach; differs when an update is pending.
    pub target_build: Option<String>,
    /// Steam's own size figure.
    pub size_on_disk: Option<u64>,
    /// Bytes still to download (`to`, `done`).
    pub download: (u64, u64),
    /// Bytes still to stage (`to`, `done`).
    pub stage: (u64, u64),
    /// The library folder this app belongs to.
    pub library: PathBuf,
}

impl App {
    /// Whether this is a runtime or redistributable rather than a game.
    pub fn is_tool(&self) -> bool {
        crate::model::steam_state::is_tool(self.appid, &self.name)
    }

    /// Whether Steam still has bytes to fetch or stage for this app.
    pub fn transfer_pending(&self) -> bool {
        self.download.1 < self.download.0 || self.stage.1 < self.stage.0
    }
}

/// Every Steam installation root on this machine, de-duplicated.
///
/// Covers the native install, both legacy symlinks, Flatpak and snap.
pub fn roots(env: &Env) -> Vec<PathBuf> {
    let home = &env.home;
    let candidates = [
        home.join(".local/share/Steam"),
        home.join(".steam/root"),
        home.join(".steam/steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        home.join(".var/app/com.valvesoftware.Steam/data/Steam"),
        home.join("snap/steam/common/.local/share/Steam"),
    ];
    let mut out: Vec<PathBuf> = Vec::new();
    for c in candidates {
        if !c.join("steamapps").is_dir() {
            continue;
        }
        let canonical = c.canonicalize().unwrap_or(c);
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    out
}

/// Largest manifest read. Steam's own are a few kilobytes.
const MANIFEST_LIMIT: u64 = 16 * 1024 * 1024;

/// Reads a Steam text file, replacing bytes that are not UTF-8.
///
/// Older manifests hold game names in the system code page. Failing on those
/// dropped the game, and an unbounded read let one huge file stall a scan.
fn read_manifest_text(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MANIFEST_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MANIFEST_LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "larger than any Steam manifest",
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The library folders configured in a Steam root.
///
/// The root itself is always a library. Both the current nested format and the
/// old flat one are accepted.
pub fn libraries(root: &Path) -> Result<Vec<PathBuf>, DetectError> {
    let mut out = vec![root.to_path_buf()];
    let path = root.join("steamapps/libraryfolders.vdf");
    let text = match read_manifest_text(&path) {
        Ok(t) => t,
        // A fresh install may not have the file yet; the root still counts.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(DetectError::new(path.display().to_string(), e)),
    };
    let obj = vdf::parse(&text).map_err(|e| DetectError::new(path.display().to_string(), e))?;
    for (_, value) in obj.entries() {
        let entry = match value {
            // Current format: "0" { "path" "/games" ... }
            vdf::Value::Obj(o) => o.get_str("path").map(PathBuf::from),
            // Old format: "1" "/games"
            vdf::Value::Str(s) => Some(PathBuf::from(s)),
        };
        let Some(dir) = entry else { continue };
        if !dir.join("steamapps").is_dir() {
            continue;
        }
        let canonical = dir.canonicalize().unwrap_or(dir);
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    Ok(out)
}

/// Reads one `appmanifest_*.acf`.
pub fn read_app_manifest(path: &Path, library: &Path) -> Result<App, DetectError> {
    let ctx = || path.display().to_string();
    let text = read_manifest_text(path).map_err(|e| DetectError::new(ctx(), e))?;
    let obj = vdf::parse(&text).map_err(|e| DetectError::new(ctx(), e))?;
    let appid = obj
        .get_u32("appid")
        .ok_or_else(|| DetectError::new(ctx(), "no appid"))?;
    let install_name = obj
        .get_str("installdir")
        .ok_or_else(|| DetectError::new(ctx(), "no installdir"))?;
    // A manifest on a shared or removable library must not name a folder
    // outside it, such as `../../..` or an absolute path.
    if install_name.is_empty()
        || !Path::new(install_name)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return Err(DetectError::new(ctx(), "installdir leaves the library"));
    }
    Ok(App {
        appid,
        name: obj.get_str("name").unwrap_or("").to_owned(),
        state_flags: obj.get_u32("StateFlags").unwrap_or(0),
        install_dir: library.join("steamapps/common").join(install_name),
        build: obj.get_str("buildid").map(str::to_owned),
        target_build: obj.get_str("TargetBuildID").map(str::to_owned),
        size_on_disk: obj.get_u64("SizeOnDisk"),
        download: (
            obj.get_u64("BytesToDownload").unwrap_or(0),
            obj.get_u64("BytesDownloaded").unwrap_or(0),
        ),
        stage: (
            obj.get_u64("BytesToStage").unwrap_or(0),
            obj.get_u64("BytesStaged").unwrap_or(0),
        ),
        library: library.to_path_buf(),
    })
}

/// Every app manifest in a library folder.
pub fn apps_in_library(library: &Path) -> Result<Vec<App>, DetectError> {
    apps_in_library_noting(library, &mut Vec::new())
}

/// [`apps_in_library`], recording each manifest it could not read.
fn apps_in_library_noting(
    library: &Path,
    skipped: &mut Vec<DetectError>,
) -> Result<Vec<App>, DetectError> {
    let steamapps = library.join("steamapps");
    let entries = std::fs::read_dir(&steamapps)
        .map_err(|e| DetectError::new(steamapps.display().to_string(), e))?;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("appmanifest_") || !name.ends_with(".acf") {
            continue;
        }
        // One unreadable manifest must not hide the rest of the library.
        match read_app_manifest(&entry.path(), library) {
            Ok(app) => out.push(app),
            Err(error) => skipped.push(error),
        }
    }
    out.sort_by_key(|a| a.appid);
    Ok(out)
}

/// The appid Steam currently reports as running, from `registry.vdf`.
///
/// The key only exists while a game is up, so `None` is the normal state.
pub fn running_app_id(root: &Path) -> Option<u32> {
    // The registry lives next to the root, not inside it.
    let candidates = [
        root.join("registry.vdf"),
        root.parent()
            .map(|p| p.join("registry.vdf"))
            .unwrap_or_default(),
    ];
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(obj) = vdf::parse(&text) else { continue };
        if let Some(id) = obj.find_str("RunningAppID").and_then(|s| s.parse().ok())
            && id != 0
        {
            return Some(id);
        }
    }
    None
}

/// Works out the state of a group of apps sharing one install directory.
pub fn group_state(apps: &[App], running: Option<u32>) -> InstallState {
    if apps.iter().any(|a| Some(a.appid) == running) {
        return InstallState::Busy(BusyReason::Running);
    }
    let union = apps.iter().fold(0, |acc, a| acc | a.state_flags);
    let transfer = apps.iter().any(|a| {
        a.transfer_pending() || crate::model::steam_state::staging_in_progress(&a.library, a.appid)
    });
    crate::model::steam_state::classify(union, transfer)
}

/// Finds every installed Steam game.
///
/// Apps sharing an install directory (Half-Life 2 and its episodes, say) are
/// returned as one game whose `also` lists the other appids.
pub fn discover(env: &Env) -> Result<Vec<Game>, DetectError> {
    Ok(discover_noting(env, &mut Vec::new()))
}

/// [`discover`], recording every root, library and manifest it had to skip.
///
/// Nothing here ends the scan. A malformed `libraryfolders.vdf` in the native
/// root used to hide the Flatpak root behind it, and a skipped library or
/// manifest reached only the log.
pub fn discover_noting(env: &Env, skipped: &mut Vec<DetectError>) -> Vec<Game> {
    let mut groups: BTreeMap<PathBuf, Vec<App>> = BTreeMap::new();
    let mut running = None;
    for root in roots(env) {
        running = running.or_else(|| running_app_id(&root));
        let found = libraries(&root).unwrap_or_else(|error| {
            skipped.push(error);
            // The root is a library whatever its list of others says.
            vec![root.clone()]
        });
        for library in found {
            // A library that cannot be read is a whole drive of games missing
            // from the list, so it has to reach the caller. Returning here
            // instead would let one unplugged drive hide every other library.
            let apps = match apps_in_library_noting(&library, skipped) {
                Ok(apps) => apps,
                Err(e) => {
                    tracing::warn!(library = %library.display(), error = %e, "skipped a library");
                    skipped.push(e);
                    continue;
                }
            };
            for app in apps {
                if app.state_flags & state_flags::UNINSTALLED != 0 {
                    continue;
                }
                if !app.install_dir.is_dir() {
                    continue;
                }
                let key = app
                    .install_dir
                    .canonicalize()
                    .unwrap_or(app.install_dir.clone());
                groups.entry(key).or_default().push(app);
            }
        }
    }

    let mut games = Vec::new();
    for (install_dir, mut apps) in groups {
        apps.sort_by_key(|a| a.appid);
        let Some(primary) = apps.first() else {
            continue;
        };
        let state = group_state(&apps, running);
        games.push(Game {
            id: GameId::new(Launcher::Steam, primary.appid.to_string()),
            also: apps
                .iter()
                .skip(1)
                .map(|a| GameId::new(Launcher::Steam, a.appid.to_string()))
                .collect(),
            title: if primary.name.is_empty() {
                primary.appid.to_string()
            } else {
                primary.name.clone()
            },
            install_dir,
            build: primary.build.clone(),
            size_hint: apps.iter().filter_map(|a| a.size_on_disk).max(),
            state,
            is_tool: apps.iter().all(App::is_tool),
        });
    }
    games.sort_by_key(|g| g.title.to_lowercase());
    games
}

#[cfg(test)]
mod tests {
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    use super::*;

    /// Builds a Steam root fixture: `library(..)`, then `app(..)` per app.
    struct FakeSteam {
        root: PathBuf,
    }

    impl FakeSteam {
        fn new(base: &Path) -> Result<Self, String> {
            let root = base.join(".local/share/Steam");
            std::fs::create_dir_all(root.join("steamapps")).ctx("creating steamapps")?;
            std::fs::write(
                root.join("steamapps/libraryfolders.vdf"),
                format!(
                    "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
                    root.display()
                ),
            )
            .ctx("writing libraryfolders.vdf")?;
            Ok(Self { root })
        }

        fn app(
            &self,
            appid: u32,
            name: &str,
            flags: u32,
            installdir: &str,
        ) -> Result<&Self, String> {
            std::fs::create_dir_all(self.root.join("steamapps/common").join(installdir))
                .ctx("creating the install directory")?;
            let acf = format!(
                "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"{name}\"\n\t\
                 \"StateFlags\"\t\t\"{flags}\"\n\t\"installdir\"\t\t\"{installdir}\"\n\t\
                 \"buildid\"\t\t\"1000\"\n\t\"SizeOnDisk\"\t\t\"2048\"\n}}\n"
            );
            std::fs::write(
                self.root.join(format!("steamapps/appmanifest_{appid}.acf")),
                acf,
            )
            .ctx("writing the app manifest")?;
            Ok(self)
        }
    }

    fn fixture() -> Result<(tempfile::TempDir, Env), String> {
        let tmp = tempfile::tempdir().ctx("creating a temporary directory")?;
        let steam = FakeSteam::new(tmp.path())?;
        steam
            .app(
                105_600,
                "Terraria",
                state_flags::FULLY_INSTALLED,
                "Terraria",
            )?
            .app(
                220,
                "Half-Life 2",
                state_flags::FULLY_INSTALLED,
                "Half-Life 2",
            )?
            // Shares the Half-Life 2 folder, and carries the stale running bit
            // this machine actually has.
            .app(
                340,
                "Half-Life 2: Lost Coast",
                state_flags::FULLY_INSTALLED | state_flags::APP_RUNNING,
                "Half-Life 2",
            )?
            .app(
                228_980,
                "Steamworks Common Redistributables",
                state_flags::FULLY_INSTALLED,
                "Steamworks Shared",
            )?;
        let env = Env::from_home(tmp.path());
        Ok((tmp, env))
    }

    #[test]
    fn bad_manifests_are_reported_and_do_not_hide_the_rest() -> TestResult {
        let (tmp, env) = fixture()?;
        let steamapps = tmp.path().join(".local/share/Steam/steamapps");
        check(steamapps.is_dir(), "fixture layout")?;
        let manifest = |appid: u32, name: &[u8], dir: &str| {
            let mut text =
                format!("\"AppState\"\n{{\n\"appid\" \"{appid}\"\n\"name\" \"").into_bytes();
            text.extend_from_slice(name);
            text.extend_from_slice(
                format!("\"\n\"StateFlags\" \"4\"\n\"installdir\" \"{dir}\"\n}}\n").as_bytes(),
            );
            std::fs::write(steamapps.join(format!("appmanifest_{appid}.acf")), text)
        };
        // A Latin-1 name, which is not UTF-8.
        manifest(900, b"Caf\xe9 Racer", "Cafe").ctx("latin-1 manifest")?;
        std::fs::create_dir_all(steamapps.join("common/Cafe")).ctx("game folder")?;
        manifest(901, b"Escape", "../../../../..").ctx("escaping manifest")?;
        std::fs::write(steamapps.join("appmanifest_902.acf"), b"\"AppState\" {")
            .ctx("truncated manifest")?;

        let mut skipped = Vec::new();
        let games = discover_noting(&env, &mut skipped);
        check(
            games
                .iter()
                .any(|g| g.title.starts_with("Caf") && g.title.ends_with(" Racer")),
            format!(
                "the Latin-1 game is listed: {:?}",
                games.iter().map(|g| &g.title).collect::<Vec<_>>()
            ),
        )?;
        check(
            games.iter().any(|g| g.title == "Terraria"),
            "games with good manifests are still listed",
        )?;
        check(
            !games.iter().any(|g| g.title == "Escape"),
            "a manifest that leaves its library names no game",
        )?;
        check_eq(
            skipped.len(),
            2,
            format!("both bad manifests are reported: {skipped:?}"),
        )
    }

    #[test]
    fn finds_libraries_and_groups_shared_install_dirs() -> TestResult {
        let (_tmp, env) = fixture()?;
        let games = discover(&env).ctx("discovering games")?;
        let titles: Vec<&str> = games.iter().map(|g| g.title.as_str()).collect();
        check_eq(
            titles.as_slice(),
            [
                "Half-Life 2",
                "Steamworks Common Redistributables",
                "Terraria",
            ]
            .as_slice(),
            "the discovered titles",
        )?;

        let hl2 = games
            .iter()
            .find(|g| g.title == "Half-Life 2")
            .ctx("the Half-Life 2 game")?;
        // 220 and 340 share one folder, so they are one game with two ids.
        check_eq(
            hl2.also.as_slice(),
            &[GameId::new(Launcher::Steam, "340")],
            "the other appids in the Half-Life 2 folder",
        )?;
        // The stale running bit from 340 makes the whole folder unsafe.
        check_eq(
            &hl2.state,
            &InstallState::Busy(BusyReason::StaleRunningFlag),
            "the Half-Life 2 state",
        )?;

        let terraria = games
            .iter()
            .find(|g| g.title == "Terraria")
            .ctx("the Terraria game")?;
        check_eq(&terraria.state, &InstallState::Idle, "the Terraria state")?;
        check(!terraria.is_tool, "Terraria is not a tool")?;
        let steamworks = games
            .iter()
            .find(|g| g.title.starts_with("Steamworks"))
            .ctx("the Steamworks entry")?;
        check(
            steamworks.is_tool,
            "Steamworks Common Redistributables is a tool",
        )
    }

    #[test]
    fn running_app_id_beats_the_flags() -> TestResult {
        let apps = |flags: u32| {
            vec![App {
                appid: 105_600,
                name: "Terraria".to_owned(),
                state_flags: flags,
                install_dir: PathBuf::from("/games/Terraria"),
                build: None,
                target_build: None,
                size_on_disk: None,
                download: (0, 0),
                stage: (0, 0),
                library: PathBuf::from("/nonexistent"),
            }]
        };
        let installed = state_flags::FULLY_INSTALLED;
        check_eq(
            group_state(&apps(installed), None),
            InstallState::Idle,
            "installed and idle",
        )?;
        check_eq(
            group_state(&apps(installed), Some(105_600)),
            InstallState::Busy(BusyReason::Running),
            "the running appid wins",
        )?;
        check_eq(
            group_state(&apps(installed | state_flags::UPDATE_REQUIRED), None),
            InstallState::UpdatePending,
            "the update-required flag",
        )?;
        check_eq(
            group_state(&apps(installed | state_flags::DOWNLOADING), None),
            InstallState::Busy(BusyReason::LauncherBusy("downloading".to_owned())),
            "the downloading flag",
        )?;
        check_eq(
            group_state(&apps(installed | state_flags::FILES_MISSING), None),
            InstallState::Broken {
                detail: "files missing".to_owned(),
            },
            "the files-missing flag",
        )
    }

    #[test]
    fn a_pending_transfer_counts_as_busy() -> TestResult {
        let app = App {
            appid: 1,
            name: "X".to_owned(),
            state_flags: state_flags::FULLY_INSTALLED,
            install_dir: PathBuf::from("/games/X"),
            build: None,
            target_build: None,
            size_on_disk: None,
            download: (100, 40),
            stage: (0, 0),
            library: PathBuf::from("/nonexistent"),
        };
        check(
            app.transfer_pending(),
            "an unfinished download is a pending transfer",
        )?;
        check(!app.is_tool(), "a plain app is not a tool")?;
        check_eq(
            group_state(std::slice::from_ref(&app), None),
            InstallState::Busy(BusyReason::LauncherBusy("transfer in progress".to_owned())),
            "a pending transfer makes the group busy",
        )
    }
}
