//! `flummox-gui`: the desktop front end.
//!
//! [`app`] holds the state and the update function and knows nothing about
//! `iced`, [`view`] builds widgets and changes nothing, and [`theme`] holds
//! the colours.

mod app;
mod theme;
mod view;

use anyhow::{Context, Result};
use crate::launchers::Env;

/// The window's theme.
///
/// A function item rather than a closure, for the same reason `view::view` is
/// one: iced needs something that accepts a reference of *any* lifetime, and
/// an inline closure gets inferred for one specific lifetime instead.
fn theme_of(_state: &app::State) -> iced::Theme {
    theme::theme()
}

/// Frames, but only while something is moving.
///
/// Subscribing unconditionally would redraw at the display's rate forever,
/// which costs power for a window that is usually still.
fn animation_frames(state: &app::State) -> iced::Subscription<app::Message> {
    if state.nav.is_animating(std::time::Instant::now()) {
        iced::window::frames().map(|_| app::Message::Tick)
    } else {
        iced::Subscription::none()
    }
}

/// The state database, or `None` when it cannot be opened.
///
/// The window still works without it, showing what a scan finds and no
/// history.
fn open_db() -> Option<crate::db::Db> {
    let path = crate::db::Db::default_path()?;
    match crate::db::Db::open(&path) {
        Ok(db) => Some(db),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "cannot read the state database");
            None
        }
    }
}

pub fn run() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _started = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();

    let env = Env::current().context("HOME is not set, so no game library can be found")?;

    // `view::view` is passed as a function item, not wrapped in a closure: a
    // closure's return lifetime is a fresh one rather than tied to its
    // argument, which is exactly the higher-ranked bound `ViewFn` needs.
    iced::application(
        move || app::State::new(env.clone(), open_db()),
        |state: &mut app::State, message: app::Message| app::update(state, message),
        view::view,
    )
    .title("Flummox")
    .subscription(animation_frames)
    .theme(theme_of)
    .default_font(theme::BODY_FONT)
    .window_size((1100.0, 720.0))
    .run()?;
    Ok(())
}
