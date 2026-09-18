//! Shared desktop shell for targets without a storage backend yet.

use anyhow::Result;
use iced::widget::{column, container, text};
use iced::{Element, Length, Task, Theme};

use super::theme;

#[derive(Default)]
struct State;

#[derive(Debug, Clone)]
enum Message {}

fn update(_state: &mut State, message: Message) -> Task<Message> {
    match message {}
}

fn view(_state: &State) -> Element<'_, Message> {
    container(
        column![
            text("Flummox").size(26),
            text("This platform does not have a transparent storage backend yet.").size(16),
            theme::muted(
                "The shared desktop application is available, but compression remains disabled until a safe native backend is implemented.",
            ),
        ]
        .spacing(12),
    )
    .padding(28)
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::app_background)
    .into()
}

fn theme_of(_state: &State) -> Theme {
    theme::theme()
}

pub fn run() -> Result<()> {
    iced::application(State::default, update, view)
        .title("Flummox")
        .theme(theme_of)
        .default_font(theme::BODY_FONT)
        .window_size((900.0, 620.0))
        .run()?;
    Ok(())
}
