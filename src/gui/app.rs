//! The window's state and the messages that change it.
//!
//! Holds no widgets, so everything here can be driven from a test with no
//! display attached. [`crate::gui::view`] is the only module that builds
//! widgets. The one `iced` type here is [`Animation`], which is arithmetic
//! over time and needs no window.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use iced::Animation;
use iced::animation::Easing;

use crate::db::{Activity, Db, GameRecord};
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

    /// Position in the sidebar, as the value the selection animates towards.
    ///
    /// Spelled out instead of derived from [`PAGES`] so it needs no cast from
    /// an index. `pages_and_slots_agree` keeps the two in step.
    pub fn slot(self) -> f32 {
        match self {
            Self::Overview => 0.0,
            Self::Games => 1.0,
            Self::Queue => 2.0,
            Self::Updates => 3.0,
            Self::Drives => 4.0,
            Self::Activity => 5.0,
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
    /// Where that filesystem is mounted.
    ///
    /// Cached because probing re-reads and re-parses `/proc/self/mountinfo`.
    /// Without this the Drives page parsed it once per game to build the
    /// list, then again per game on every frame.
    pub mountpoint: Option<PathBuf>,
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
                Self {
                    game,
                    filesystem: fs.fstype,
                    mountpoint: Some(fs.mountpoint),
                    supported,
                    note,
                }
            }
            Err(e) => Self {
                game,
                filesystem: "unknown".to_owned(),
                mountpoint: None,
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
    /// The state database, when one could be opened.
    pub db: Option<Db>,
    /// What earlier passes recorded, most recent first.
    pub records: Vec<GameRecord>,
    /// Recent log lines, for the Activity page.
    pub activity: Vec<Activity>,
    /// The sidebar selection, so it slides between entries.
    pub nav: Animation<f32>,
}

impl State {
    /// Builds the initial state by scanning for games.
    ///
    /// The database is passed in so a test can run against none, and never
    /// against the real one.
    pub fn new(env: Env, db: Option<Db>) -> Self {
        let mut state = Self {
            env,
            page: Page::Overview,
            games: Vec::new(),
            warnings: Vec::new(),
            status: None,
            db,
            records: Vec::new(),
            activity: Vec::new(),
            nav: Animation::new(Page::Overview.slot())
                .duration(Duration::from_millis(220))
                .easing(Easing::EaseOutCubic),
        };
        state.refresh();
        state
    }

    /// Rescans the launchers and reloads what earlier passes recorded.
    pub fn refresh(&mut self) {
        let scan = crate::launchers::scan_all(&self.env);
        self.warnings = scan.warnings.iter().map(ToString::to_string).collect();
        self.games = scan.games.into_iter().filter(|g| !g.is_tool).map(GameRow::probe).collect();
        if let Some(db) = &self.db {
            // An unreadable database leaves these empty, so the window shows
            // what is on disk without claiming anything was compressed.
            self.records = db.games().unwrap_or_default();
            self.activity = db.recent_activity(50).unwrap_or_default();
        }
        tracing::info!(games = self.games.len(), records = self.records.len(), "scanned");
    }

    /// Games with a recorded compression pass.
    pub fn compressed_count(&self) -> usize {
        self.records.len()
    }

    /// What earlier passes estimated they saved, summed.
    ///
    /// The estimate is shown instead of the free-space delta, which covers the
    /// whole filesystem and includes writes by other processes.
    pub fn estimated_saved(&self) -> u64 {
        self.records.iter().filter_map(|r| u64::try_from(r.est_saving).ok()).sum()
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
        for mountpoint in self.games.iter().filter_map(|row| row.mountpoint.clone()) {
            if !out.contains(&mountpoint) {
                out.push(mountpoint);
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
    /// A frame passed while something was animating.
    ///
    /// Carries nothing: the view reads the clock itself as it draws.
    Tick,
}

/// Applies a message.
///
/// Returns nothing today because every action is still instant. Compression
/// jobs will change that, and the signature will become `Task<Message>` when
/// the first one lands rather than before.
pub fn update(state: &mut State, message: Message) {
    match message {
        Message::GoTo(page) => {
            state.page = page;
            state.nav.go_mut(page.slot(), Instant::now());
        }
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
        // The frame itself is the work: receiving it redraws the window, and
        // the sidebar reads the animation afresh each time.
        Message::Tick => {}
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
        let state = State::new(Env::from_home(tmp.path()), None);
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
    fn pages_and_slots_agree() -> TestResult {
        let mut previous: Option<f32> = None;
        for page in PAGES {
            let slot = page.slot();
            let matches = PAGES.iter().filter(|other| other.slot() == slot).count();
            check_eq(matches, 1, format!("{page:?} has a slot of its own"))?;
            match previous {
                Some(last) => {
                    check(slot > last, format!("{page:?} sits after the entry before it"))?;
                }
                None => check(slot.abs() < f32::EPSILON, "the first entry sits at zero")?,
            }
            previous = Some(slot);
        }
        check(previous.is_some(), "the sidebar lists at least one page")
    }

    #[test]
    fn navigating_moves_the_selection_animation() -> TestResult {
        let (_tmp, mut state) = empty_state()?;
        update(&mut state, Message::GoTo(Page::Drives));
        let settled = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let landed = state.nav.interpolate_with(|value| value, settled);
        check(
            (landed - Page::Drives.slot()).abs() < 0.01,
            format!("the selection settles on Drives, reached {landed}"),
        )
    }

    #[test]
    fn every_page_has_a_name() -> TestResult {
        for page in PAGES {
            check(!page.label().is_empty(), format!("{page:?} needs a label"))?;
        }
        check_eq(PAGES.len(), 6, "the sidebar lists six pages")
    }
}
