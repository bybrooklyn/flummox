//! Native Mac and Windows desktop workflows over their filesystem backends.

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
#[cfg(target_os = "macos")]
use crate::macos as backend;
#[cfg(windows)]
use crate::windows as backend;
#[cfg(windows)]
const PLATFORM: &str = "Windows · WOF/LZX";
#[cfg(target_os = "macos")]
const PLATFORM: &str = "macOS · APFS";
#[cfg(windows)]
const MODE: &str = "LZX";
#[cfg(target_os = "macos")]
const MODE: &str = "APFS";
#[cfg(windows)]
const RECOVERY_ACTION: &str = "Restore ordinary storage";
#[cfg(target_os = "macos")]
const RECOVERY_ACTION: &str = "Restore retained original";

struct Status {
    error: bool,
    text: String,
}

struct State {
    games: Vec<crate::model::Game>,
    folder: String,
    status: Option<Status>,
    working: bool,
    scanning: bool,
    progress: Option<backend::Progress>,
    cancel: Option<Arc<AtomicBool>>,
    system_theme: iced::theme::Mode,
    planned: Option<(PathBuf, bool, crate::storage::SpacePlan)>,
    recovery: Vec<backend::Recovery>,
    qualification: Option<crate::qualification::Wizard>,
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
            system_theme: iced::theme::Mode::Dark,
            planned: None,
            recovery: vec![],
            qualification: None,
        }
    }
}

