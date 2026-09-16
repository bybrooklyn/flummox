//! Steam library detection.
//!
//! Reads `libraryfolders.vdf` and the `appmanifest_*.acf` files. Nothing is
//! ever written back; Steam owns those files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::model::{BusyReason, Game, GameId, InstallState, Launcher};

use super::vdf;
use super::{DetectError, Env};

/// `StateFlags` bits from Steam's `EAppState`.
pub mod state_flags {
    /// Not installed.
    pub const UNINSTALLED: u32 = 1;
    /// An update is required before the game can run.
    pub const UPDATE_REQUIRED: u32 = 2;
    /// Installed and complete.
    pub const FULLY_INSTALLED: u32 = 4;
    /// Encrypted.
    pub const ENCRYPTED: u32 = 8;
    /// Locked.
    pub const LOCKED: u32 = 16;
    /// Files are missing.
    pub const FILES_MISSING: u32 = 32;
    /// The game is running.
    pub const APP_RUNNING: u32 = 64;
    /// Files are corrupt.
    pub const FILES_CORRUPT: u32 = 128;
    /// An update is running.
    pub const UPDATE_RUNNING: u32 = 256;
    /// An update is paused.
    pub const UPDATE_PAUSED: u32 = 512;
    /// An update has started.
    pub const UPDATE_STARTED: u32 = 1024;
    /// Being uninstalled.
    pub const UNINSTALLING: u32 = 2048;
    /// A backup is running.
    pub const BACKUP_RUNNING: u32 = 4096;
    /// Being reconfigured.
    pub const RECONFIGURING: u32 = 65536;
    /// Being validated.
    pub const VALIDATING: u32 = 131_072;
    /// Files are being added.
    pub const ADDING_FILES: u32 = 262_144;
    /// Space is being preallocated.
    pub const PREALLOCATING: u32 = 524_288;
    /// Downloading.
    pub const DOWNLOADING: u32 = 1_048_576;
    /// Staging downloaded data.
    pub const STAGING: u32 = 2_097_152;
    /// Committing staged data.
    pub const COMMITTING: u32 = 4_194_304;
    /// An update is stopping.
    pub const UPDATE_STOPPING: u32 = 8_388_608;
}

/// Flags that mean Steam is actively working on the files.
const WORKING_FLAGS: &[(u32, &str)] = &[
    (state_flags::UPDATE_RUNNING, "updating"),
    (state_flags::UPDATE_PAUSED, "update paused"),
    (state_flags::UPDATE_STARTED, "update starting"),
    (state_flags::UPDATE_STOPPING, "update stopping"),
    (state_flags::UNINSTALLING, "uninstalling"),
    (state_flags::BACKUP_RUNNING, "backing up"),
    (state_flags::RECONFIGURING, "reconfiguring"),
    (state_flags::VALIDATING, "validating"),
    (state_flags::ADDING_FILES, "adding files"),
    (state_flags::PREALLOCATING, "preallocating"),
    (state_flags::DOWNLOADING, "downloading"),
    (state_flags::STAGING, "staging"),
    (state_flags::COMMITTING, "committing"),
    (state_flags::LOCKED, "locked"),
];

/// Appids that are runtimes or redistributables rather than games.
///
/// Compressing these would slow every game's startup for almost no gain, so
/// they are excluded unless the user asks for them by id.
pub const TOOL_APPIDS: &[u32] = &[
    228_980,   // Steamworks Common Redistributables
    1_070_560, // Steam Linux Runtime 1.0 (scout)
    1_391_110, // Steam Linux Runtime 2.0 (soldier)
    1_628_350, // Steam Linux Runtime 3.0 (sniper)
    1_493_710, // Proton Experimental
    2_180_100, // Proton Hotfix
    1_826_330, // Proton EasyAntiCheat Runtime
    1_887_720, // Proton 7.0
    2_348_590, // Proton 8.0
    2_805_730, // Proton 9.0
];

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
        TOOL_APPIDS.contains(&self.appid)
            || self.name.starts_with("Proton")
            || self.name.starts_with("Steam Linux Runtime")
            || self.name.starts_with("Steamworks")
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

/// The library folders configured in a Steam root.
///
/// The root itself is always a library. Both the current nested format and the
/// old flat one are accepted.
pub fn libraries(root: &Path) -> Result<Vec<PathBuf>, DetectError> {
    let mut out = vec![root.to_path_buf()];
    let path = root.join("steamapps/libraryfolders.vdf");
    let text = match std::fs::read_to_string(&path) {
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
    let text = std::fs::read_to_string(path).map_err(|e| DetectError::new(ctx(), e))?;
    let obj = vdf::parse(&text).map_err(|e| DetectError::new(ctx(), e))?;
    let appid = obj
        .get_u32("appid")
        .ok_or_else(|| DetectError::new(ctx(), "no appid"))?;
    let install_name = obj
        .get_str("installdir")
        .ok_or_else(|| DetectError::new(ctx(), "no installdir"))?;
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
        if let Ok(app) = read_app_manifest(&entry.path(), library) {
            out.push(app);
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
        root.parent().map(|p| p.join("registry.vdf")).unwrap_or_default(),
    ];
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(obj) = vdf::parse(&text) else { continue };
        if let Some(id) = obj.find_str("RunningAppID").and_then(|s| s.parse().ok())
            && id != 0
        {
            return Some(id);
        }
    }
    None
}

