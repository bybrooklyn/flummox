//! The data model every layer shares: games, where they came from, and
//! whether they can be touched right now.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which launcher a game was found through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Launcher {
    /// Valve's Steam client.
    Steam,
    /// Epic Games Launcher.
    Epic,
    /// GOG Galaxy or offline GOG installers.
    Gog,
    /// Heroic, Epic games (via legendary).
    HeroicLegendary,
    /// Heroic, GOG games.
    HeroicGog,
    /// Heroic, Amazon games (via nile).
    HeroicNile,
    /// Heroic, sideloaded games.
    HeroicSideload,
    /// Lutris.
    Lutris,
    /// Bottles.
    Bottles,
    /// A folder the user added by hand.
    Manual,
}

impl Launcher {
    /// The stable identifier used in [`GameId`] strings and on disk.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Steam => "steam",
            Self::Epic => "epic",
            Self::Gog => "gog",
            Self::HeroicLegendary => "heroic-epic",
            Self::HeroicGog => "heroic-gog",
            Self::HeroicNile => "heroic-amazon",
            Self::HeroicSideload => "heroic-sideload",
            Self::Lutris => "lutris",
            Self::Bottles => "bottles",
            Self::Manual => "manual",
        }
    }

    /// The name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Steam => "Steam",
            Self::Epic => "Epic Games",
            Self::Gog => "GOG",
            Self::HeroicLegendary => "Heroic (Epic)",
            Self::HeroicGog => "Heroic (GOG)",
            Self::HeroicNile => "Heroic (Amazon)",
            Self::HeroicSideload => "Heroic (sideload)",
            Self::Lutris => "Lutris",
            Self::Bottles => "Bottles",
            Self::Manual => "Manual",
        }
    }
}

/// A game's identity: which launcher, plus that launcher's own key.
///
/// Displays and parses as `steam:105600`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GameId {
    /// The launcher this key belongs to.
    pub launcher: Launcher,
    /// The launcher's own identifier: a Steam appid, an Epic app name, …
    pub key: String,
}

impl GameId {
    /// Builds an id.
    pub fn new(launcher: Launcher, key: impl Into<String>) -> Self {
        Self {
            launcher,
            key: key.into(),
        }
    }

    /// The Steam appid, if this is a Steam game.
    pub fn steam_appid(&self) -> Option<u32> {
        (self.launcher == Launcher::Steam)
            .then(|| self.key.parse().ok())
            .flatten()
    }
}

impl fmt::Display for GameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.launcher.slug(), self.key)
    }
}

/// Why a game must not be touched right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "reason", content = "detail")]
pub enum BusyReason {
    /// The launcher says the game is running.
    Running,
    /// The launcher is downloading, staging, committing or validating.
    LauncherBusy(String),
    /// A process of ours has the install directory open.
    ProcessOpen(String),
    /// Steam's "running" bit is set but nothing is actually running.
    ///
    /// Seen in the wild on this machine: `StateFlags 68` with no game up.
    /// Treated as busy in automatic mode and as a confirmation prompt in
    /// manual mode.
    StaleRunningFlag,
}

impl fmt::Display for BusyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::LauncherBusy(what) => write!(f, "{what}"),
            Self::ProcessOpen(what) => write!(f, "in use by {what}"),
            Self::StaleRunningFlag => f.write_str("marked running (possibly stale)"),
        }
    }
}

/// Whether a game can be compressed right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "state")]
pub enum InstallState {
    /// Fully installed and idle: safe to work on.
    Idle,
    /// An update is pending, so compressing now would be wasted work.
    UpdatePending,
    /// Busy for the given reason.
    Busy(BusyReason),
    /// Installed but not usable as-is (files missing, corrupt, …).
    Broken { detail: String },
}

impl InstallState {
    /// Whether a job may start without asking the user.
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

impl fmt::Display for InstallState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => f.write_str("idle"),
            Self::UpdatePending => f.write_str("update pending"),
            Self::Busy(r) => write!(f, "busy ({r})"),
            Self::Broken { detail } => write!(f, "broken ({detail})"),
        }
    }
}

/// One installed game, as found by a detector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Game {
    /// The primary id.
    pub id: GameId,
    /// Other ids sharing this install directory.
    ///
    /// Steam does this: appids 340, 380 and 420 all live in `Half-Life 2`. A
    /// directory is busy if *any* of them is busy.
    pub also: Vec<GameId>,
    /// Display name.
    pub title: String,
    /// Absolute path to the install directory.
    #[serde(with = "crate::path_serde")]
    pub install_dir: PathBuf,
    /// Build id or version string, used to notice updates.
    pub build: Option<String>,
    /// The launcher's own size figure, when it has one.
    pub size_hint: Option<u64>,
    /// Whether this is safe to work on right now.
    pub state: InstallState,
    /// True for runtimes and redistributables (Proton, the Steam Linux
    /// Runtime, …), which are excluded by default.
    pub is_tool: bool,
}

