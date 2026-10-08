//! Game library detection.
//!
//! Each launcher gets a module that turns its on-disk config into
//! [`crate::model::Game`] values. Nothing here touches game files; it only
//! reads launcher metadata, so a scan is always safe to run.

mod desktop;
pub(crate) mod scan_job;
pub mod steam;
pub mod vdf;

use std::path::PathBuf;

use crate::model::Game;

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
        Self {
            context: context.into(),
            source: source.into(),
        }
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
    scan_with(env, &std::sync::atomic::AtomicBool::new(false), |_| {}).unwrap_or_default()
}

pub(crate) fn scan_with(
    env: &Env,
    cancel: &std::sync::atomic::AtomicBool,
    mut emit: impl FnMut(scan_job::Event),
) -> Option<Scan> {
    use std::sync::atomic::Ordering;
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    emit(scan_job::Event::Source("Steam"));
    let mut scan = Scan::default();
    scan.games = steam::discover_noting(env, &mut scan.warnings);
    emit(scan_job::Event::Batch(scan.games.clone()));
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    emit(scan_job::Event::Source("Heroic and Lutris"));
    desktop::discover(env, &mut scan);
    emit(scan_job::Event::Batch(scan.games.clone()));
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    emit(scan_job::Event::Source("Custom locations"));
    // Fixture environments must never read the real user's custom libraries.
    if Env::current().is_some_and(|current| current.home == env.home) {
        desktop::custom(&mut scan);
    }
    desktop::merge(&mut scan);
    // Titles come from launcher files and are printed to terminals, where a
    // control character could rewrite the lines around it.
    for game in &mut scan.games {
        if game.title.chars().any(char::is_control) {
            game.title = game.title.replace(char::is_control, " ");
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    if Env::current().is_some_and(|current| current.home == env.home) {
        let libraries = crate::jobs::configured_libraries().unwrap_or_default();
        let keep = |game: &Game| {
            game.id.launcher != crate::model::Launcher::Manual
                || libraries
                    .iter()
                    .any(|library| game.install_dir.starts_with(&library.path))
        };
        match crate::libraries::data_dir()
            .and_then(|root| crate::libraries::remember(&root, scan.games.clone(), keep))
        {
            Ok(games) => scan.games = games,
            Err(error) => scan.warnings.push(DetectError::new(
                "Remembering offline libraries",
                std::io::Error::other(error.to_string()),
            )),
        }
    }
    Some(scan)
}
