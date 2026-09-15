//! Game library detection.
//!
//! Each launcher gets a module that turns its on-disk config into
//! [`gc_core::model::Game`] values. Nothing here touches game files; it only
//! reads launcher metadata, so a scan is always safe to run.

pub mod steam;
pub mod vdf;

use std::path::PathBuf;

use gc_core::model::Game;

/// Where to look for launcher configuration.
///
/// Tests build one of these pointing at a fixture tree instead of the real
/// `$HOME`, which is why every detector takes it rather than reading the
/// environment itself.
#[derive(Debug, Clone)]
pub struct Env {
    /// The user's home directory.
    pub home: PathBuf,
}

impl Env {
    /// Builds an `Env` from `$HOME`.
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// Builds an `Env` from the current process environment.
    pub fn current() -> Option<Self> {
        std::env::var_os("HOME").map(Self::from_home)
    }
}

/// A problem that made one detector give up, without failing the whole scan.
#[derive(Debug, thiserror::Error)]
#[error("{context}: {source}")]
pub struct DetectError {
    /// What was being read, usually a file path.
    pub context: String,
    /// The underlying cause.
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

impl DetectError {
    /// Wraps an error with the path or operation it came from.
    pub fn new(
        context: impl Into<String>,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self { context: context.into(), source: source.into() }
    }
}

/// Everything one scan found: the games, plus whatever went wrong on the way.
///
/// Warnings are kept rather than raised because a broken Heroic config should
/// never hide the user's Steam library.
#[derive(Debug, Default)]
pub struct Scan {
    /// Games found, in detector order.
    pub games: Vec<Game>,
    /// Non-fatal problems, for the activity log and `doctor`.
    pub warnings: Vec<DetectError>,
}

/// Runs every detector.
pub fn scan_all(env: &Env) -> Scan {
    let mut scan = Scan::default();
    match steam::discover(env) {
        Ok(mut games) => scan.games.append(&mut games),
        Err(e) => scan.warnings.push(e),
    }
    scan
}
