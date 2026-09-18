//! Renders cached application state. No filesystem access occurs during drawing.

use super::{
    app::{Filter, GameRow, Message, PAGES, Page, Sort, State},
    theme,
};
use crate::{
    backend::Preset,
    jobs::{Command, Job, Library, Operation, Phase},
};
use humansize::{DECIMAL, format_size};
use iced::widget::{
    Space, button, checkbox, column, container, image, pick_list, progress_bar, responsive, row,
    scrollable, stack, text, text_input,
};
use iced::{Alignment, Element, Length};

fn size(bytes: u64) -> String {
    format_size(bytes, DECIMAL)
}
fn action(label: impl Into<String>, message: Message) -> Element<'static, Message> {
    action_maybe(label, Some(message))
}
fn action_maybe(label: impl Into<String>, message: Option<Message>) -> Element<'static, Message> {
    button(text(label.into()))
        .padding([8, 14])
        .style(theme::action_button)
        .on_press_maybe(message)
        .into()
}
fn secondary(label: impl Into<String>, message: Message) -> Element<'static, Message> {
    secondary_maybe(label, Some(message))
}
fn secondary_maybe(
    label: impl Into<String>,
    message: Option<Message>,
) -> Element<'static, Message> {
    button(text(label.into()))
        .padding([7, 10])
        .style(button::secondary)
        .on_press_maybe(message)
        .into()
}
fn panel<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding(16)
        .width(Length::Fill)
        .style(theme::panel)
        .into()
}
fn hero<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding(20)
        .width(Length::Fill)
        .style(theme::hero)
        .into()
}

pub fn view(state: &State) -> Element<'_, Message> {
    responsive(move |size| layout(state, size.width < 880.0)).into()
}