#[derive(Debug, Clone)]
enum Message {
    Folder(String),
    Select(PathBuf),
    Refresh,
    Scanned(std::result::Result<Vec<crate::model::Game>, String>),
    Optimize,
    Planned(
        PathBuf,
        bool,
        std::result::Result<crate::storage::SpacePlan, String>,
    ),
    StartPlanned,
    CancelPlanned,
    Remember,
    Qualify,
    QualificationReady(std::result::Result<Box<crate::qualification::Wizard>, String>),
    QualificationField(crate::qualification::Field, String),
    QualificationCheck(crate::qualification::Check, bool),
    QualificationMode(crate::compatibility::StorageMode),
    SaveQualification,
    CloseQualification,
    QualificationSaved(std::result::Result<String, String>),
    Remembered(std::result::Result<(), String>),
    Recover(PathBuf),
    RecoveryScanned(std::result::Result<Vec<backend::Recovery>, String>),
    Restore,
    Stop,
    Progress(backend::Progress),
    Finished(std::result::Result<String, String>),
    SystemTheme(iced::theme::Mode),
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

fn plan(state: &mut State, optimize: bool) -> Task<Message> {
    if state.working {
        return Task::none();
    }
    let folder = PathBuf::from(state.folder.trim());
    state.working = true;
    let planned_folder = folder.clone();
    Task::perform(
        background(move || {
            crate::storage::native_plan(&folder, !optimize).map_err(|error| error.to_string())
        }),
        move |result| Message::Planned(planned_folder.clone(), optimize, result),
    )
}

fn start(state: &mut State, folder: PathBuf, optimize: bool) -> Task<Message> {
    if state.working {
        return Task::none();
    }
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
            format!("Compressing worthwhile files with {MODE}…")
        } else {
            "Restoring ordinary storage…".into()
        },
    });
    let stream = iced::stream::channel(32, async move |sender| {
        std::thread::spawn(move || {
            let mut progress_sender = sender.clone();
            let result = if optimize {
                backend::optimize_folder_with(&folder, &cancel, move |progress| {
                    let _sent = progress_sender.try_send(Message::Progress(progress));
                })
            } else {
                backend::restore_folder_with(&folder, &cancel, move |progress| {
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
        Message::Qualify => {
            let path = PathBuf::from(state.folder.trim());
            if let Some(game) = state
                .games
                .iter()
                .find(|game| game.install_dir == path)
                .cloned()
            {
                return Task::perform(
                    background(move || {
                        crate::qualification::baseline(&game)
                            .map(|corpus| Box::new(crate::qualification::Wizard::new(game, corpus)))
                            .map_err(|error| error.to_string())
                    }),
                    Message::QualificationReady,
                );
            }
            state.status = Some(Status {
                error: true,
                text: "Remember or select the game folder first.".into(),
            });
        }
        Message::QualificationReady(result) => match result {
            Ok(wizard) => state.qualification = Some(*wizard),
            Err(error) => {
                state.status = Some(Status {
                    error: true,
                    text: error,
                })
            }
        },
        Message::QualificationField(field, text) => {
            if let Some(wizard) = &mut state.qualification {
                wizard.field(field, text);
            }
        }
        Message::QualificationCheck(check, value) => {
            if let Some(wizard) = &mut state.qualification {
                wizard.check(check, value);
            }
        }
        Message::QualificationMode(mode) => {
            if let Some(wizard) = &mut state.qualification {
                wizard.mode = mode;
            }
        }
        Message::CloseQualification => state.qualification = None,
        Message::SaveQualification => {
            if let Some(wizard) = &state.qualification {
                match wizard.report() {
                    Ok(report) => {
                        return Task::perform(
                            background(move || {
                                crate::compatibility::Store::local()
                                    .and_then(|store| store.save(&report))
                                    .map(|path| format!("Report saved to {}", path.display()))
                                    .map_err(|error| error.to_string())
                            }),
                            Message::QualificationSaved,
                        );
                    }
                    Err(error) => {
                        state.status = Some(Status {
                            error: true,
                            text: error.to_string(),
                        })
                    }
                }
            }
        }
        Message::QualificationSaved(result) => {
            state.qualification = None;
            state.status = Some(match result {
                Ok(text) => Status { error: false, text },
                Err(text) => Status { error: true, text },
            });
        }
        Message::Folder(folder) => {
            state.folder = folder;
            state.planned = None;
        }
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
                background(|| crate::native::discover().map_err(|error| error.to_string())),
                Message::Scanned,
            );
        }
        Message::Scanned(games) => {
            state.scanning = false;
            match games {
                Ok(games) => state.games = games,
                Err(error) => {
                    state.status = Some(Status {
                        error: true,
                        text: error,
                    });
                    return Task::none();
                }
            }
            state.status = Some(Status {
                error: false,
                text: format!("Found {} installed Steam games.", state.games.len()),
            });
        }
        Message::Optimize => return plan(state, true),
        Message::Restore => return plan(state, false),
        Message::Planned(folder, optimize, result) => {
            state.working = false;
            match result {
                Ok(plan) => state.planned = Some((folder, optimize, plan)),
                Err(error) => {
                    state.status = Some(Status {
                        error: true,
                        text: error,
                    })
                }
            }
        }
        Message::StartPlanned => {
            if let Some((folder, optimize, plan)) = state.planned.take() {
                if let Err(error) = plan.check() {
                    state.status = Some(Status {
                        error: true,
                        text: error.to_string(),
                    });
                } else {
                    return start(state, folder, optimize);
                }
            }
        }
        Message::CancelPlanned => state.planned = None,
        Message::Remember => {
            let folder = PathBuf::from(state.folder.trim());
            return Task::perform(
                background(move || {
                    crate::native::add_folder(&folder).map_err(|error| error.to_string())
                }),
                Message::Remembered,
            );
        }
        Message::Remembered(result) => match result {
            Ok(()) => return update(state, Message::Refresh),
            Err(error) => {
                state.status = Some(Status {
                    error: true,
                    text: error,
                })
            }
        },
        Message::Recover(folder) => {
            state.working = true;
            return Task::perform(
                background(move || {
                    backend::recover_folder(&folder)
                        .map(|()| "Recovery finished.".into())
                        .map_err(|error| error.to_string())
                }),
                Message::Finished,
            );
        }
        Message::RecoveryScanned(result) => match result {
            Ok(records) => state.recovery = records,
            Err(error) => {
                state.status = Some(Status {
                    error: true,
                    text: error,
                })
            }
        },
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
            return Task::perform(
                background(|| backend::recovery().map_err(|error| error.to_string())),
                Message::RecoveryScanned,
            );
        }
        Message::SystemTheme(theme) => state.system_theme = theme,
    }
    Task::none()
}

fn view(state: &State) -> Element<'_, Message> {
    responsive(move |size| layout(state, size.width < 760.0)).into()
}

fn layout(state: &State, compact: bool) -> Element<'_, Message> {
    let hero = container(
        column![
            text(format!("Save space with {MODE}")).size(28),
            theme::muted("Games stay in place and launch normally"),
            row![
                theme::stat(state.games.len().to_string(), "Games"),
                theme::stat(MODE.into(), "Mode")
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
        game_list = game_list.push(theme::muted(
            "No Steam games found. Paste a folder to continue",
        ));
    }
    for game in &state.games {
        game_list = game_list.push(
            button(
                column![
                    text(&game.title).size(14),
                    text(game.install_dir.display().to_string()).size(11),
                    text(game.state.to_string()).size(11),
                ]
                .spacing(2),
            )
            .width(Length::Fill)
            .padding([9, 11])
            .on_press_maybe(
                (!state.working && !state.scanning && game.state.is_idle())
                    .then(|| Message::Select(game.install_dir.clone())),
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
            .style(theme::secondary_button)
            .on_press_maybe((!state.working).then_some(Message::Restore)),
        button("Stop")
            .padding([11, 18])
            .style(theme::secondary_button)
            .on_press_maybe(state.working.then_some(Message::Stop)),
    ]
    .spacing(10);
    let mut action = column![
        theme::section_title("Selected folder"),
        text_input("C:\\Games\\Your game", &state.folder)
            .on_input(Message::Folder)
            .padding(12)
            .width(Length::Fill),
        theme::muted("Optimize skips files the filesystem cannot shrink"),
        controls,
        button("Qualify compatibility")
            .on_press_maybe((!state.working).then_some(Message::Qualify)),
        button("Remember this folder")
            .on_press_maybe((!state.working).then_some(Message::Remember)),
    ]
    .spacing(14);
    if let Some(wizard) = &state.qualification {
        action = action.push(crate::qualification::view(
            wizard,
            Message::QualificationField,
            Message::QualificationCheck,
            Message::QualificationMode,
            Message::SaveQualification,
            Message::CloseQualification,
        ));
    }
    if let Some((_, _, plan)) = &state.planned {
        action = action.push(theme::section_title("Storage plan"));
        for row in &plan.requirements {
            action = action.push(
                text(format!(
                    "{}: {} needed including headroom; {} available",
                    row.volume.path.display(),
                    humansize::format_size(
                        row.additional.saturating_add(row.headroom),
                        humansize::DECIMAL
                    ),
                    humansize::format_size(row.volume.available, humansize::DECIMAL)
                ))
                .size(13),
            );
        }
        if let Err(error) = plan.check() {
            action = action.push(text(error.to_string()).size(13));
        }
        action = action.push(
            row![
                button("Start job")
                    .on_press_maybe(plan.check().is_ok().then_some(Message::StartPlanned)),
                button("Cancel").on_press(Message::CancelPlanned)
            ]
            .spacing(10),
        );
    }
    if !state.recovery.is_empty() {
        action = action.push(theme::section_title("Recovery"));
        for record in &state.recovery {
            action = action.push(text(record.root.display().to_string()).size(13));
            action =
                action.push(button(RECOVERY_ACTION).on_press_maybe(
                    (!state.working).then(|| Message::Recover(record.root.clone())),
                ));
        }
    }
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
    let action = container(scrollable(action))
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
                theme::muted(format!("{PLATFORM} · {}", env!("CARGO_PKG_VERSION")))
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

fn theme(state: &State) -> Theme {
    theme::theme(state.system_theme != iced::theme::Mode::Light)
}

fn boot() -> (State, Task<Message>) {
    let state = State::default();
    let task = Task::batch([
        Task::perform(
            background(|| crate::native::discover().map_err(|error| error.to_string())),
            Message::Scanned,
        ),
        iced::system::theme().map(Message::SystemTheme),
        Task::perform(
            background(|| backend::recovery().map_err(|error| error.to_string())),
            Message::RecoveryScanned,
        ),
    ]);
    (state, task)
}

/// Runs the native desktop app.
pub fn run() -> Result<()> {
    iced::application(boot, update, view)
        .title("Flummox")
        .theme(theme)
        .subscription(|_| iced::system::theme_changes().map(Message::SystemTheme))
        .default_font(theme::BODY_FONT)
        .window_size((900.0, 620.0))
        .run()?;
    Ok(())
}