impl Game {
    /// Every id pointing at this install directory.
    pub fn ids(&self) -> impl Iterator<Item = &GameId> {
        std::iter::once(&self.id).chain(self.also.iter())
    }

    /// Whether any id matches the user's selector: an exact `launcher:key`, a
    /// bare Steam appid, or a case-insensitive substring of the title.
    pub fn matches(&self, selector: &str) -> bool {
        let sel = selector.trim();
        if self
            .ids()
            .any(|id| id.to_string().eq_ignore_ascii_case(sel))
            || self.ids().any(|id| id.key == sel)
        {
            return true;
        }
        self.title.to_lowercase().contains(&sel.to_lowercase())
    }
}

/// Why a launcher-supplied install path must not be treated as one game.
///
/// Refuses a relative path, a path with `..`, a filesystem root, the home
/// directory and every ancestor of it, and on Windows the system folders.
/// `home` is compared both as given and canonicalized. The path is compared
/// as given and, when it exists, canonicalized. Touches the filesystem only
/// to canonicalize.
pub fn refuse_install_path_with(path: &Path, home: Option<&Path>) -> Result<(), String> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err("Install path is not absolute".into());
    }
    if path.components().any(|part| part == Component::ParentDir) {
        return Err("Install path contains `..`".into());
    }
    if path.parent().is_none() {
        return Err("Install path is a filesystem root".into());
    }
    let canonical = path.canonicalize().ok();
    let mut homes: Vec<PathBuf> = Vec::new();
    if let Some(home) = home {
        homes.push(home.to_path_buf());
        if let Ok(real) = home.canonicalize() {
            homes.push(real);
        }
    }
    for home in &homes {
        let covers = |candidate: &Path| home.starts_with(candidate);
        if covers(path) || canonical.as_deref().is_some_and(covers) {
            return Err("Install path is the home folder or contains it".into());
        }
    }
    if canonical.as_deref().is_some_and(|c| c.parent().is_none()) {
        return Err("Install path resolves to a filesystem root".into());
    }
    #[cfg(windows)]
    for name in [
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
    ] {
        if let Some(system) = std::env::var_os(name)
            && Path::new(&system).starts_with(path)
        {
            return Err("Install path is a system folder or contains one".into());
        }
    }
    Ok(())
}

/// [`refuse_install_path_with`] using the current user's home directory.
pub fn refuse_install_path(path: &Path) -> Result<(), String> {
    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    refuse_install_path_with(path, home.as_deref())
}

/// Marks every `Manual` game that would swallow other games as `Broken`.
///
/// A folder is refused when another discovered game lives inside it or when
/// it holds a `steamapps` directory, since a job on it would rewrite those
/// games without their launcher state or exclusions.
pub fn flag_swallowing_games(games: &mut [Game]) {
    let dirs: Vec<PathBuf> = games.iter().map(|g| g.install_dir.clone()).collect();
    for game in games.iter_mut() {
        if game.id.launcher != Launcher::Manual || !game.state.is_idle() {
            continue;
        }
        let holds_games = dirs
            .iter()
            .any(|dir| dir != &game.install_dir && dir.starts_with(&game.install_dir));
        if holds_games || game.install_dir.join("steamapps").is_dir() {
            game.state = InstallState::Broken {
                detail: "Folder holds other games or a Steam library; add the games' own folders"
                    .into(),
            };
        }
    }
}

/// Steam's `StateFlags` and what they mean, shared by every platform's reader.
pub mod steam_state {
    use super::{BusyReason, InstallState};
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

    /// Whether Steam is still working on an app's files.
    ///
    /// Reads the same table the scan reports from, so a bit added there is
    /// honoured here without a second list to keep in step.
    pub fn is_working(flags: u32) -> bool {
        WORKING_FLAGS.iter().any(|(bit, _)| flags & bit != 0)
    }

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

    /// Whether an app is a runtime or redistributable rather than a game.
    ///
    /// Matches the appid list, then exact name shapes. A game that merely starts
    /// with "Proton" is not a tool.
    pub fn is_tool(appid: u32, name: &str) -> bool {
        if TOOL_APPIDS.contains(&appid) {
            return true;
        }
        if matches!(
            name,
            "Proton Experimental"
                | "Proton Hotfix"
                | "Proton EasyAntiCheat Runtime"
                | "Proton BattlEye Runtime"
                | "Steamworks Common Redistributables"
        ) {
            return true;
        }
        let versioned = |prefix: &str| {
            name.strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        };
        versioned("Proton ") || versioned("Steam Linux Runtime ")
    }

