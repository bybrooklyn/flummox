//! The data model every layer shares: games, where they came from, and
//! whether they can be touched right now.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which launcher a game was found through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Launcher {
    /// Valve's Steam client.
    Steam,
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
