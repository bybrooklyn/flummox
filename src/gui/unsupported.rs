//! Shared desktop shell for targets without a storage backend yet.

use anyhow::Result;
use iced::widget::{column, container, text};
use iced::{Element, Length, Task, Theme};

use super::theme;

struct State {
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
    container(
        column![
            text("Flummox").size(26),
            text("Compression is not available on this platform yet").size(16),
            theme::muted("Game files are unchanged"),
        ]
        .spacing(12),
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
