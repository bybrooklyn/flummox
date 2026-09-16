//! The window's state and the messages that change it.
//!
//! Free of `iced` types, so everything here can be driven from a test with no
//! display attached. [`crate::gui::view`] is the only module that knows what a
//! widget is.

use std::path::PathBuf;

use crate::fsprobe::{self, Tier};
use crate::model::Game;
use crate::launchers::Env;

/// The pages in the sidebar, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// What is installed, what is compressed, what could be saved.
    Overview,
    /// Every game, with its size and what compressing it would save.
    Games,
    /// Jobs waiting or running.
    Queue,
    /// Games updated since they were last compressed.
    Updates,
    /// The drives games live on, and how each can be compressed.
    Drives,
    /// What the tool has done.
    Activity,
}

/// Every page, in the order the sidebar lists them.
pub const PAGES: [Page; 6] =
    [Page::Overview, Page::Games, Page::Queue, Page::Updates, Page::Drives, Page::Activity];

impl Page {
    /// The name shown in the sidebar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Games => "Games",
            Self::Queue => "Queue",
            Self::Updates => "Updates",
            Self::Drives => "Drives",
            Self::Activity => "Activity",
        }
    }
}

/// One game as the window needs it: the model, plus what we learned about the
/// drive it sits on.
#[derive(Debug, Clone)]
pub struct GameRow {
    /// The game itself.
    pub game: Game,
    /// The filesystem holding it.
    pub filesystem: String,
    /// Whether this tool can compress it at all.
    pub supported: bool,
    /// Why not, when it cannot.
    pub note: Option<String>,
}

impl GameRow {
    /// Looks up the drive behind a game.
    fn probe(game: Game) -> Self {
        match fsprobe::probe(&game.install_dir) {
            Ok(fs) => {
                let (supported, note) = match fsprobe::tier_for(&fs) {
                    Tier::Native(_) => (true, None),
                    Tier::Pack => (false, Some("needs the pack tier, which is not built yet".to_owned())),
                    Tier::Unsupported(why) => (false, Some(why.to_owned())),
                };
                Self { game, filesystem: fs.fstype, supported, note }
            }
            Err(e) => Self {
                game,
                filesystem: "unknown".to_owned(),
                supported: false,
                note: Some(format!("could not read the drive: {e}")),
            },
        }
    }
}

/// A message shown above the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Whether this reports a failure.
    pub is_error: bool,
    /// The sentence shown.
    pub text: String,
}

impl Status {
    /// Something went as intended.
    pub fn info(text: impl Into<String>) -> Self {
        Self { is_error: false, text: text.into() }
    }

    /// Something failed.
    pub fn error(text: impl Into<String>) -> Self {
        Self { is_error: true, text: text.into() }
    }
}

/// Everything the window displays.
pub struct State {
    /// Where to look for launchers.
    pub env: Env,
    /// The page on show.
    pub page: Page,
    /// Every game found, with its drive.
    pub games: Vec<GameRow>,
    /// Problems the scan hit, kept so one broken launcher cannot hide the
    /// rest of the library.
    pub warnings: Vec<String>,
    /// The banner above the page, if any.
    pub status: Option<Status>,
}

impl State {
    /// Builds the initial state by scanning for games.
    pub fn new(env: Env) -> Self {
        let mut state =
            Self { env, page: Page::Overview, games: Vec::new(), warnings: Vec::new(), status: None };
        state.refresh();
        state
    }

    /// Rescans the launchers.
    pub fn refresh(&mut self) {
        let scan = crate::launchers::scan_all(&self.env);
        self.warnings = scan.warnings.iter().map(ToString::to_string).collect();
        self.games = scan.games.into_iter().filter(|g| !g.is_tool).map(GameRow::probe).collect();
        tracing::info!(games = self.games.len(), "scanned");
    }

    /// Games this tool can actually work on.
    pub fn supported_games(&self) -> impl Iterator<Item = &GameRow> {
        self.games.iter().filter(|row| row.supported)
    }

    /// The total size of every game found.
    pub fn total_bytes(&self) -> u64 {
        self.games.iter().filter_map(|row| row.game.size_hint).sum()
    }

    /// The drives games were found on.
    pub fn drives(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for row in &self.games {
            if let Ok(fs) = fsprobe::probe(&row.game.install_dir)
                && !out.contains(&fs.mountpoint)
            {
                out.push(fs.mountpoint);
            }
        }
        out
    }
}

/// Everything that can change the window.
#[derive(Debug, Clone)]
pub enum Message {
    /// Show a different page.
    GoTo(Page),
    /// Rescan the launchers.
    Refresh,
    /// Dismiss the banner.
    Dismiss,
}

/// Applies a message.
///
/// Returns nothing today because every action is still instant. Compression
/// jobs will change that, and the signature will become `Task<Message>` when
/// the first one lands rather than before.
pub fn update(state: &mut State, message: Message) {
    match message {
        Message::GoTo(page) => state.page = page,
        Message::Refresh => {
            state.refresh();
            // A launcher that could not be read is worth saying out loud: the
            // list is incomplete and nothing about it looks wrong.
            state.status = Some(if state.warnings.is_empty() {
                Status::info(format!("Found {} games.", state.games.len()))
            } else {
                Status::error(format!(
                    "Found {} games, but {} launcher(s) could not be read. See Overview.",
                    state.games.len(),
                    state.warnings.len()
                ))
            });
        }
        Message::Dismiss => state.status = None,
    }
}

#[cfg(test)]
mod tests {
    use crate::testutil::{TestResult, check, check_eq};

    use super::*;

    /// A state with no launchers to find, so the tests do not depend on what
    /// happens to be installed.
    fn empty_state() -> Result<(tempfile::TempDir, State), String> {
        let tmp = tempfile::TempDir::new().map_err(|e| e.to_string())?;
        let state = State::new(Env::from_home(tmp.path()));
        Ok((tmp, state))
    }

    #[test]
    fn starts_on_the_overview_page() -> TestResult {
        let (_tmp, state) = empty_state()?;
        check_eq(state.page, Page::Overview, "the window opens on Overview")?;
        check(state.games.is_empty(), "an empty home has no games")
    }

    #[test]
    fn navigation_changes_the_page() -> TestResult {
        let (_tmp, mut state) = empty_state()?;
        update(&mut state, Message::GoTo(Page::Drives));
        check_eq(state.page, Page::Drives, "GoTo switches page")
    }

    #[test]
    fn the_banner_can_be_dismissed() -> TestResult {
        let (_tmp, mut state) = empty_state()?;
        update(&mut state, Message::Refresh);
        check(state.status.is_some(), "a refresh reports what it found")?;
        update(&mut state, Message::Dismiss);
        check(state.status.is_none(), "dismissing clears the banner")
    }

    #[test]
    fn every_page_has_a_name() -> TestResult {
        for page in PAGES {
            check(!page.label().is_empty(), format!("{page:?} needs a label"))?;
        }
        check_eq(PAGES.len(), 6, "the sidebar lists six pages")
    }
}
