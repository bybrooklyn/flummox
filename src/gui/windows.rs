//! Native Windows shell over the WOF backend.

use anyhow::Result;
use iced::futures::SinkExt;
use iced::widget::{
    Space, button, column, container, responsive, row, scrollable, text, text_input,
};
use iced::{Element, Length, Task, Theme};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::theme;

struct Status {
    error: bool,
    text: String,
}

struct State {
    games: Vec<crate::windows::InstalledGame>,
    folder: String,
    status: Option<Status>,
    working: bool,
    scanning: bool,
    progress: Option<crate::windows::Progress>,
    cancel: Option<Arc<AtomicBool>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            games: Vec::new(),
            folder: String::new(),
            status: None,
            working: false,
            scanning: true,
            progress: None,
            cancel: None,
        }
    }
}

#[derive(Debug, Clone)]
enum Message {
    Folder(String),
    Select(PathBuf),
    Refresh,
    Scanned(Vec<crate::windows::InstalledGame>),
    Optimize,
    Restore,
    Stop,
    Progress(crate::windows::Progress),
    Finished(std::result::Result<String, String>),
}

async fn background<T: Send + 'static>(
    operation: impl FnOnce() -> std::result::Result<T, String> + Send + 'static,
) -> std::result::Result<T, String> {
    let (send, receive) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _sent = send.send(operation());
    });
    receive
        .await
        .map_err(|_| "The background operation stopped unexpectedly.".to_owned())?
}

fn start(state: &mut State, optimize: bool) -> Task<Message> {
    if state.working {
        return Task::none();
    }
    let folder = PathBuf::from(state.folder.trim());
    if folder.as_os_str().is_empty() {
        state.status = Some(Status {
            error: true,
            text: "Choose an installed game folder first.".into(),
        });
        return Task::none();
    }
    if !folder.is_dir() {
        state.status = Some(Status {
            error: true,
            text: "Choose an existing installed game folder.".into(),
        });
        return Task::none();
    }
    state.working = true;
    state.progress = None;
    let cancel = Arc::new(AtomicBool::new(false));
    state.cancel = Some(cancel.clone());
    state.status = Some(Status {
        error: false,
        text: if optimize {
            "Compressing worthwhile files with Windows LZX…".into()
        } else {
            "Restoring ordinary NTFS storage…".into()
        },
    });
    let stream = iced::stream::channel(32, async move |sender| {
        std::thread::spawn(move || {
            let mut progress_sender = sender.clone();
            let result = if optimize {
                crate::windows::optimize_folder_with(&folder, &cancel, move |progress| {
                    let _sent = progress_sender.try_send(Message::Progress(progress));
                })
            } else {
                crate::windows::restore_folder_with(&folder, &cancel, move |progress| {
                    let _sent = progress_sender.try_send(Message::Progress(progress));
                })
            }
            .map_err(|error| error.to_string());
            let mut finished_sender = sender;
            let _sent =
                iced::futures::executor::block_on(finished_sender.send(Message::Finished(result)));
        });
    });
    Task::run(stream, |message| message)
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Folder(folder) => state.folder = folder,
        Message::Select(folder) => {
            state.folder = folder.display().to_string();
            state.status = None;
        }
        Message::Refresh => {
            if state.scanning || state.working {
                return Task::none();
            }
            state.scanning = true;
            return Task::perform(
                background(|| Ok(crate::windows::discover_steam())),
                |result| Message::Scanned(result.unwrap_or_default()),
            );
        }
        Message::Scanned(games) => {
            state.scanning = false;
            state.games = games;
            state.status = Some(Status {
                error: false,
                text: format!("Found {} installed Steam games.", state.games.len()),
            });
        }
        Message::Optimize => return start(state, true),
        Message::Restore => return start(state, false),
        Message::Stop => {
            if let Some(cancel) = &state.cancel {
                cancel.store(true, Ordering::Relaxed);
                state.status = Some(Status {
                    error: false,
                    text: "Stopping after the current file…".into(),
                });
            }
        }
        Message::Progress(progress) => state.progress = Some(progress),
        Message::Finished(result) => {
            let stopped = state
                .cancel
                .take()
                .is_some_and(|cancel| cancel.load(Ordering::Relaxed));
            state.working = false;
            state.status = Some(match (stopped, result) {
                (true, _) => Status {
                    error: false,
                    text: "Stopped. Files already processed remain valid.".into(),
                },
                (false, Ok(text)) => Status { error: false, text },
                (false, Err(text)) => Status { error: true, text },
            });
        }
    }
    Task::none()
}

fn view(state: &State) -> Element<'_, Message> {
    responsive(move |size| layout(state, size.width < 760.0)).into()
}