    /// Whether Steam left files in `steamapps/{downloading,temp}/<appid>`.
    ///
    /// Empty leftovers are normal; content means a transfer is in flight.
    pub fn staging_in_progress(library: &std::path::Path, appid: u32) -> bool {
        ["downloading", "temp"].iter().any(|sub| {
            let dir = library.join("steamapps").join(sub).join(appid.to_string());
            std::fs::read_dir(&dir).is_ok_and(|mut e| e.next().is_some())
        })
    }

    /// The state a group of apps is in, from their combined `StateFlags`.
    ///
    /// `transfer` is true when Steam still has bytes to fetch or stage. A running
    /// app is the caller's concern, since only the caller knows `RunningAppID`.
    pub fn classify(union: u32, transfer: bool) -> InstallState {
        if let Some((_, what)) = WORKING_FLAGS.iter().find(|(bit, _)| union & bit != 0) {
            return InstallState::Busy(BusyReason::LauncherBusy((*what).to_owned()));
        }
        if transfer {
            return InstallState::Busy(BusyReason::LauncherBusy("transfer in progress".to_owned()));
        }
        if union & state_flags::FILES_MISSING != 0 {
            return InstallState::Broken {
                detail: "files missing".to_owned(),
            };
        }
        if union & state_flags::FILES_CORRUPT != 0 {
            return InstallState::Broken {
                detail: "files corrupt".to_owned(),
            };
        }
        if union & state_flags::UPDATE_REQUIRED != 0 {
            return InstallState::UpdatePending;
        }
        if union & state_flags::FULLY_INSTALLED == 0 {
            return InstallState::Broken {
                detail: "not fully installed".to_owned(),
            };
        }
        // Steam sometimes leaves the running bit set after a crash.
        if union & state_flags::APP_RUNNING != 0 {
            return InstallState::Busy(BusyReason::StaleRunningFlag);
        }
        InstallState::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestResult, check, check_eq};

    fn game() -> Game {
        Game {
            id: GameId::new(Launcher::Steam, "220"),
            also: vec![GameId::new(Launcher::Steam, "380")],
            title: "Half-Life 2".to_owned(),
            install_dir: PathBuf::from("/games/Half-Life 2"),
            build: Some("123".to_owned()),
            size_hint: Some(1024),
            state: InstallState::Idle,
            is_tool: false,
        }
    }

    #[test]
    fn game_id_round_trips_through_display() -> TestResult {
        let id = GameId::new(Launcher::Steam, "105600");
        check_eq(
            id.to_string(),
            "steam:105600".to_owned(),
            "an id displays as launcher:key",
        )?;
        check_eq(
            id.steam_appid(),
            Some(105600),
            "a Steam key parses as an appid",
        )?;
        check_eq(
            GameId::new(Launcher::Lutris, "x").steam_appid(),
            None,
            "a non-Steam game has no appid",
        )
    }

    #[test]
    fn selectors_match_id_appid_and_title() -> TestResult {
        let g = game();
        check(
            g.matches("steam:220"),
            "a full launcher:key selector matches",
        )?;
        check(g.matches("220"), "a bare appid matches")?;
        // Secondary ids count, so any appid sharing the folder finds it.
        check(g.matches("steam:380"), "a secondary id matches too")?;
        check(
            g.matches("half-life"),
            "a lowercase substring of the title matches",
        )?;
        check(!g.matches("portal"), "an unrelated title does not match")
    }

    #[test]
    fn only_idle_games_may_start_a_job() -> TestResult {
        check(InstallState::Idle.is_idle(), "an idle install is idle")?;
        check(
            !InstallState::UpdatePending.is_idle(),
            "a pending update is not idle",
        )?;
        check(
            !InstallState::Busy(BusyReason::Running).is_idle(),
            "a running game is not idle",
        )
    }

    #[test]
    fn broken_install_state_has_a_serializable_detail_field() -> TestResult {
        let state = InstallState::Broken {
            detail: "files missing".into(),
        };
        let json = serde_json::to_string(&state).map_err(|error| error.to_string())?;
        check(
            json.contains("\"state\":\"broken\"") && json.contains("\"detail\":\"files missing\""),
            "the tagged state keeps its error detail",
        )?;
        check_eq(
            serde_json::from_str(&json).map_err(|error| error.to_string())?,
            state,
            "broken state round trip",
        )
    }
}