fn layout(state: &State, compact: bool) -> Element<'_, Message> {
    let mut nav = column![
        text("Flummox").size(23),
        Space::new().height(16)
    ]
    .spacing(6)
    .padding(16);
    for page in PAGES {
        let selected = state.page == page;
        // A page's selected state never passes through unrelated buttons.
        nav = nav.push(
            button(text(page.label()).size(15))
                .width(Length::Fill)
                .padding(11)
                .style(theme::nav_button(if state.reduced_motion {
                    if selected { 1. } else { 0. }
                } else {
                    state
                        .nav
                        .iter()
                        .find(|(target, _)| *target == page)
                        .map(|(_, a)| a.interpolate(0., 1., std::time::Instant::now()))
                        .unwrap_or(0.)
                }))
                .on_press(Message::GoTo(page)),
        );
    }
    nav = nav
        .push(Space::new().height(Length::Fill))
        .push(theme::muted(if state.snapshot.gaming.is_some() {
            "Paused while gaming"
        } else {
            "Works in the background"
        }));
    let sidebar = container(nav)
        .width(if compact { 150 } else { 190 })
        .height(Length::Fill)
        .style(theme::sidebar);
    let mut body = column![]
        .spacing(12)
        .padding(if compact { 14 } else { 24 })
        .width(Length::Fill);
    let page = match state.page {
        Page::Overview => overview(state),
        Page::Games => games(state, compact),
        Page::Queue => queue(state),
        Page::Updates => updates(state, compact),
        Page::Drives => drives(state),
        Page::Activity => activity(state),
    };
    let page_reveal = if state.reduced_motion {
        1.0
    } else {
        state
            .page_reveal
            .interpolate(0.0, 1.0, std::time::Instant::now())
    };
    body = body.push(
        column![
            Space::new().height(Length::Fixed(12.0 * (1.0 - page_reveal))),
            scrollable(container(page).padding(iced::Padding::default().right(14)))
                .id(iced::widget::Id::new(state.page.label()))
                .height(Length::Fill)
        ]
        .height(Length::Fill),
    );
    if let Some(job) = state.active() {
        body = body.push(panel(
            row![
                column![
                    text(format!("{} · {}", job.game.title, job.phase.label())).size(14),
                    progress(state, job)
                ]
                .spacing(6)
                .width(Length::Fill),
                secondary("View queue", Message::GoTo(Page::Queue))
            ]
            .spacing(16)
            .align_y(Alignment::Center),
        ));
    }
    let base: Element<'_, Message> = container(row![sidebar, body])
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::app_background)
        .into();
    let Some(status) = &state.status else {
        return base;
    };
    let reveal = if state.reduced_motion {
        1.0
    } else {
        state
            .status_reveal
            .interpolate(0.0, 1.0, std::time::Instant::now())
    };
    let toast = container(
        row![
            column![
                text(if status.is_error {
                    "Needs attention"
                } else {
                    "Flummox"
                })
                .size(13),
                text(&status.text).size(14)
            ]
            .spacing(4)
            .width(Length::Fill),
            button(text("Dismiss").size(12))
                .style(button::text)
                .padding([5, 7])
                .on_press(Message::Dismiss)
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding(14)
    .width(Length::Fill)
    .max_width(480)
    .style(theme::toast(status.is_error, reveal));
    let overlay = container(toast)
        .padding(
            iced::Padding::default()
                .top(18.0 + 12.0 * (1.0 - reveal))
                .right(18),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::End)
        .align_y(Alignment::Start);
    stack([base, overlay.into()])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn overview(state: &State) -> Element<'_, Message> {
    let compressed = state
        .games
        .iter()
        .filter(|g| state.compressed(&g.game))
        .count();
    let mut content = column![
        theme::page_title("Overview"),
        theme::muted("Keep your games. Make more room."),
        panel(
            row![
                theme::stat(size(state.total_bytes()), "Installed"),
                theme::stat(state.games.len().to_string(), "Games"),
                theme::stat(compressed.to_string(), "Compressed")
            ]
            .spacing(32)
        ),
        hero(
            column![
                text(if state.potential_saving() > 0 {
                    format!("{} ready to recover", size(state.potential_saving()))
                } else {
                    "Your library, ready when you are".into()
                })
                .size(19),
                theme::muted(if state.scanning {
                    "Finding your games…"
                } else {
                    "Flummox chooses worthwhile games and keeps them playable."
                }),
                action(
                    if state.potential_saving() > 0 {
                        format!("Free up about {}", size(state.potential_saving()))
                    } else if state.scanning || state.analysis_queuing() {
                        "Analyzing library…".into()
                    } else {
                        "Check library".into()
                    },
                    if state.potential_saving() > 0 {
                        Message::OptimizeLibrary
                    } else {
                        Message::GoTo(Page::Games)
                    }
                )
            ]
            .spacing(12)
        ),
        theme::section_title("Your drives")
    ]
    .spacing(16);
    for drive in &state.drives {
        content = content.push(panel(
            row![
                text(drive.path.display().to_string()).width(Length::Fill),
                text(
                    drive
                        .free
                        .map(size)
                        .map(|s| format!("{s} free"))
                        .unwrap_or_else(|| "Drive unavailable".into())
                )
            ]
            .spacing(12),
        ));
    }
    let recent: Vec<_> = state
        .snapshot
        .jobs
        .iter()
        .rev()
        .filter(|j| j.operation != Operation::Analyze)
        .take(3)
        .collect();
    if !recent.is_empty() {
        content = content.push(theme::section_title("Recent work"));
    }
    for job in recent {
        content = content.push(panel(row![
            text(&job.game.title).width(Length::Fill),
            theme::muted(job.phase.label())
        ]));
    }
    if !state.warnings.is_empty() {
        content = content.push(panel(
            column![
                theme::section_title("Some information is unavailable"),
                theme::muted(state.warnings.join("\n")),
                secondary("Refresh", Message::Refresh)
            ]
            .spacing(8),
        ));
    }
    content.into()
}

fn games(state: &State, compact: bool) -> Element<'_, Message> {
    let filtered = state.filtered();
    let search_and_sort: Element<'_, Message> = if compact {
        column![
            text_input("Search your games", &state.query)
                .id("game-search")
                .on_input(Message::Query)
                .padding(10)
                .width(Length::Fill),
            row![
                pick_list(
                    [
                        Filter::All,
                        Filter::Ready,
                        Filter::Compressed,
                        Filter::Attention
                    ],
                    Some(state.filter),
                    Message::Filter
                ),
                pick_list(
                    [Sort::Name, Sort::Size, Sort::Saving],
                    Some(state.sort),
                    Message::Sort
                )
            ]
            .spacing(8)
        ]
        .spacing(8)
        .into()
    } else {
        row![
            text_input("Search your games", &state.query)
                .id("game-search")
                .on_input(Message::Query)
                .padding(10)
                .width(Length::Fill),
            pick_list(
                [
                    Filter::All,
                    Filter::Ready,
                    Filter::Compressed,
                    Filter::Attention
                ],
                Some(state.filter),
                Message::Filter
            ),
            pick_list(
                [Sort::Name, Sort::Size, Sort::Saving],
                Some(state.sort),
                Message::Sort
            )
        ]
        .spacing(8)
        .into()
    };
    let mut content = column![
        row![
            theme::page_title("Games"),
            Space::new().width(Length::Fill),
            secondary(
                if state.scanning {
                    "Refreshing…"
                } else {
                    "Refresh"
                },
                Message::Refresh
            )
        ]
        .align_y(Alignment::Center),
        search_and_sort,
    ]
    .spacing(12);
    let mut drive_options = vec!["All drives".to_owned()];
    drive_options.extend(state.drives.iter().map(|d| d.path.display().to_string()));
    let selected_drive = state
        .drive_filter
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "All drives".into());
    let mut launcher_options = vec!["All launchers".to_owned()];
    for game in &state.games {
        let label = game.game.id.launcher.label().to_owned();
        if !launcher_options.contains(&label) {
            launcher_options.push(label);
        }
    }
    content = content.push(
        row![
            pick_list(drive_options, Some(selected_drive), |value| {
                Message::DriveFilter((value != "All drives").then(|| value.into()))
            }),
            pick_list(
                launcher_options,
                Some(
                    state
                        .launcher_filter
                        .clone()
                        .unwrap_or_else(|| "All launchers".into())
                ),
                |value| Message::LauncherFilter((value != "All launchers").then_some(value))
            )
        ]
        .spacing(8),
    );
    let selected = state.actionable_selection().len();
    if selected > 0 {
        content = content.push(panel(
            row![
                text(format!("{selected} selected")).width(Length::Fill),
                action("Compress selected", Message::Queue(Operation::Compress))
            ]
            .align_y(Alignment::Center),
        ));
    }
    if filtered.is_empty() {
        return content
            .push(panel(
                column![
                    text(if state.scanning {
                        "Finding your games…"
                    } else {
                        "No games match"
                    })
                    .size(18),
                    theme::muted("Try another search, or add a game folder in Drives."),
                    secondary("Add a folder", Message::GoTo(Page::Drives))
                ]
                .spacing(10),
            ))
            .into();
    }
    for game in filtered.iter().take(state.shown) {
        content = content.push(game_row(state, game, compact));
    }
    if filtered.len() > state.shown {
        content = content.push(secondary(
            format!("Show more · {} games", filtered.len()),
            Message::ShowMore,
        ));
    }
    content.into()
}