fn layout(state: &State, compact: bool) -> Element<'_, Message> {
    let hero = container(
        column![
            text("Make room. Keep playing.").size(28),
            text("Flummox uses Windows' transparent LZX storage. Games stay at the same path and launch normally.")
                .size(14),
            row![
                theme::stat(state.games.len().to_string(), "Detected games"),
                theme::stat("LZX".into(), "Storage mode")
            ]
            .spacing(32),
        ]
        .spacing(14),
    )
    .padding(22)
    .width(Length::Fill)
    .style(theme::hero);

    let mut game_list = column![
        row![
            text(format!("Steam library · {} games", state.games.len())).size(16),
            button(if state.scanning {
                "Refreshing…"
            } else {
                "Refresh"
            })
            .padding([6, 10])
            .on_press_maybe((!state.working && !state.scanning).then_some(Message::Refresh)),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center)
    ]
    .spacing(8);
    if state.games.is_empty() {
        game_list = game_list.push(
            text("No Steam installs were found. You can still paste any game folder.").size(13),
        );
    }
    for game in &state.games {
        game_list = game_list.push(
            button(
                column![
                    text(&game.title).size(14),
                    text(game.path.display().to_string()).size(11),
                ]
                .spacing(2),
            )
            .width(Length::Fill)
            .padding([9, 11])
            .on_press_maybe(
                (!state.working && !state.scanning).then(|| Message::Select(game.path.clone())),
            ),
        );
    }
    let library = container(scrollable(game_list).height(Length::Fill))
        .padding(16)
        .width(if compact {
            Length::Fill
        } else {
            Length::FillPortion(5)
        })
        .height(if compact {
            Length::Fixed(250.0)
        } else {
            Length::Fill
        })
        .style(theme::panel);

    let controls = row![
        button("Optimize")
            .padding([11, 18])
            .style(theme::action_button)
            .on_press_maybe((!state.working).then_some(Message::Optimize)),
        button("Restore")
            .padding([11, 18])
            .on_press_maybe((!state.working).then_some(Message::Restore)),
        button("Stop")
            .padding([11, 18])
            .on_press_maybe(state.working.then_some(Message::Stop)),
    ]
    .spacing(10);
    let mut action = column![
        theme::section_title("Selected folder"),
        text_input("C:\\Games\\Your game", &state.folder)
            .on_input(Message::Folder)
            .padding(12)
            .width(Length::Fill),
        text("Optimize skips tiny files and content Windows cannot shrink. Restore removes WOF backing without changing file bytes.")
            .size(12),
        controls,
    ]
    .spacing(14);
    if let Some(progress) = &state.progress {
        let freed = progress
            .allocation_before
            .saturating_sub(progress.allocation_after);
        action = action.push(
            container(
                column![
                    text(format!(
                        "{} files processed · {} changed · {} skipped",
                        progress.files, progress.changed, progress.skipped
                    ))
                    .size(13),
                    theme::muted(format!(
                        "{} scanned · {} freed so far",
                        humansize::format_size(progress.bytes, humansize::DECIMAL),
                        humansize::format_size(freed, humansize::DECIMAL)
                    )),
                ]
                .spacing(4),
            )
            .padding(12)
            .width(Length::Fill)
            .style(theme::hero),
        );
    }
    if let Some(status) = &state.status {
        action = action.push(
            container(text(&status.text).size(13))
                .padding(12)
                .width(Length::Fill)
                .style(theme::banner(status.error)),
        );
    }
    let action = container(action)
        .padding(18)
        .width(if compact {
            Length::Fill
        } else {
            Length::FillPortion(6)
        })
        .height(Length::Fill)
        .style(theme::panel);

    let workspace: Element<'_, Message> = if compact {
        column![library, action].spacing(16).into()
    } else {
        row![library, action]
            .spacing(16)
            .height(Length::Fill)
            .into()
    };

    container(
        column![
            row![
                theme::page_title("Flummox"),
                Space::new().width(Length::Fill),
                theme::muted("Windows · WOF/LZX")
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center),
            hero,
            workspace,
        ]
        .spacing(16),
    )
    .padding(24)
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::app_background)
    .into()
}

fn theme(_state: &State) -> Theme {
    theme::theme()
}

fn boot() -> (State, Task<Message>) {
    let state = State::default();
    let task = Task::perform(
        background(|| Ok(crate::windows::discover_steam())),
        |result| Message::Scanned(result.unwrap_or_default()),
    );
    (state, task)
}

/// Runs the Windows desktop app.
pub fn run() -> Result<()> {
    iced::application(boot, update, view)
        .title("Flummox")
        .theme(theme)
        .default_font(theme::BODY_FONT)
        .window_size((900.0, 620.0))
        .run()?;
    Ok(())
}
