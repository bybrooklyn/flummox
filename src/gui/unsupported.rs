//! The window for targets without a storage backend yet.
//!
//! It draws a titled page with a card saying compression is not available and
//! the version. `shell` (sidebar, toast, About card) is compiled only for
//! Linux, Windows and macOS, so this window uses the cards, headings and
//! colours of `theme`, which every target has.

use anyhow::Result;
use iced::widget::{column, container};
use iced::{Element, Length, Task, Theme};

use super::theme;

struct State {
    /// Follows the system. Anything but Light renders dark.
    system_theme: iced::theme::Mode,
}

impl Default for State {
    fn default() -> Self {
        Self {
            system_theme: iced::theme::Mode::Dark,
        }
    }
}

#[derive(Debug, Clone)]
enum Message {
    SystemTheme(iced::theme::Mode),
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::SystemTheme(theme) => state.system_theme = theme,
    }
    Task::none()
}

fn view(_state: &State) -> Element<'_, Message> {
    let card = theme::panel_card(
        column![
            theme::section_title("Compression is not available on this system"),
            theme::muted(
                "Flummox has no storage backend for this operating system, so it changes no game files. It compresses games on Linux, Windows and macOS."
            ),
            theme::muted(format!("Version {}", env!("CARGO_PKG_VERSION"))),
        ]
        .spacing(8),
    );
    container(
        column![
            theme::page_header::<Message>("Flummox", Some("Game compression"), None),
            card
        ]
        .spacing(theme::PAGE_GAP),
    )
    .padding(28)
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::app_background)
    .into()
}

fn theme_of(state: &State) -> Theme {
    theme::theme(state.system_theme != iced::theme::Mode::Light)
}

fn boot() -> (State, Task<Message>) {
    (
        State::default(),
        iced::system::theme().map(Message::SystemTheme),
    )
}

/// Runs a window that states the version and that no backend exists. It touches
/// no game files.
pub fn run() -> Result<()> {
    iced::application(boot, update, view)
        .title("Flummox")
        .theme(theme_of)
        .subscription(|_| iced::system::theme_changes().map(Message::SystemTheme))
        .default_font(theme::BODY_FONT)
        .window_size((900.0, 620.0))
        .run()?;
    Ok(())
}