fn game_row<'a>(state: &'a State, item: &'a GameRow, compact: bool) -> Element<'a, Message> {
    let game = &item.game;
    let id = game.id.to_string();
    let compressed = state.compressed(game);
    let artwork_size = if compact { 44 } else { 52 };
    let icon: Element<'a, Message> = match &item.artwork {
        Some(path) => image(path.clone())
            .width(artwork_size)
            .height(artwork_size)
            .content_fit(iced::ContentFit::Cover)
            .into(),
        None => container(text(game.title.chars().next().unwrap_or('F').to_string()).size(23))
            .center(artwork_size)
            .style(theme::panel)
            .into(),
    };
    let status = item.note.clone().unwrap_or_else(|| {
        if let Some(job) = state.latest(game)
            && (job.phase.active()
                || matches!(
                    job.phase,
                    Phase::Partial | Phase::Failed | Phase::Interrupted | Phase::Cancelled
                ))
        {
            return job.phase.label().to_owned();
        }
        if compressed {
            "Compressed".into()
        } else {
            game.id.launcher.label().into()
        }
    });
    let saving = state
        .recommendation(game)
        .map(|choice| {
            if choice.predicted_saving == 0 {
                "Little extra space expected".into()
            } else {
                format!(
                    "~{} · {}",
                    size(choice.predicted_saving),
                    choice.mode.label()
                )
            }
        })
        .unwrap_or_else(|| {
            if compressed {
                "Up to date".into()
            } else {
                "Not analyzed yet".into()
            }
        });
    let check_id = id.clone();
    let mut line = row![
        checkbox(state.selected.contains(&id)).on_toggle_maybe(
            (item.supported && game.state.is_idle() && !state.pending.contains(&id))
                .then_some(move |value| Message::Select(check_id.clone(), value)),
        ),
        icon,
        button(column![text(&game.title).size(16), theme::muted(status)].spacing(4))
            .style(button::text)
            .width(Length::Fill)
            .on_press(Message::Expand(id.clone())),
        column![
            text(
                game.size_hint
                    .map(size)
                    .unwrap_or_else(|| "Size pending".into())
            ),
            theme::muted(saving)
        ]
        .spacing(4)
        .align_x(Alignment::End)
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    if item.supported {
        line = line.push(action_maybe(
            if compressed { "Recheck" } else { "Compress" },
            (!state.pending.contains(&id)).then(|| {
                Message::One(
                    id.clone(),
                    if compressed {
                        Operation::Analyze
                    } else {
                        Operation::Compress
                    },
                )
            }),
        ));
    }
    let mut contents = column![line].spacing(12);
    if state.expanded.as_ref() == Some(&id)
        && (state.detail.value()
            || (!state.reduced_motion && state.detail.is_animating(std::time::Instant::now())))
    {
        let mut details = column![theme::muted(format!(
            "{} · {}",
            item.filesystem,
            game.install_dir.display()
        ))]
        .spacing(10);
        let preset_id = id.clone();
        if item.supported {
            details = details
                .extend(
                    [row![
                        theme::muted("Compression"),
                        pick_list(
                            [Preset::Fast, Preset::Balanced, Preset::Max],
                            Some(state.preset_for(&id)),
                            move |preset| Message::Preset(preset_id.clone(), preset)
                        )
                    ]
                    .spacing(12)]
                    .into_iter()
                    .map(Element::from),
                )
                .push(theme::muted(
                    "Balanced is recommended. Your games remain playable.",
                ))
                .push(
                    row![
                        secondary("Analyze", Message::One(id.clone(), Operation::Analyze)),
                        secondary(
                            "Decompress",
                            Message::One(id.clone(), Operation::Decompress)
                        )
                    ]
                    .spacing(8),
                );
        }
        if item.pack_supported {
            details = details.push(secondary(
                if state.advanced.contains(&id) {
                    "Hide advanced storage"
                } else {
                    "Advanced storage"
                },
                Message::ToggleAdvanced(id.clone()),
            ));
        }
        if item.pack_supported && state.advanced.contains(&id) {
            if let Some(install) = state
                .snapshot
                .packs
                .iter()
                .find(|install| install.game_path == game.install_dir)
            {
                details = details
                    .push(text("Writable pack store").size(16))
                    .push(theme::muted(format!(
                        "{} · {}",
                        install.phase.label(),
                        install.message
                    )));
                if let Some(summary) = &install.summary {
                    let used = summary.archive_bytes.saturating_sub(summary.shared_bytes);
                    let difference = summary.logical_bytes.abs_diff(used);
                    let result = if used <= summary.logical_bytes {
                        format!("{} smaller", size(difference))
                    } else {
                        format!("{} larger", size(difference))
                    };
                    details = details
                    .push(theme::muted(format!(
                        "{} additional store data for {} of game files · {result}",
                        size(used),
                        size(summary.logical_bytes)
                    )))
                    .push(theme::muted(format!(
                        "{} stayed raw · {} compressed to {} · {} deduplicated · {} zero-filled",
                        size(summary.raw_bytes),
                        size(summary.compressed_input_bytes),
                        size(summary.compressed_bytes),
                        size(summary.duplicate_bytes),
                        size(summary.zero_bytes)
                    )));
                    if summary.shared_bytes > 0 {
                        details = details.push(theme::muted(format!(
                            "{} shares physical allocation with another game store",
                            size(summary.shared_bytes)
                        )));
                    }
                }
                details = details
                    .push(theme::muted(format!(
                        "Store: {} · updates: {}",
                        install.store_path.display(),
                        install.writes_path.display()
                    )))
                    .push(if let Some(previous) = &install.previous_store_path {
                        Element::from(theme::muted(format!(
                            "Previous store retained: {}",
                            previous.display()
                        )))
                    } else {
                        Element::from(theme::muted(
                            "Compact after large launcher updates to fold them into the store.",
                        ))
                    })
                    .push(
                        row![
                            if install.previous_store_path.is_some() {
                                Element::from(theme::muted("Test this version before reclaiming"))
                            } else {
                                secondary_maybe(
                                    "Compact updates",
                                    (!state.pending.contains(&id)).then(|| {
                                        Message::Send(Command::PackCompact {
                                            game_path: game.install_dir.clone(),
                                        })
                                    }),
                                )
                            },
                            if install.previous_store_path.is_some() {
                                if state.confirm_prune.contains(&id) {
                                    secondary_maybe(
                                        "Confirm delete previous",
                                        (!state.pending.contains(&id)).then(|| {
                                            Message::Send(Command::PackPrune {
                                                game_path: game.install_dir.clone(),
                                            })
                                        }),
                                    )
                                } else {
                                    secondary_maybe(
                                        "Reclaim previous version",
                                        (!state.pending.contains(&id))
                                            .then(|| Message::PackPrunePrompt(id.clone())),
                                    )
                                }
                            } else {
                                Element::from(Space::new().width(0))
                            }
                        ]
                        .spacing(8),
                    )
                    .push(
                        row![
                            secondary_maybe(
                                "Restore ordinary files",
                                (!state.pending.contains(&id)).then(|| {
                                    Message::Send(Command::PackRollback {
                                        game_path: game.install_dir.clone(),
                                    })
                                })
                            ),
                            if install.backup_path.is_some() {
                                if state.confirm_reclaim.contains(&id) {
                                    secondary_maybe(
                                        "Confirm delete original",
                                        (!state.pending.contains(&id)).then(|| {
                                            Message::Send(Command::PackReclaim {
                                                game_path: game.install_dir.clone(),
                                            })
                                        }),
                                    )
                                } else {
                                    secondary_maybe(
                                        "Reclaim original",
                                        (!state.pending.contains(&id))
                                            .then(|| Message::PackReclaimPrompt(id.clone())),
                                    )
                                }
                            } else {
                                Element::from(theme::muted("Original reclaimed"))
                            }
                        ]
                        .spacing(8),
                    );
            } else {
                let path_id = id.clone();
                let store = state.pack_paths.get(&id).cloned().unwrap_or_default();
                details = details
                    .push(text("Writable pack store").size(16))
                    .push(theme::muted(
                        "Uses verified content-defined chunks and shares matching data with stores in the same folder.",
                    ))
                    .push(
                        text_input("Store path", &store)
                            .on_input(move |path| Message::PackPath(path_id.clone(), path))
                            .padding(10),
                    )
                    .push(
                        row![
                            action_maybe(
                                "Create & activate",
                                (!state.pending.contains(&id))
                                    .then(|| Message::PackActivate(id.clone(), true))
                            ),
                            secondary_maybe(
                                "Activate existing",
                                (!state.pending.contains(&id))
                                    .then(|| Message::PackActivate(id.clone(), false))
                            )
                        ]
                        .spacing(8),
                    );
            }
        }
        if let Some(note) = &item.note {
            details = details.push(theme::muted(note));
        }
        details = details.push(secondary(
            "Exclude",
            Message::Send(Command::Exclude {
                id: id.clone(),
                excluded: true,
            }),
        ));
        if let Some(est) = state.estimate(game) {
            if let Some(choice) = state.recommendation(game) {
                details = details
                    .push(text(choice.mode.label()).size(16))
                    .push(theme::muted(format!(
                        "{} · native ~{}{}",
                        choice.confidence.label(),
                        size(choice.native_saving),
                        choice
                            .maximum_saving
                            .map(|saving| format!(" · Maximum Space ~{}", size(saving)))
                            .unwrap_or_default()
                    )))
                    .push(theme::muted(choice.reasons.join("\n")));
            }
            details = details.push(theme::muted(format!(
                "Sampled {} · {} files inspected · {} skipped. Savings are estimates{}.",
                size(est.sampled),
                est.inspected_files,
                est.skipped_files,
                if est.already_compressed_mount {
                    " of additional space on an already compressed drive"
                } else {
                    ""
                }
            )));
            let evidence = est.format_evidence;
            details = details.push(theme::muted(format!(
                "Format evidence: {} recognized · {} unknown · {} encoded · {} containers · {} raw media{}.",
                evidence.recognized_files,
                evidence.unknown_files,
                evidence.encoded_files,
                evidence.container_files,
                evidence.raw_media_files,
                if evidence.encrypted_files > 0 {
                    format!(
                        " · {} encrypted and still sampled",
                        evidence.encrypted_files
                    )
                } else {
                    String::new()
                }
            )));
        }
        if let Some(est) = state.estimate(game)
            && est.unsampled_files > 0
        {
            details = details.push(theme::muted(format!(
                "Partial estimate: {} files were outside the sampling budget.",
                est.unsampled_files
            )));
        }
        if let Some(job) = state.latest(game)
            && !job.errors.is_empty()
        {
            details = details.push(theme::muted(job.errors.join("\n")));
        }
        let reveal = if state.reduced_motion {
            1.0
        } else {
            state
                .detail
                .interpolate(0.0, 1.0, std::time::Instant::now())
        };
        contents = contents.push(column![
            Space::new().height(Length::Fixed(10.0 * (1.0 - reveal))),
            container(details)
        ]);
    }
    panel(contents)
}

