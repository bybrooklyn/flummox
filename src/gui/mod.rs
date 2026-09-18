//! `flummox-gui`: the desktop front end.
//!
//! [`app`] holds state and schedules background tasks, [`view`] builds
//! widgets from cached data, and [`theme`] holds
//! the colours.

#[cfg(target_os = "linux")]
mod app;
mod theme;
#[cfg(not(any(target_os = "linux", windows)))]
mod unsupported;
#[cfg(target_os = "linux")]
mod view;
#[cfg(windows)]
mod windows;

use anyhow::Result;

#[cfg(target_os = "linux")]
use crate::launchers::Env;
#[cfg(target_os = "linux")]
use anyhow::Context;

/// The window's theme.
///
/// A function item rather than a closure, for the same reason `view::view` is
/// one: iced needs something that accepts a reference of *any* lifetime, and
/// an inline closure gets inferred for one specific lifetime instead.
#[cfg(target_os = "linux")]
fn theme_of(_state: &app::State) -> iced::Theme {
    theme::theme()
}

/// Frames, but only while something is moving.
///
/// Subscribing unconditionally would redraw at the display's rate forever,
/// which costs power for a window that is usually still.
#[cfg(target_os = "linux")]
fn animation_frames(state: &app::State) -> iced::Subscription<app::Message> {
    let moving = !state.reduced_motion
        && (state
            .nav
            .iter()
            .any(|(_, a)| a.is_animating(std::time::Instant::now()))
            || state.page_reveal.is_animating(std::time::Instant::now())
            || state.status_reveal.is_animating(std::time::Instant::now())
            || state.detail.is_animating(std::time::Instant::now())
            || state
                .progress
                .values()
                .any(|p| p.is_animating(std::time::Instant::now())));
    let frames = if moving || state.status_deadline.is_some() {
        iced::window::frames().map(|_| app::Message::Tick)
    } else {
        iced::Subscription::none()
    };
    let polling = if state.polling {
        iced::Subscription::run(app::polls)
    } else {
        iced::Subscription::none()
    };
    iced::Subscription::batch([
        frames,
        polling,
        iced::keyboard::listen().map(app::Message::Keyboard),
    ])
}

#[cfg(target_os = "linux")]
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
        move || {
            let mut state = app::State::new(env.clone());
            let task = app::update(&mut state, app::Message::Refresh);
            (state, task)
        },
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

#[cfg(windows)]
pub fn run() -> Result<()> {
    windows::run()
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn run() -> Result<()> {
    unsupported::run()
}
