//! `flummox-gui`: the desktop front end.
//!
//! [`app`] holds state and schedules background tasks, [`view`] builds
//! widgets from cached data, and [`theme`] holds
//! the colours.

#[cfg(target_os = "linux")]
mod app;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod artwork;
#[cfg(target_os = "linux")]
mod desktop_theme;
#[cfg(target_os = "linux")]
mod dialog;
#[cfg(any(windows, target_os = "macos"))]
mod native;
#[cfg(any(windows, target_os = "macos", all(test, target_os = "linux")))]
mod native_rules;
#[cfg(all(test, target_os = "linux"))]
mod preview;
#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod preview_renderer;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod icon;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod shell;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod surface;
pub(crate) mod theme;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod unsupported;
#[cfg(target_os = "linux")]
mod view;

use anyhow::Result;

/// Where Settings > About sends a reader for what each version changed.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
const CHANGELOG_URL: &str = "https://github.com/bybrooklyn/flummox/blob/main/CHANGELOG.md";

/// Opens the changelog in the desktop's browser.
///
/// The address is a constant, so nothing a game or launcher supplies reaches
/// the helper's arguments.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn open_changelog() -> std::io::Result<()> {
    let helper = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(helper)
        .arg(CHANGELOG_URL)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

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
fn theme_of(state: &app::State) -> iced::Theme {
    shell::window_theme(
        state.system_theme,
        match state.theme {
            crate::jobs::ThemePreference::System => None,
            crate::jobs::ThemePreference::Dark => Some(true),
            crate::jobs::ThemePreference::Light => Some(false),
        },
    )
}

/// Whether any animation held in the state is still running. Always false
/// when motion is reduced.
#[cfg(target_os = "linux")]
fn animation_pending(state: &app::State) -> bool {
    let now = std::time::Instant::now();
    !state.reduced_motion
        && (state.nav.animating(now)
            || state.page_reveal.is_animating(now)
            || state.toast.animating()
            || state.detail.is_animating(now)
            || state
                .progress
                .values()
                .any(|animation| animation.is_animating(now)))
}

/// Frames, but only while something is moving.
///
/// Subscribing unconditionally would redraw at the display's rate forever,
/// which costs power for a window that is usually still.
#[cfg(target_os = "linux")]
fn animation_frames(state: &app::State) -> iced::Subscription<app::Message> {
    let moving = animation_pending(state)
        || state
            .scroll_redraw_until
            .is_some_and(|until| std::time::Instant::now() < until);
    // A toast's deadline is a timer started by `update`, not a reason for
    // frames. Every frame is a message and rebuilds the page.
    let frames = if moving {
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
        iced::system::theme_changes().map(app::Message::SystemTheme),
    ])
}

/// The log filter when `RUST_LOG` is not set. The graphics libraries log a
/// warning for each EGL and Vulkan extension a driver lacks, which says
/// nothing is wrong, so they are held to errors.
#[cfg(target_os = "linux")]
const DEFAULT_LOG_FILTER: &str = "warn,wgpu_hal=error,wgpu_core=error";

/// Opens the window and runs until it closes. Logs go to stderr at `warn`
/// unless `RUST_LOG` says otherwise.
#[cfg(target_os = "linux")]
pub fn run() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER));
    let _started = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();

    let env = Env::current().context("HOME is not set, so no game library can be found")?;
    let first_theme = desktop_theme::initial_mode();

    // `view::view` is passed as a function item, not wrapped in a closure. A
    // closure's return lifetime is inferred as a fresh one, and `ViewFn`
    // needs it tied to the argument for every lifetime.
    let result = iced::application(
        // The first scan and the desktop theme query start with the window.
        move || {
            let mut state = app::State::new(env.clone());
            state.system_theme = first_theme;
            let refresh = app::update(&mut state, app::Message::Refresh);
            let system_theme = iced::system::theme().map(app::Message::SystemTheme);
            (state, iced::Task::batch([refresh, system_theme]))
        },
        |state: &mut app::State, message: app::Message| app::update(state, message),
        view::view,
    )
    .title("Flummox")
    .subscription(animation_frames)
    .theme(theme_of)
    .default_font(theme::BODY_FONT)
    .window(iced::window::Settings {
        size: iced::Size::new(shell::WINDOW_SIZE.0, shell::WINDOW_SIZE.1),
        min_size: Some(iced::Size::new(shell::MIN_WINDOW.0, shell::MIN_WINDOW.1)),
        ..iced::window::Settings::default()
    })
    .run();
    // A picker is its own process and outlives the window unless closed.
    dialog::close_open_picker();
    result?;
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
pub fn run() -> Result<()> {
    native::run()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn run() -> Result<()> {
    unsupported::run()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::testutil::{TestResult, check};

    #[test]
    fn the_default_log_filter_quiets_the_graphics_libraries() -> TestResult {
        let filter = tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER).to_string();
        check(
            filter.contains("wgpu_hal=error") && filter.contains("wgpu_core=error"),
            format!("the probe warnings are held to errors: {filter}"),
        )?;
        check(
            filter.contains("warn"),
            format!("everything else stays at warn: {filter}"),
        )
    }
}
