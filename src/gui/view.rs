//! Turns [`crate::app::State`] into widgets.
//!
//! This module only reads state and builds elements; anything that changes
//! the world goes through a [`Message`] and [`crate::app::update`].

use std::time::Instant;

use humansize::{DECIMAL, format_size};
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Length};

use super::app::{GameRow, Message, PAGES, Page, State};
use super::theme;

/// Width of the navigation strip.
const SIDEBAR_WIDTH: f32 = 190.0;

/// The whole window.
pub fn view(state: &State) -> Element<'_, Message> {
    let body = row![sidebar(state, Instant::now()), content(state)];
    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::app_background)
        .into()
}

/// The page list down the left.
///
/// `now` is read once per frame so every entry interpolates against the same
/// instant, which keeps the highlight one moving shape.
fn sidebar(state: &State, now: Instant) -> Element<'_, Message> {
    let position = state.nav.interpolate_with(|slot| slot, now);
    let mut items = column![text("Flummox").size(20)].spacing(6).padding(16);
    for page in PAGES {
        // Full strength where the selection has arrived, fading out across the
        // one entry either side of it.
        let highlight = (1.0 - (position - page.slot()).abs()).clamp(0.0, 1.0);
        items = items.push(nav_item(page, highlight, page == state.page));
    }
    container(items)
        .width(Length::Fixed(SIDEBAR_WIDTH))
        .height(Length::Fill)
        .style(theme::sidebar)
        .into()
}

/// One entry in the sidebar.
fn nav_item(page: Page, highlight: f32, selected: bool) -> Element<'static, Message> {
    button(text(page.label()).size(15))
        .width(Length::Fill)
        .padding(10)
        .style(theme::nav_button(highlight))
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
        Page::Updates => updates(state),
        Page::Drives => drives(state),
        Page::Activity => activity(state),
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
        theme::stat(state.compressed_count().to_string(), "compressed so far"),
        theme::stat(format_size(state.estimated_saved(), DECIMAL), "estimated saving"),
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
        .push(
            button(text("Rescan"))
                .padding(10)
                .style(theme::action_button)
                .on_press(Message::Refresh),
        )
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

/// Games the launcher has updated since the last pass.
///
/// A build string that no longer matches the one recorded means Steam has
/// written new files, and those files are not compressed yet.
fn updates(state: &State) -> Element<'_, Message> {
    let mut list = column![theme::page_title("Updates")].spacing(10);
    let mut found = 0usize;
    for row_data in &state.games {
        let Some(record) = state.records.iter().find(|r| r.id == row_data.game.id) else {
            continue;
        };
        if record.build == row_data.game.build {
            continue;
        }
        found += 1;
        let entry = column![
            text(row_data.game.title.clone()).size(15),
            theme::muted(format!(
                "compressed at zstd {}, and the game has changed since",
                record.level
            )),
        ]
        .spacing(2);
        list = list.push(container(entry).padding(12).width(Length::Fill).style(theme::panel));
    }
    if found == 0 {
        return list.push(theme::muted("Every compressed game is up to date.")).into();
    }
    list.into()
}

/// What the tool has done, newest first.
fn activity(state: &State) -> Element<'_, Message> {
    let mut list = column![theme::page_title("Activity")].spacing(10);
    if state.activity.is_empty() {
        return list
            .push(theme::muted("Nothing recorded yet. Compress a game and it appears here."))
            .into();
    }
    for entry in &state.activity {
        let mut line = column![text(entry.message.clone()).size(15)].spacing(2);
        if entry.bytes_delta != 0 {
            let size = format_size(entry.bytes_delta.unsigned_abs(), DECIMAL);
            let note =
                if entry.bytes_delta < 0 { format!("{size} freed") } else { format!("{size} used") };
            line = line.push(theme::muted(note));
        }
        list = list.push(container(line).padding(12).width(Length::Fill).style(theme::panel));
    }
    list.into()
}