/// Whether Steam left files in `steamapps/{downloading,temp}/<appid>`.
///
/// Empty leftovers are normal; content means a transfer is in flight.
fn staging_in_progress(library: &Path, appid: u32) -> bool {
    ["downloading", "temp"].iter().any(|sub| {
        let dir = library.join("steamapps").join(sub).join(appid.to_string());
        std::fs::read_dir(&dir).is_ok_and(|mut e| e.next().is_some())
    })
}

/// Works out the state of a group of apps sharing one install directory.
pub fn group_state(apps: &[App], running: Option<u32>) -> InstallState {
    if apps.iter().any(|a| Some(a.appid) == running) {
        return InstallState::Busy(BusyReason::Running);
    }
    let union = apps.iter().fold(0, |acc, a| acc | a.state_flags);
    if let Some((_, what)) = WORKING_FLAGS.iter().find(|(bit, _)| union & bit != 0) {
        return InstallState::Busy(BusyReason::LauncherBusy((*what).to_owned()));
    }
    if apps.iter().any(|a| a.transfer_pending())
        || apps
            .iter()
            .any(|a| staging_in_progress(&a.library, a.appid))
    {
        return InstallState::Busy(BusyReason::LauncherBusy("transfer in progress".to_owned()));
    }
    if union & state_flags::FILES_MISSING != 0 {
        return InstallState::Broken("files missing".to_owned());
    }
    if union & state_flags::FILES_CORRUPT != 0 {
        return InstallState::Broken("files corrupt".to_owned());
    }
    if union & state_flags::UPDATE_REQUIRED != 0 {
        return InstallState::UpdatePending;
    }
    if union & state_flags::FULLY_INSTALLED == 0 {
        return InstallState::Broken("not fully installed".to_owned());
    }
    // Installed and idle as far as the flags go, except Steam sometimes
    // leaves the running bit set after a crash. `RunningAppID` already said
    // nothing is running, so this is reported as stale rather than busy.
    if union & state_flags::APP_RUNNING != 0 {
        return InstallState::Busy(BusyReason::StaleRunningFlag);
    }
    InstallState::Idle
}

/// Finds every installed Steam game.
///
/// Apps sharing an install directory (Half-Life 2 and its episodes, say) are
/// returned as one game whose `also` lists the other appids.
pub fn discover(env: &Env) -> Result<Vec<Game>, DetectError> {
    let mut groups: BTreeMap<PathBuf, Vec<App>> = BTreeMap::new();
    let mut running = None;
    for root in roots(env) {
        running = running.or_else(|| running_app_id(&root));
        for library in libraries(&root)? {
            // A library that cannot be read is a whole drive of games missing
            // from the list, so it has to reach the caller. Returning here
            // instead would let one unplugged drive hide every other library.
            let apps = match apps_in_library(&library) {
                Ok(apps) => apps,
                Err(e) => {
                    tracing::warn!(library = %library.display(), error = %e, "skipped a library");
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
                let key = app.install_dir.canonicalize().unwrap_or(app.install_dir.clone());
                groups.entry(key).or_default().push(app);
            }
        }
    }

    let mut games = Vec::new();
    for (install_dir, mut apps) in groups {
        apps.sort_by_key(|a| a.appid);
        let Some(primary) = apps.first() else { continue };
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
    Ok(games)
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
            .app(105_600, "Terraria", state_flags::FULLY_INSTALLED, "Terraria")?
            .app(220, "Half-Life 2", state_flags::FULLY_INSTALLED, "Half-Life 2")?
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
    fn finds_libraries_and_groups_shared_install_dirs() -> TestResult {
        let (_tmp, env) = fixture()?;
        let games = discover(&env).ctx("discovering games")?;
        let titles: Vec<&str> = games.iter().map(|g| g.title.as_str()).collect();
        check_eq(
            titles.as_slice(),
            ["Half-Life 2", "Steamworks Common Redistributables", "Terraria"].as_slice(),
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
        check(steamworks.is_tool, "Steamworks Common Redistributables is a tool")
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
        check_eq(group_state(&apps(installed), None), InstallState::Idle, "installed and idle")?;
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
            InstallState::Broken("files missing".to_owned()),
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
        check(app.transfer_pending(), "an unfinished download is a pending transfer")?;
        check(!app.is_tool(), "a plain app is not a tool")?;
        check_eq(
            group_state(std::slice::from_ref(&app), None),
            InstallState::Busy(BusyReason::LauncherBusy("transfer in progress".to_owned())),
            "a pending transfer makes the group busy",
        )
    }
}