fn progress<'a>(state: &'a State, job: &'a Job) -> Element<'a, Message> {
    let fraction = if job.bytes_total > 0 {
        job.bytes_done as f32 / job.bytes_total as f32
    } else {
        0.
    };
    progress_bar(
        0.0..=1.0,
        if state.reduced_motion {
            fraction
        } else {
            state
                .progress
                .get(&job.id)
                .map(|p| p.interpolate_with(|v| v, std::time::Instant::now()))
                .unwrap_or(fraction)
        }
        .clamp(0., 1.),
    )
    .girth(5)
    .into()
}
fn job_row<'a>(state: &'a State, job: &'a Job) -> Element<'a, Message> {
    let mut controls = row![].spacing(8);
    if job.phase.active() {
        controls = controls
            .push(secondary(
                if job.user_paused { "Resume" } else { "Pause" },
                Message::Send(Command::Pause {
                    id: job.id,
                    paused: !job.user_paused,
                }),
            ))
            .push(secondary("Cancel", Message::Send(Command::Cancel(job.id))));
    } else if job.phase != Phase::Completed {
        controls = controls.push(secondary("Resume", Message::Send(Command::Retry(job.id))));
    }
    let kind = match job.operation {
        Operation::Analyze => "Analysis",
        Operation::Compress => "Compression",
        Operation::Decompress => "Decompression",
    };
    let mut content = column![
        row![
            text(&job.game.title).size(16).width(Length::Fill),
            theme::muted(format!("{kind} · {}", job.phase.label()))
        ],
        progress(state, job),
        theme::muted(&job.message),
        row![
            theme::muted(format!(
                "{} / {} files · {} processed · {}s",
                job.files_done,
                job.files_total,
                size(job.bytes_done),
                job.elapsed
            ))
            .width(Length::Fill),
            controls
        ]
        .spacing(12)
    ]
    .spacing(10);
    if let Some(change) = job.drive_change {
        content = content.push(theme::muted(format!(
            "Drive free-space change: {}{}. Includes other applications' disk activity.",
            if change >= 0 { "+" } else { "−" },
            size(change.unsigned_abs())
        )));
    }
    if !job.errors.is_empty() {
        content = content.push(theme::muted(job.errors.join("\n")));
    }
    panel(content)
}
fn queue(state: &State) -> Element<'_, Message> {
    let mut content = column![
        theme::page_title("Queue"),
        theme::muted("You can close this window. Your jobs will keep running.")
    ]
    .spacing(14);
    if let Some(game) = &state.snapshot.gaming {
        content = content.push(panel(text(format!("Paused while you play {game}"))));
    }
    let jobs: Vec<_> = state
        .snapshot
        .jobs
        .iter()
        .filter(|j| j.operation != Operation::Analyze && j.phase.active())
        .collect();
    if jobs.is_empty() {
        content = content.push(panel(
            column![
                text("Nothing waiting"),
                theme::muted("Choose games and press Compress to add work here."),
                action("Choose games", Message::GoTo(Page::Games))
            ]
            .spacing(10),
        ));
    }
    for job in jobs {
        content = content.push(job_row(state, job));
    }
    for job in state
        .snapshot
        .jobs
        .iter()
        .rev()
        .filter(|j| j.operation != Operation::Analyze && !j.phase.active())
        .take(20)
    {
        content = content.push(job_row(state, job));
    }
    content.into()
}
fn updates(state: &State, compact: bool) -> Element<'_, Message> {
    let mut content = column![
        theme::page_title("Updates"),
        theme::muted("Only new or changed files need processing again.")
    ]
    .spacing(14);
    let mut count = 0;
    for game in &state.games {
        if state
            .records
            .iter()
            .any(|r| r.id == game.game.id && r.build != game.game.build)
        {
            count += 1;
            content = content.push(game_row(state, game, compact));
        }
    }
    if count == 0 {
        content = content.push(panel(text("No updates need attention.")));
    }
    content.into()
}
fn drives(state: &State) -> Element<'_, Message> {
    let mut content = column![
        theme::page_title("Drives & libraries"),
        theme::muted(
            "Automatic maintenance is opt-in. Enabling it also starts Flummox at sign-in."
        ),
        panel(
            column![
                text("Add a game folder").size(17),
                row![
                    text_input("Folder path", &state.folder)
                        .on_input(Message::Folder)
                        .padding(10)
                        .width(Length::Fill),
                    action("Add folder", Message::AddFolder)
                ]
                .spacing(8)
            ]
            .spacing(10)
        )
    ]
    .spacing(14);
    let mut paths: Vec<_> = state
        .snapshot
        .libraries
        .iter()
        .map(|l| (l.path.clone(), l.custom))
        .collect();
    for game in &state.games {
        let path = game
            .game
            .install_dir
            .parent()
            .unwrap_or(&game.game.install_dir)
            .to_path_buf();
        if !paths.iter().any(|(p, _)| *p == path) {
            paths.push((path, false));
        }
    }
    for (path, custom) in paths {
        let automatic = state
            .snapshot
            .libraries
            .iter()
            .any(|l| l.path == path && l.automatic);
        let label = path.display().to_string();
        content = content.push(panel(
            column![
                text(label),
                checkbox(automatic)
                    .label("Maintain new installs and updates")
                    .on_toggle(move |enabled| Message::Send(Command::Library(Library {
                        path: path.clone(),
                        automatic: enabled,
                        custom
                    })))
            ]
            .spacing(10),
        ));
    }
    for id in &state.snapshot.excluded {
        content = content.push(panel(row![
            theme::muted(format!("Excluded: {id}")).width(Length::Fill),
            secondary(
                "Restore",
                Message::Send(Command::Exclude {
                    id: id.clone(),
                    excluded: false
                })
            )
        ]));
    }
    content = content.push(panel(
        checkbox(state.reduced_motion)
            .label("Reduce motion")
            .on_toggle(Message::ReducedMotion),
    ));
    content.into()
}
fn activity(state: &State) -> Element<'_, Message> {
    let mut content = column![theme::page_title("Activity")].spacing(14);
    for job in state
        .snapshot
        .jobs
        .iter()
        .rev()
        .filter(|j| j.operation != Operation::Analyze)
        .take(50)
    {
        content = content.push(job_row(state, job));
    }
    for entry in &state.activity {
        content = content.push(panel(text(&entry.message)));
    }
    if state.snapshot.jobs.is_empty() && state.activity.is_empty() {
        content = content.push(panel(text("Your results will appear here.")));
    }
    content.into()
}
