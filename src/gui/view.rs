//! Turns [`crate::app::State`] into widgets.
//!
//! This module only reads state and builds elements; anything that changes
//! the world goes through a [`Message`] and [`crate::app::update`].

use humansize::{DECIMAL, format_size};
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Length};

use super::app::{GameRow, Message, PAGES, Page, State};
use super::theme;

/// Width of the navigation strip.
const SIDEBAR_WIDTH: f32 = 190.0;

/// The whole window.
pub fn view(state: &State) -> Element<'_, Message> {
    let body = row![sidebar(state), content(state)];
    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::app_background)
        .into()
}

/// The page list down the left.
fn sidebar(state: &State) -> Element<'_, Message> {
    let mut items = column![text("flummox").size(18)].spacing(4).padding(16);
    for page in PAGES {
        items = items.push(nav_item(page, page == state.page));
    }
    container(items)
        .width(Length::Fixed(SIDEBAR_WIDTH))
        .height(Length::Fill)
        .style(theme::sidebar)
        .into()
}

/// One entry in the sidebar.
fn nav_item(page: Page, selected: bool) -> Element<'static, Message> {
    let label = if selected {
        text(page.label()).size(15)
    } else {
        theme::muted(page.label()).size(15)
    };
    button(label)
        .width(Length::Fill)
        .style(button::text)
        // The page you are on is not somewhere to navigate to.
        .on_press_maybe((!selected).then_some(Message::GoTo(page)))
        .into()
}

/// The banner, if any, above the selected page.
fn content(state: &State) -> Element<'_, Message> {
    let mut items: Vec<Element<'_, Message>> = Vec::new();
    if let Some(status) = &state.status {
        items.push(
            container(
                row![
                    text(status.text.clone()).width(Length::Fill),
                    button(text("Dismiss")).style(button::text).on_press(Message::Dismiss)
                ]
                .align_y(iced::Alignment::Center),
            )
            .padding(10)
            .width(Length::Fill)
            .style(theme::banner(status.is_error))
            .into(),
        );
    }
    items.push(
        container(scrollable(page(state)))
            .padding(20)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
    );
    column(items).spacing(12).padding(16).width(Length::Fill).into()
}

/// The selected page.
fn page(state: &State) -> Element<'_, Message> {
    match state.page {
        Page::Overview => overview(state),
        Page::Games => games(state),
        Page::Queue => placeholder("Queue", "Jobs will appear here once the GUI can start them."),
        Page::Updates => {
            placeholder("Updates", "Games updated since they were last compressed will be listed here.")
        }
        Page::Drives => drives(state),
        Page::Activity => {
            placeholder("Activity", "What the tool has done, read from the state database.")
        }
    }
}

/// A page that is not built yet, said plainly rather than left blank.
fn placeholder<'a>(title: &'a str, what: &'a str) -> Element<'a, Message> {
    column![theme::page_title(title), theme::muted(what)].spacing(8).into()
}

/// Headline figures.
fn overview(state: &State) -> Element<'_, Message> {
    let supported = state.supported_games().count();
    let stats = row![
        theme::stat(state.games.len().to_string(), "games found"),
        theme::stat(format_size(state.total_bytes(), DECIMAL), "installed"),
        theme::stat(supported.to_string(), "can be compressed"),
    ]
    .spacing(40);

    let mut items = column![
        theme::page_title("Overview"),
        container(stats).padding(16).width(Length::Fill).style(theme::panel),
    ]
    .spacing(16);

    if !state.warnings.is_empty() {
        let mut list = column![theme::section_title("Problems while scanning")].spacing(4);
        for warning in &state.warnings {
            list = list.push(theme::muted(warning.clone()));
        }
        items = items.push(container(list).padding(16).width(Length::Fill).style(theme::panel));
    }

    items
        .push(button(text("Rescan")).on_press(Message::Refresh))
        .into()
}

/// Every game, largest first.
fn games(state: &State) -> Element<'_, Message> {
    let mut rows: Vec<&GameRow> = state.games.iter().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.game.size_hint.unwrap_or(0)));

    let mut list = column![theme::page_title("Games")].spacing(10);
    if rows.is_empty() {
        return list.push(theme::muted("No games found. Is Steam installed for this user?")).into();
    }
    for row_data in rows {
        list = list.push(game_row(row_data));
    }
    list.into()
}

/// One game's line.
fn game_row(row_data: &GameRow) -> Element<'_, Message> {
    let size = row_data.game.size_hint.map(|b| format_size(b, DECIMAL)).unwrap_or_default();
    let state_line = match &row_data.note {
        Some(note) => theme::muted(note.clone()),
        None => theme::muted(row_data.game.state.to_string()),
    };
    let details = column![
        text(row_data.game.title.clone()).size(15),
        state_line,
    ]
    .spacing(2)
    .width(Length::Fill);

    let figures = column![
        text(size).size(15),
        theme::muted(row_data.filesystem.clone()),
    ]
    .spacing(2)
    .align_x(iced::Alignment::End);

    container(row![details, figures].spacing(12).width(Length::Fill))
        .padding(12)
        .width(Length::Fill)
        .style(theme::panel)
        .into()
}

/// The drives games live on.
fn drives(state: &State) -> Element<'_, Message> {
    let mut list = column![theme::page_title("Drives")].spacing(10);
    for mountpoint in state.drives() {
        let games_here = state
            .games
            .iter()
            .filter(|row| row.game.install_dir.starts_with(&mountpoint))
            .count();
        let free = crate::backend::free_bytes(&mountpoint).unwrap_or(0);
        let entry = column![
            text(mountpoint.display().to_string()).size(15),
            theme::muted(format!(
                "{games_here} games   {} free",
                format_size(free, DECIMAL)
            )),
        ]
        .spacing(2);
        list = list.push(container(entry).padding(12).width(Length::Fill).style(theme::panel));
    }
    list.into()
}
