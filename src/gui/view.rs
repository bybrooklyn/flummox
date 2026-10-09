//! Renders cached application state. No filesystem access occurs during drawing.

use super::{
    app::{Filter, GameRow, Message, PAGES, Page, Sort, State, StorageChoice},
    shell::{self, games_count, input, picker, size},
    theme,
};
use crate::{
    backend::Preset,
    jobs::{
        Command, FolderKind, Job, Library, MotionPreference, Operation, Phase, ThemePreference,
    },
};
use iced::widget::{
    Space, button, checkbox, column, container, pick_list, progress_bar, responsive, row, text,
    text_input,
};
use iced::{Alignment, Element, Length};

// Shared pieces: byte formatting, the two button styles and the two cards.

/// The filled button for the main action of a row or page.
fn action(label: impl Into<String>, message: Message) -> Element<'static, Message> {
    theme::action(label, message)
}
/// As `action`, drawn disabled when `message` is `None`.
fn action_maybe(label: impl Into<String>, message: Option<Message>) -> Element<'static, Message> {
    theme::action_maybe(label, message)
}
/// The outlined button for every other action.
fn secondary(label: impl Into<String>, message: Message) -> Element<'static, Message> {
    theme::secondary(label, message)
}
/// As `secondary`, drawn disabled when `message` is `None`.
fn secondary_maybe(
    label: impl Into<String>,
    message: Option<Message>,
) -> Element<'static, Message> {
    theme::secondary_maybe(label, message)
}
fn panel<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    theme::panel_card(content)
}
fn hero<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    theme::hero_card(content)
}
/// Builds the whole window from `state`.
///
/// Passed to iced as a function item.
/// The layout switches to its compact form below 880 pixels of width. The
/// outer wrapper keeps frames coming while anything animates.
pub fn view(state: &State) -> Element<'_, Message> {
    super::surface::animate(
        responsive(move |size| layout(state, size.width < shell::COMPACT_BELOW)),
        super::animation_pending(state)
            || state
                .scroll_redraw_until
                .is_some_and(|until| std::time::Instant::now() < until),
    )
}

/// The window frame: sidebar, then a column of scan banner, qualification
/// wizard, space plan review, the page, and the active job bar, with the
/// toast stacked over the bottom right corner. `compact` shrinks the sidebar
/// to icons.
fn layout(state: &State, compact: bool) -> Element<'_, Message> {
    // Sidebar. Entries in `PAGES` animate their highlight. Settings sits at
    // the bottom and switches without animation.
    // Every entry, Settings included, eases its highlight the same way.
    let highlight_of = |page: Page| {
        if state.reduced_motion {
            if state.page == page { 1. } else { 0. }
        } else {
            state
                .nav
                .iter()
                .find(|(target, _)| *target == page)
                .map(|(_, a)| a.interpolate(0., 1., std::time::Instant::now()))
                .unwrap_or(0.)
        }
    };
    let nav_entry = |page: Page| -> Element<'_, Message> {
        shell::nav_entry(
            page.label(),
            page_icon(page),
            highlight_of(page),
            compact,
            Message::GoTo(page),
        )
    };
    let sidebar = shell::sidebar(
        compact,
        PAGES.into_iter().map(nav_entry).collect(),
        nav_entry(Page::Settings),
    );
    // The scan banner, the wizard and the plan each keep a slot while absent.
    // Widget state is matched by position, so a child that came and went would
    // shift the page and reset its scrolling and focus.
    let absent = || Element::from(Space::new());
    // Banner shown while the worker is discovering games.
    let mut body = column![].spacing(0).width(Length::Fill);
    body = body.push(match &state.snapshot.scan_source {
        Some(source) => Element::from(
            container(
                row![
                    theme::muted(format!(
                        "Scanning {source} · {} games found",
                        state.snapshot.discovered.len()
                    )),
                    Space::new().width(Length::Fill),
                    secondary("Cancel scan", Message::Send(Command::CancelDiscovery))
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            )
            .padding(theme::page_gutter(compact)),
        ),
        None => absent(),
    });
    // The wizard and the plan sit at the top of the page column, inside the
    // page gutter and under the page's own scrolling.
    fn below(card: Element<'_, Message>) -> Element<'_, Message> {
        container(card)
            .padding(iced::Padding::default().bottom(theme::PAGE_GAP))
            .into()
    }
    let wizard_slot = match &state.qualification {
        Some(wizard) => below(panel(crate::qualification::view(
            wizard,
            Message::QualificationField,
            Message::QualificationCheck,
            Message::QualificationMode,
            Message::MeasureQualification,
            Message::SaveQualification,
            Message::CloseQualification,
        ))),
        None => absent(),
    };
    // Space plan review. "Start job" is disabled while the plan's own check
    // fails, and the failure is shown above it.
    let plan_slot = if let Some((command, plan)) = &state.planned {
        below(shell::plan_review(
            command_words(command).map(|(work, title)| format!("{work} · {title}")),
            plan,
            Message::StartPlanned,
            Message::CancelPlanned,
        ))
    } else {
        absent()
    };
    let page = column![wizard_slot, plan_slot, page_content(state, compact)];
    // The page slides in over `distance` pixels as its reveal animation runs.
    // The scrollable's id and the surface key are both the main page's label,
    // which is what `scroll_to` in `app` and the surface's keyboard scrolling
    // address.
    let page_reveal = if state.reduced_motion {
        1.0
    } else {
        state
            .page_reveal
            .interpolate(0.0, 1.0, std::time::Instant::now())
    };
    let page_key = state.page.main();
    let distance = if state.motion == MotionPreference::Subtle {
        6.0
    } else {
        12.0
    };
    let offset = state.page_direction * distance * (1.0 - page_reveal);
    body = body.push(shell::page_surface(
        page.into(),
        page_key.label(),
        compact,
        offset,
        !state.reduced_motion && state.motion != MotionPreference::Reduced,
        state.scroll_positions.clone(),
    ));
    // The bar for the job in progress, on every page but Jobs, which already
    // shows it.
    if let Some(job) = state.active()
        && state.page != Page::Queue
    {
        body = body.push(
            container(panel(
                row![
                    column![
                        text(format!("{} · {}", job.game.title, job.phase.label())).size(14),
                        progress(state, job)
                    ]
                    .spacing(6)
                    .width(Length::Fill),
                    secondary("See jobs", Message::GoTo(Page::Queue))
                ]
                .spacing(16)
                .align_y(Alignment::Center),
            ))
            .padding(theme::page_gutter(compact).top(0)),
        );
    }
    let base = shell::frame(sidebar, body.into());
    shell::with_toast(base, &state.toast, state.reduced_motion, Message::Dismiss)
}

/// The body of the page `state.page` names.
fn page_content(state: &State, compact: bool) -> Element<'_, Message> {
    match state.page {
        Page::Overview => overview(state),
        Page::Games => games(state, compact),
        Page::Queue => queue(state),
        Page::Drives => drives(state, false),
        Page::Recovery => recovery(state, false),
        Page::Settings => settings_page(state),
    }
}

/// The kind of work and the game's title for the command a storage plan is
/// for.
fn command_words(command: &Command) -> Option<(&'static str, &str)> {
    match command {
        Command::Enqueue {
            game, operation, ..
        } => Some((
            match operation {
                Operation::Analyze => "Analysis",
                Operation::Compress => "Compression",
                Operation::Decompress => "Decompression",
                Operation::Pack => "Maximum",
            },
            game.title.as_str(),
        )),
        Command::EnqueuePack { game, task } => Some((pack_words(task), game.title.as_str())),
        _ => None,
    }
}

/// The Overview headline: the saving so far, else what the library could
/// save, else what the window is doing.
fn headline(current: u64, potential: u64, scanning: bool, queuing: bool) -> String {
    if current > 0 {
        size(current)
    } else if scanning {
        "Finding your games…".into()
    } else if potential > 0 {
        format!("About {} to save", size(potential))
    } else if queuing {
        "Analyzing your games…".into()
    } else {
        "Analyze your games".into()
    }
}

/// The Games page when nothing is listed: a title, a hint, and the button
/// that gets the user out of it, chosen by why the list is empty.
fn empty_games(state: &State) -> (&'static str, &'static str, Option<(&'static str, Message)>) {
    if state.games.is_empty() && state.scanning {
        (
            "Finding your games…",
            "This can take a moment on a large library",
            None,
        )
    } else if state.games.is_empty() {
        (
            "No games found",
            "Add a folder that holds your games",
            Some(("Add a folder", Message::GoTo(Page::Drives))),
        )
    } else if !state.query.trim().is_empty() {
        (
            "No games match your search",
            "Try fewer letters or clear the search",
            Some(("Clear search", Message::Query(String::new()))),
        )
    } else {
        (
            "No games match these filters",
            "The filters hide every game",
            Some(("Show all games", Message::ClearFilters)),
        )
    }
}

/// The symbol drawn for a page in the sidebar.
fn page_icon(page: Page) -> &'static str {
    match page {
        Page::Overview => shell::icons::OVERVIEW,
        Page::Games => shell::icons::GAMES,
        Page::Queue => shell::icons::JOBS,
        Page::Drives => "▰",
        Page::Recovery => "⟲",
        Page::Settings => shell::icons::SETTINGS,
    }
}

// Overview page.

/// The Overview page: the saving so far with the main action, library totals,
/// a count of things needing attention, drives, and recent jobs.
fn overview(state: &State) -> Element<'_, Message> {
    let compressed = state
        .games
        .iter()
        .filter(|g| state.compressed(&g.game))
        .count();
    let current = state.current_saving();
    let potential = state.potential_saving();
    let total = state.total_bytes();
    // The same test as `Filter::Attention`, which leaves out excluded games.
    let attention = state
        .games
        .iter()
        .filter(|row| {
            !state.is_excluded(&row.game)
                && (!row.supported
                    || state.latest(&row.game).is_some_and(|job| {
                        matches!(
                            job.phase,
                            Phase::Failed | Phase::Partial | Phase::Interrupted
                        )
                    }))
        })
        .count();
    // The hero's main button compresses the library when a saving is
    // predicted, is disabled while scanning or analysing, and otherwise
    // offers another scan.
    let mut content = column![
        theme::page_header(
            "Overview",
            None,
            Some(secondary(
                if state.scanning {
                    "Refreshing…"
                } else {
                    "Refresh"
                },
                Message::Rescan
            ))
        ),
        hero(
            column![
                theme::muted(if current > 0 {
                    "SPACE SAVED (ESTIMATE)"
                } else {
                    "COMPRESS YOUR GAMES"
                }),
                text(headline(
                    current,
                    potential,
                    state.scanning,
                    state.analysis_queuing()
                ))
                .size(if current > 0 { 38 } else { 27 }),
                theme::muted(if state.scanning {
                    "Scanning…".into()
                } else if current > 0 && potential > 0 {
                    format!("About {} more to save", size(potential))
                } else if current > 0 {
                    "Up to date".into()
                } else {
                    "Only files that shrink are compressed".into()
                }),
                row![
                    action_maybe(
                        if potential > 0 {
                            format!("Compress to save about {}", size(potential))
                        } else if state.scanning || state.analysis_queuing() {
                            "Analyzing…".into()
                        } else {
                            "Scan again".into()
                        },
                        if potential > 0 {
                            Some(Message::OptimizeLibrary)
                        } else if state.scanning || state.analysis_queuing() {
                            None
                        } else {
                            Some(Message::Rescan)
                        }
                    ),
                    secondary("Games", Message::GoTo(Page::Games))
                ]
                .spacing(8)
                .align_y(Alignment::Center)
            ]
            .spacing(14)
        ),
        panel(
            row![
                theme::stat(size(total), "Installed"),
                theme::stat(state.games.len().to_string(), "Games"),
                theme::stat(compressed.to_string(), "Compressed")
            ]
            .spacing(36)
        )
    ]
    .spacing(theme::PAGE_GAP);
    if attention > 0 || !state.warnings.is_empty() {
        // Scan warnings are listed here, since no game row carries them.
        content = content.push(shell::attention_notes(
            attention,
            &state.warnings,
            Some(Message::ReviewAttention),
        ));
    }
    if !state.drives.is_empty() {
        content = content.push(theme::section_title("Drives"));
    }
    for drive in &state.drives {
        content = content.push(panel(
            row![
                column![
                    text(drive.path.display().to_string()),
                    theme::muted(format!(
                        "{} game{}",
                        drive.games,
                        if drive.games == 1 { "" } else { "s" }
                    ))
                ]
                .spacing(3)
                .width(Length::Fill),
                text(
                    drive
                        .free
                        .map(size)
                        .map(|s| format!("{s} free"))
                        .unwrap_or_else(|| "Drive unavailable".into())
                )
            ]
            .spacing(12)
            .align_y(Alignment::Center),
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
        content = content.push(panel(
            row![
                text(&job.game.title).width(Length::Fill),
                theme::muted(match super::app::job_outcome(job) {
                    Some(outcome) => outcome_words(outcome),
                    None => format!("{} · {}", kind_words(job), job.phase.label()),
                })
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        ));
    }
    content.into()
}

// Games page.

/// The Games page: search, filters, the bulk action bar and the first
/// `state.shown` rows of the filtered list.
fn games(state: &State, compact: bool) -> Element<'_, Message> {
    let filtered = state.filtered();
    let listed = state.listed(&filtered);
    // The same three controls, stacked when compact and in one row otherwise.
    let search_and_sort: Element<'_, Message> = if compact {
        column![
            input("Search your games", &state.query)
                .id("game-search")
                .on_input(Message::Query)
                .width(Length::Fill),
            row![
                picker(
                    [
                        Filter::All,
                        Filter::Ready,
                        Filter::Compressed,
                        Filter::Attention,
                        Filter::Updated
                    ],
                    Some(state.filter),
                    Message::Filter
                ),
                picker(
                    [Sort::Worth, Sort::Name, Sort::Size],
                    Some(state.sort),
                    Message::Sort
                )
            ]
            .spacing(8)
            .align_y(Alignment::Center)
        ]
        .spacing(8)
        .into()
    } else {
        row![
            input("Search your games", &state.query)
                .id("game-search")
                .on_input(Message::Query)
                .width(Length::Fill),
            picker(
                [
                    Filter::All,
                    Filter::Ready,
                    Filter::Compressed,
                    Filter::Attention,
                    Filter::Updated
                ],
                Some(state.filter),
                Message::Filter
            ),
            picker(
                [Sort::Worth, Sort::Name, Sort::Size],
                Some(state.sort),
                Message::Sort
            )
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    };
    let mut content = column![
        theme::page_header(
            "Games",
            None,
            Some(secondary(
                if state.scanning {
                    "Refreshing…"
                } else {
                    "Refresh"
                },
                Message::Rescan
            ))
        ),
        search_and_sort,
    ]
    .spacing(theme::PAGE_GAP);
    // Drive and launcher filters. The first option of each stands for no
    // filter and is mapped back to `None` by comparing its label.
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
            picker(drive_options, Some(selected_drive), |value| {
                Message::DriveFilter((value != "All drives").then(|| value.into()))
            }),
            picker(
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
        .spacing(8)
        .align_y(Alignment::Center),
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
        let (title, hint, way_out) = empty_games(state);
        return content
            .push(shell::empty_panel(title, hint, way_out))
            .into();
    }
    if state.order_stale {
        content = content.push(panel(
            row![
                theme::muted("New estimates arrived while a game was open or ticked")
                    .width(Length::Fill),
                secondary("Sort again", Message::Sort(state.sort))
            ]
            .align_y(Alignment::Center),
        ));
    }
    // Rows are keyed by a hash of the game id, so a row keeps its widget
    // state when the list is filtered or reordered. Under the Worth sort each
    // group gets a heading keyed by its name, and the last group stays
    // collapsed until asked for.
    let grouped = state.sort == Sort::Worth;
    let mut rows = Vec::new();
    let mut heading = None;
    for game in listed.iter().take(state.shown) {
        let (group, _) = state.worth(&game.game);
        if grouped && heading != Some(group) {
            heading = Some(group);
            rows.push(group_line(state, &filtered, group));
        }
        rows.push((
            *blake3::hash(game.game.id.to_string().as_bytes()).as_bytes(),
            game_row(state, game, compact),
        ));
    }
    // The collapsed group has no rows in the list, so its heading and the
    // button that opens it come last.
    if grouped && !state.show_low && listed.len() <= state.shown && filtered.len() > listed.len() {
        rows.push(group_line(state, &filtered, 3));
    }
    content = content.push(iced::widget::keyed_column(rows).spacing(12));
    let hidden = listed.len().saturating_sub(state.shown);
    if hidden > 0 {
        content = content.push(secondary(
            format!("Show more · {}", games_count(hidden)),
            Message::ShowMore,
        ));
    }
    content.into()
}

/// The control that opens and closes a game's Advanced section.
fn advanced_toggle<'a>(id: &str, open: bool) -> Element<'a, Message> {
    theme::disclosure(
        if open { "Hide advanced" } else { "Advanced" },
        open,
        Message::ToggleAdvanced(id.to_owned()),
    )
}

/// The heading above a group of the Worth order, keyed by the group's name.
/// The last group's heading carries the button that expands it.
fn group_line<'a>(
    state: &'a State,
    filtered: &[&GameRow],
    group: u8,
) -> ([u8; 32], Element<'a, Message>) {
    let members = filtered
        .iter()
        .filter(|other| state.worth(&other.game).0 == group);
    let (count, total) = members.fold((0, 0_u64), |(count, total), other| {
        (count + 1, total + state.worth(&other.game).1)
    });
    let name = super::app::WORTH_GROUPS
        .get(usize::from(group))
        .copied()
        .unwrap_or("Games");
    let count_text = games_count(count);
    let summary = match group {
        0 => format!("{name} · {count_text} · about {} to save", size(total)),
        2 if total > 0 => format!("{name} · {count_text} · about {} saved", size(total)),
        _ => format!("{name} · {count_text}"),
    };
    let line: Element<'_, Message> = if group == 3 {
        row![
            theme::section_text(summary).width(Length::Fill),
            secondary(
                if state.show_low { "Hide" } else { "Show" },
                Message::ToggleLow
            )
        ]
        .align_y(Alignment::Center)
        .into()
    } else {
        theme::section_text(summary).into()
    };
    (*blake3::hash(name.as_bytes()).as_bytes(), line)
}

/// One game: a summary line, and below it the detail pane when this game is
/// the expanded one.
fn game_row<'a>(state: &'a State, item: &'a GameRow, compact: bool) -> Element<'a, Message> {
    let game = &item.game;
    let id = game.id.to_string();
    let compressed = state.compressed(game);
    // Artwork tile. Clicking it opens the row like the rest of the header does.
    let icon = shell::artwork_tile(
        &state.artwork_cache,
        item.artwork.as_ref(),
        &game.title,
        shell::Tile::Row(if compact { 44 } else { 52 }),
        Message::ArtworkVisible,
    );
    let icon = button(icon)
        .style(button::text)
        .padding(0)
        .on_press(Message::Expand(id.clone()));
    // Status under the title: the row's note, else the phase of a job that
    // is active or ended badly, else "Compressed", else the launcher's name.
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
        if matches!(
            state.result(game),
            Some(super::app::Outcome::AwaitingConfirm { .. })
        ) {
            "Play it, then confirm".into()
        } else if compressed {
            "Compressed".into()
        } else {
            game.id.launcher.label().into()
        }
    });
    // Under the size: what the chosen mode would save, or what compressing
    // already gained.
    // A compression or decompression in progress for this game. Its row
    // reports the work and offers nothing that would queue it again.
    let working = state
        .latest(game)
        .filter(|job| job.phase.active() && job.operation != Operation::Analyze);
    let saving = if let Some(job) = working {
        if job.files_total > 0 {
            format!(
                "{} · {} of {} files",
                kind_words(job),
                job.files_done,
                job.files_total
            )
        } else {
            kind_words(job).to_owned()
        }
    } else if compressed {
        match state.result(game) {
            Some(outcome) => outcome_words(outcome),
            None => "Compressed".into(),
        }
    } else {
        match state.prospect(game, state.choice_for(game)) {
            Some(0) => "Little to save".into(),
            Some(saving) => format!("About {} to save", size(saving)),
            None => "Not analyzed yet".into(),
        }
    };
    let ready = !state.pending.contains(&id) && working.is_none();
    // Summary line: checkbox, artwork, title and status, size and saving,
    // then the main button. The button reads "Recheck" and queues an analysis
    // once the game is compressed. A custom folder has no launcher size, so
    // the analysis supplies it. When compact, the figures move under the
    // title, where the row has the width for them.
    let size_text = state
        .size_of(game)
        .map(size)
        .unwrap_or_else(|| "Size pending".into());
    let check_id = id.clone();
    let mut title = column![text(&game.title).size(16), theme::muted(status)].spacing(4);
    if compact {
        title = title.push(theme::muted(format!("{size_text} · {saving}")));
    }
    let mut line = row![
        checkbox(state.selected.contains(&id)).on_toggle_maybe(
            (item.supported && game.state.is_idle() && !state.pending.contains(&id))
                .then_some(move |value| Message::Select(check_id.clone(), value)),
        ),
        icon,
        button(title)
            .style(button::text)
            .width(Length::Fill)
            .on_press(Message::Expand(id.clone())),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    if !compact {
        line = line.push(
            container(
                column![text(size_text), theme::muted(saving)]
                    .spacing(4)
                    .align_x(Alignment::End),
            )
            .width(theme::TRAILING_WIDTH)
            .align_x(Alignment::End),
        );
    }
    // Compress is the main action. A compressed game only offers another
    // analysis, which is not, so it gets the quieter button. The slot is
    // there without a button so the figures keep their column.
    let button_slot: Element<'_, Message> = if !item.supported {
        Space::new().into()
    } else if state
        .snapshot
        .packs
        .iter()
        .any(|install| install.game_path == game.install_dir)
    {
        // A stored game's next step is in its details.
        secondary("Details", Message::Expand(id.clone()))
    } else if compressed {
        secondary_maybe(
            "Analyze again",
            ready.then(|| Message::One(id.clone(), Operation::Analyze)),
        )
    } else {
        action_maybe(
            "Compress",
            ready.then(|| Message::One(id.clone(), Operation::Compress)),
        )
    };
    line = line.push(if compact {
        Element::from(button_slot)
    } else {
        container(button_slot)
            .width(theme::ACTION_SLOT_WIDTH)
            .align_x(Alignment::End)
            .into()
    });
    // Detail pane. It stays in the tree while its closing animation runs.
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
        .spacing(12);
        // The cover sits to the left of everything else in the pane. On a
        // narrow window there is no room for it.
        let cover_tile = item.cover.as_ref().filter(|_| !compact).map(|source| {
            shell::artwork_tile(
                &state.artwork_cache,
                Some(source),
                &game.title,
                shell::Tile::Cover,
                Message::ArtworkVisible,
            )
        });
        // The one choice that matters: how this game is compressed. Each mode
        // shows what it is predicted to save once the game is analyzed.
        let stored = state
            .snapshot
            .packs
            .iter()
            .any(|install| install.game_path == game.install_dir);
        let advanced_open = state.advanced.contains(&id);
        // One plain line about how far to trust the figures.
        let confidence = state.estimate(game).and_then(|est| {
            state.recommendation(game).map(|choice| {
                if est.unsampled_files > 0 {
                    format!(
                        "{}. The largest files were sampled and the rest scaled in.",
                        choice.confidence.label()
                    )
                } else {
                    format!("{}.", choice.confidence.label())
                }
            })
        });
        // A game that already runs from a store shows its Maximum card below
        // and none of this: its mode is settled, and the native Analyze and
        // Decompress do not apply to a mounted store.
        let mut confidence_shown = false;
        if item.supported && !stored {
            let chosen = state.choice_for(game);
            let mut modes = row![].spacing(8);
            for (choice, name, available) in [
                (StorageChoice::Standard, "Standard", item.native_supported),
                (StorageChoice::Maximum, "Maximum", item.pack_supported),
            ] {
                if !available {
                    continue;
                }
                let label = match state.prospect(game, choice) {
                    Some(0) => format!("{name} · little to save"),
                    Some(saving) => format!("{name} · about {}", size(saving)),
                    None => name.to_owned(),
                };
                // The chosen mode wears the selected style. Pressing it again
                // changes nothing, which keeps it from looking disabled.
                modes = modes.push(theme::choice(
                    label,
                    choice == chosen,
                    Message::Choice(id.clone(), choice),
                ));
            }
            let mut heading = row![theme::section_title("How to compress")]
                .spacing(12)
                .align_y(Alignment::Center);
            if let Some(confidence) = &confidence {
                confidence_shown = true;
                heading = heading
                    .push(Space::new().width(Length::Fill))
                    .push(theme::muted(confidence.clone()));
            }
            details = details
                .push(heading)
                .push(modes.wrap())
                .push(theme::muted(match chosen {
                    StorageChoice::Standard => {
                        "Standard is quick, and the game's files stay exactly where they are."
                    }
                    StorageChoice::Maximum => {
                        "Maximum saves more and takes minutes. The game runs from a compressed store, and the original is kept until you confirm it works."
                    }
                }))
                .push(
                    row![
                        secondary_maybe(
                            "Analyze",
                            ready.then(|| Message::One(id.clone(), Operation::Analyze))
                        ),
                        secondary_maybe(
                            "Decompress",
                            (ready && compressed)
                                .then(|| Message::One(id.clone(), Operation::Decompress))
                        ),
                        Space::new().width(Length::Fill),
                        advanced_toggle(&id, advanced_open)
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                );
        }
        // Advanced: the native preset, then the store controls.
        let preset_id = id.clone();
        if item.native_supported && !stored && advanced_open {
            details = details
                .push(
                    row![
                        theme::muted("Standard strength"),
                        picker(
                            [Preset::Fast, Preset::Balanced, Preset::Max],
                            Some(state.preset_for(&id)),
                            move |preset| Message::Preset(preset_id.clone(), preset)
                        )
                    ]
                    .spacing(12)
                    .align_y(Alignment::Center),
                )
                .push(theme::muted(
                    "Balanced is recommended. Max takes longer for a little more.",
                ));
        }
        // Maximum Space. A game that runs from a store gets a card that says
        // where it stands and offers the one next step. A game that does not
        // gets the store form, under Advanced.
        let install = state
            .snapshot
            .packs
            .iter()
            .find(|install| install.game_path == game.install_dir);
        if let Some(install) = install {
            let queue = |task: crate::jobs::PackTask| {
                Message::Send(Command::EnqueuePack {
                    game: game.clone(),
                    task,
                })
            };
            let saved = install.summary.as_ref().map(|summary| {
                let used = summary.archive_bytes.saturating_sub(summary.shared_bytes);
                (summary.logical_bytes, used)
            });
            details = details.push(theme::section_title("Maximum"));
            if install.phase != crate::pack::InstallPhase::Mounted {
                details = details.push(theme::muted(format!(
                    "{} · {}",
                    install.phase.label(),
                    install.message
                )));
            } else if install.backup_path.is_some() {
                // Step two of three: the store is mounted and the original
                // is still on disk, so nothing has been saved yet.
                details = details
                    .push(theme::muted(match saved {
                        Some((before, after)) => format!(
                            "Play the game once. If it works, delete the original to save about {}.",
                            size(before.saturating_sub(after))
                        ),
                        None => "Play the game once. If it works, delete the original to save the space.".into(),
                    }))
                    .push(
                        row![
                            if state.confirm_reclaim.contains(&id) {
                                action_maybe(
                                    "Yes, delete the original",
                                    ready.then(|| queue(crate::jobs::PackTask::Reclaim)),
                                )
                            } else {
                                action_maybe(
                                    "It works, delete the original",
                                    ready.then(|| Message::PackReclaimPrompt(id.clone())),
                                )
                            },
                            secondary_maybe(
                                "Decompress to ordinary files",
                                ready.then(|| queue(crate::jobs::PackTask::Restore)),
                            )
                        ]
                        .spacing(8)
                        .wrap(),
                    )
                    .push(theme::muted(
                        "The store is checked in full before the original is deleted.",
                    ));
            } else {
                details = details
                    .push(theme::muted(match saved {
                        Some((before, after)) => {
                            format!("{} → {}", size(before), size(after))
                        }
                        None => "Running from its compressed store".into(),
                    }))
                    .push(theme::muted(if install.previous_store_path.is_some() {
                        "An update was folded in. The previous version is deleted after you next play the game."
                    } else {
                        "Game updates are folded in automatically while the game is closed."
                    }))
                    .push(secondary_maybe(
                        "Decompress to ordinary files",
                        ready.then(|| queue(crate::jobs::PackTask::Restore)),
                    ))
                    .push(theme::muted(
                        "Decompressing needs free space for the whole game.",
                    ));
            }
            details = details.push(advanced_toggle(&id, advanced_open));
            if advanced_open {
                details = details.push(theme::muted(format!(
                    "Store: {} · updates: {}",
                    install.store_path.display(),
                    install.writes_path.display()
                )));
                if let Some(previous) = &install.previous_store_path {
                    details = details.push(theme::muted(format!(
                        "Previous version: {}",
                        previous.display()
                    )));
                }
                if install.backup_path.is_none()
                    && install.phase == crate::pack::InstallPhase::Mounted
                {
                    details = details.push(
                        row![
                            secondary_maybe(
                                "Fold in updates now",
                                (ready && install.previous_store_path.is_none())
                                    .then(|| queue(crate::jobs::PackTask::Compact)),
                            ),
                            secondary_maybe(
                                "Delete the previous version now",
                                (ready && install.previous_store_path.is_some())
                                    .then(|| queue(crate::jobs::PackTask::Prune)),
                            )
                        ]
                        .spacing(8)
                        .wrap(),
                    );
                }
            }
        } else if item.pack_supported && advanced_open {
            let path_id = id.clone();
            // The default path is the placeholder, so the field can be emptied.
            let default_store = state.store_path(game).display().to_string();
            let typed = state.pack_paths.get(&id).cloned().unwrap_or_default();
            details = details
                .push(theme::section_title("Maximum Space storage"))
                .push(theme::muted("Verified chunks can be shared across games"))
                .push(
                    input(&default_store, &typed)
                        .on_input(move |path| Message::PackPath(path_id.clone(), path)),
                )
                .push(secondary_maybe(
                    "Choose storage folder…",
                    (!state.picker_busy)
                        .then(|| Message::Browse(super::dialog::Target::Storage(id.clone()))),
                ))
                .push(
                    column![
                        text("1. Create and verify a store.").size(13),
                        text("2. Play the game to test it.").size(13),
                        text("3. Delete the original to save the space.").size(13)
                    ]
                    .spacing(2),
                )
                .push(theme::muted("Creation needs room for the store beside the original. Updates use more space, and decompressing later needs room for the whole game."))
                .push(
                    row![
                        secondary_maybe(
                            "Create & activate",
                            (!state.pending.contains(&id))
                                .then(|| Message::PackActivate(id.clone(), true))
                        ),
                        secondary_maybe(
                            "Create store only",
                            (!state.pending.contains(&id))
                                .then(|| Message::PackCreate(id.clone()))
                        ),
                        secondary_maybe(
                            "Activate existing",
                            (!state.pending.contains(&id))
                                .then(|| Message::PackActivate(id.clone(), false))
                        )
                    ]
                    .spacing(8)
                    .wrap(),
                );
        }
        if let Some(note) = &item.note {
            details = details.push(theme::muted(note));
        }
        if !confidence_shown && let Some(confidence) = &confidence {
            details = details.push(theme::muted(confidence.clone()));
        }
        // Advanced, continued: reports, exclusion, and what the analysis saw.
        if advanced_open {
            let mut tools = row![
                secondary_maybe(
                    if state.qualifying {
                        "Measuring…"
                    } else {
                        "Qualify compatibility"
                    },
                    (item.game.state.is_idle() && !state.qualifying)
                        .then(|| Message::Qualify(id.clone())),
                ),
                secondary_maybe(
                    "Import compatibility report…",
                    (!state.picker_busy).then_some(Message::Browse(super::dialog::Target::Report)),
                ),
                secondary_maybe(
                    "Choose artwork…",
                    (!state.picker_busy)
                        .then(|| Message::Browse(super::dialog::Target::Artwork(id.clone()))),
                ),
                secondary("Use default artwork", Message::ClearArtwork(id.clone())),
                secondary(
                    "Exclude this game",
                    Message::Send(Command::Exclude {
                        id: id.clone(),
                        excluded: true,
                    }),
                )
            ]
            .spacing(8);
            if state.qualifying {
                tools = tools.push(secondary("Cancel", Message::CloseQualification));
            }
            details = details.push(tools.wrap());
            if let Some(est) = state.estimate(game) {
                let mut facts = column![].spacing(4);
                if let Some(choice) = state.recommendation(game) {
                    facts = facts.push(theme::fact(
                        "Savings",
                        format!(
                            "Standard about {}{}",
                            size(choice.native_saving),
                            choice
                                .maximum_saving
                                .map(|saving| format!(" · Maximum sample about {}", size(saving)))
                                .unwrap_or_default()
                        ),
                    ));
                }
                facts = facts.push(theme::fact(
                    "Sampled",
                    format!(
                        "{} from {} files · {} skipped · {} not sampled{}",
                        size(est.sampled),
                        est.inspected_files,
                        est.skipped_files,
                        est.unsampled_files,
                        if est.already_compressed_mount {
                            " · savings are on top of the drive's own compression"
                        } else {
                            ""
                        }
                    ),
                ));
                let evidence = est.format_evidence;
                facts = facts.push(theme::fact(
                    "Formats",
                    format!(
                        "{} known · {} unknown · {} encoded · {} containers · {} raw{}",
                        evidence.recognized_files,
                        evidence.unknown_files,
                        evidence.encoded_files,
                        evidence.container_files,
                        evidence.raw_media_files,
                        if evidence.encrypted_files > 0 {
                            format!(" · {} encrypted", evidence.encrypted_files)
                        } else {
                            String::new()
                        }
                    ),
                ));
                if est.small_files.files > 0 {
                    facts = facts.push(theme::fact(
                        "Small files",
                        format!(
                            "{} in {} files · grouping would save {} more",
                            size(est.small_files.bytes),
                            est.small_files.files,
                            size(est.small_files.extra_payload_saving)
                        ),
                    ));
                }
                facts = facts.push(theme::fact(
                    "Compatibility",
                    if est.maximum_qualified {
                        "A compatibility report matches this build and its files".into()
                    } else {
                        "No compatibility report matches this build".into()
                    },
                ));
                details = details.push(facts);
            }
        }
        if let Some(job) = state.latest(game)
            && !job.errors.is_empty()
        {
            details = details.push(theme::danger_text(job.errors.join("\n")));
        }
        let reveal = if state.reduced_motion {
            1.0
        } else {
            state
                .detail
                .interpolate(0.0, 1.0, std::time::Instant::now())
        };
        let pane: Element<'_, Message> = match cover_tile {
            Some(cover) => row![cover, details.width(Length::Fill)].spacing(18).into(),
            None => details.into(),
        };
        contents = contents.push(super::surface::surface(
            container(pane),
            6.0 * (1.0 - reveal),
            false,
            "game-details",
        ));
    }
    // The id lets "Review game and storage" scroll to the row.
    container(panel(contents))
        .id(iced::widget::Id::from(format!("game-{id}")))
        .into()
}

// Settings sections: recovery, jobs, locations and preferences.

/// The Recovery section: jobs that failed, were partial or were interrupted,
/// and pack installs that are not simply mounted or that still retain an
/// original or a previous store.
fn recovery(state: &State, embedded: bool) -> Element<'_, Message> {
    let subtitle = "Review interrupted jobs and retained storage before retrying or restoring.";
    let mut content = column![
        if embedded {
            column![theme::section_title("Recovery"), theme::muted(subtitle)]
                .spacing(4)
                .into()
        } else {
            theme::page_header::<Message>("Recovery", Some(subtitle), None)
        },
        secondary("Export local diagnostics", Message::ExportDiagnostics)
    ]
    .spacing(theme::PAGE_GAP);
    let mut issues = 0;
    for job in state.snapshot.jobs.iter().filter(|job| {
        matches!(
            job.phase,
            Phase::Interrupted | Phase::Partial | Phase::Failed
        )
    }) {
        issues += 1;
        content = content.push(job_row(state, job));
    }
    for install in &state.snapshot.packs {
        if install.phase == crate::pack::InstallPhase::Mounted
            && install.backup_path.is_none()
            && install.previous_store_path.is_none()
        {
            continue;
        }
        issues += 1;
        let game = state
            .games
            .iter()
            .find(|row| row.game.install_dir == install.game_path);
        let mut details = column![
            theme::section_text(
                game.map(|row| row.game.title.clone())
                    .unwrap_or_else(|| install.game_path.display().to_string())
            ),
            theme::muted(install.game_path.display().to_string()),
            theme::muted(install.phase.label()),
            text(&install.message).size(13),
            theme::muted(format!(
                "Store: {} · Updates: {}",
                install.store_path.display(),
                install.writes_path.display()
            ))
        ]
        .spacing(8);
        if let Some(backup) = &install.backup_path {
            details = details.push(theme::muted(format!(
                "Retained original: {}",
                backup.display()
            )));
        }
        if let Some(previous) = &install.previous_store_path {
            details = details.push(theme::muted(format!(
                "Previous store retained: {}",
                previous.display()
            )));
        }
        // True while any job for this game is active. Restore and verify are
        // offered only without one.
        let pending = state
            .snapshot
            .jobs
            .iter()
            .any(|job| job.phase.active() && job.game.install_dir == install.game_path);
        if let Some(row) = game {
            let mut actions = iced::widget::Row::new().spacing(8);
            if !pending
                && row.game.state.is_idle()
                && install.phase == crate::pack::InstallPhase::Mounted
            {
                actions = actions.push(secondary(
                    "Restore ordinary files",
                    Message::Send(Command::EnqueuePack {
                        game: row.game.clone(),
                        task: crate::jobs::PackTask::Restore,
                    }),
                ));
            }
            if !pending
                && row.game.state.is_idle()
                && install.phase == crate::pack::InstallPhase::Attention
            {
                actions = actions.push(secondary(
                    "Verify restored files",
                    Message::Send(Command::EnqueuePack {
                        game: row.game.clone(),
                        task: crate::jobs::PackTask::VerifyRestored,
                    }),
                ));
            }
            actions = actions.push(secondary(
                "Review game and storage",
                Message::ReviewGame(row.game.id.to_string()),
            ));
            details = details.push(actions.wrap());
        }
        content = content.push(theme::attention_card(
            details,
            install.phase == crate::pack::InstallPhase::Attention,
        ));
    }
    if issues == 0 {
        content = content.push(panel(theme::muted(
            "No interrupted jobs or retained storage need review.",
        )));
    }
    content.into()
}

/// What a job is doing, in the window's words. `PackTask::label` is shared
/// with the command line, so the window names storage tasks itself.
fn kind_words(job: &Job) -> &'static str {
    match job.operation {
        Operation::Analyze => "Analysis",
        Operation::Compress => "Compression",
        Operation::Decompress => "Decompression",
        Operation::Pack => job.pack.as_ref().map_or("Maximum", pack_words),
    }
}

/// What a storage task does, in the window's words.
fn pack_words(task: &crate::jobs::PackTask) -> &'static str {
    use crate::jobs::PackTask;
    match task {
        PackTask::Create { .. } => "Build Maximum store",
        PackTask::Activate { create: true, .. } => "Maximum compression",
        PackTask::Activate { .. } => "Switch to Maximum store",
        PackTask::Compact => "Fold in updates",
        PackTask::Restore => "Decompression to ordinary files",
        PackTask::VerifyRestored => "Check decompressed files",
        PackTask::Reclaim => "Delete the original",
        PackTask::Prune => "Delete the previous version",
    }
}

/// A result in words. Estimates say so, and only measured sizes get an arrow.
fn outcome_words(outcome: super::app::Outcome) -> String {
    use super::app::Outcome;
    match outcome {
        Outcome::Estimated { saved: 0, .. } => "Compressed · little to save".into(),
        Outcome::Estimated { saved, .. } => {
            format!("About {} saved (estimate)", size(saved))
        }
        Outcome::Measured { before, after } => format!("{} → {}", size(before), size(after)),
        Outcome::AwaitingConfirm { expected } => format!(
            "Saves about {} once you delete the original",
            size(expected)
        ),
    }
}

/// How a job's bar and phase label are coloured: paused and partial work in
/// the warning colour, failed and interrupted work in the danger colour.
fn tone(phase: Phase) -> theme::Tone {
    match phase {
        Phase::Pausing | Phase::Paused | Phase::Partial => theme::Tone::Warning,
        Phase::Failed | Phase::Interrupted => theme::Tone::Failed,
        _ => theme::Tone::Normal,
    }
}

/// A job's progress bar, by bytes when a byte total is known and by files
/// otherwise. Empty when neither total is known, and for a job that has not
/// started, whose bar would be an empty track.
fn progress<'a>(state: &'a State, job: &'a Job) -> Element<'a, Message> {
    let fraction = if job.phase == Phase::Queued {
        return Space::new().height(0).into();
    } else if job.bytes_total > 0 {
        job.bytes_done as f32 / job.bytes_total as f32
    } else if job.files_total > 0 {
        job.files_done as f32 / job.files_total as f32
    } else {
        return Space::new().height(0).into();
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
    .style(theme::progress_bar(tone(job.phase)))
    .into()
}
/// A card for a job that is running, waiting or needs attention, with its
/// controls. A job that ended badly gets a card edged in the failure colour.
fn job_row<'a>(state: &'a State, job: &'a Job) -> Element<'a, Message> {
    // Pause and Cancel for an active job, except a pack job in a step that
    // cannot be interrupted. Retry for a job that stopped short of completing.
    let mut controls = iced::widget::Row::new().spacing(8);
    if job.phase.active() && (job.operation != Operation::Pack || job.pack_interruptible) {
        controls = controls
            .push(secondary(
                if job.user_paused { "Resume" } else { "Pause" },
                Message::Send(Command::Pause {
                    id: job.id,
                    paused: !job.user_paused,
                }),
            ))
            .push(secondary("Cancel", Message::Send(Command::Cancel(job.id))));
    } else if !job.phase.active() && job.phase != Phase::Completed {
        controls = controls.push(secondary("Retry", Message::Send(Command::Retry(job.id))));
    }
    let kind = kind_words(job);
    let mut content = column![
        row![
            theme::section_text(&job.game.title).width(Length::Fill),
            theme::muted(format!("{kind} · {}", job.phase.label()))
                .style(theme::tone_text(tone(job.phase)))
        ]
        .spacing(12)
        .align_y(Alignment::Center),
        progress(state, job),
        theme::muted(&job.message),
        row![
            theme::muted(if job.files_total > 0 {
                format!(
                    "{} / {} items · {} processed · {}s",
                    job.files_done,
                    job.files_total,
                    size(job.bytes_done),
                    job.elapsed
                )
            } else if job.bytes_done > 0 {
                format!("{} processed · {}s", size(job.bytes_done), job.elapsed)
            } else {
                format!("{}s elapsed", job.elapsed)
            })
            .width(Length::Fill),
            controls.wrap()
        ]
        .spacing(12)
        .align_y(Alignment::Center)
    ]
    .spacing(10);
    // The free-space change covers the whole filesystem, so the label says
    // it includes other programs' writes.
    if let Some(change) = job.drive_change {
        content = content.push(theme::muted(format!(
            "Drive free-space change: {}{}. Includes other applications' disk activity.",
            if change >= 0 { "+" } else { "−" },
            size(change.unsigned_abs())
        )));
    }
    if !job.errors.is_empty() {
        content = content.push(theme::danger_text(job.errors.join("\n")));
    }
    match tone(job.phase) {
        theme::Tone::Failed => theme::attention_card(content, true),
        theme::Tone::Warning if job.phase == Phase::Partial => {
            theme::attention_card(content, false)
        }
        _ => panel(content),
    }
}
/// The Jobs section: connection state, then jobs grouped as running, waiting,
/// needing attention and history, then recent activity. Analyses are not
/// listed. History shows the newest 20.
fn queue(state: &State) -> Element<'_, Message> {
    let mut content = column![theme::page_header::<Message>(
        "Jobs",
        Some("Track running work, waiting games, and recent results"),
        None
    )]
    .spacing(theme::PAGE_GAP);
    if let Some(error) = &state.connection_error {
        content = content.push(theme::attention_card(
            column![
                theme::section_text("Worker connection interrupted"),
                theme::danger_text(error),
                theme::muted("Showing the last received jobs. Reconnecting…")
            ]
            .spacing(6),
            true,
        ));
    }
    if !state.snapshot_loaded && state.snapshot.jobs.is_empty() {
        content = content.push(theme::muted("Connecting to the background worker…"));
    }
    if let Some(game) = &state.snapshot.gaming {
        content = content.push(panel(text(format!("Paused while you play {game}"))));
    }
    for (group, (title, empty)) in shell::JOB_GROUPS.into_iter().enumerate() {
        let jobs: Vec<_> = state
            .snapshot
            .jobs
            .iter()
            .filter(|job| {
                job.operation != Operation::Analyze
                    && match group {
                        0 => job.phase.active() && job.phase != Phase::Queued,
                        1 => job.phase == Phase::Queued,
                        2 => matches!(
                            job.phase,
                            Phase::Failed | Phase::Partial | Phase::Interrupted
                        ),
                        _ => matches!(job.phase, Phase::Completed | Phase::Cancelled),
                    }
            })
            .collect();
        content = content.push(theme::section_text(format!("{title} · {}", jobs.len())));
        if jobs.is_empty() {
            content = content.push(theme::muted(empty));
        }
        let rows: Vec<_> = if group == 3 {
            jobs.into_iter()
                .rev()
                .take(shell::HISTORY_LIMIT)
                .map(|job| (job.id, completed_job_row(job)))
                .collect()
        } else {
            jobs.into_iter()
                .map(|job| (job.id, job_row(state, job)))
                .collect()
        };
        content = content.push(iced::widget::keyed_column(rows).spacing(12));
    }
    for entry in &state.activity {
        content = content.push(theme::muted(&entry.message));
    }
    content.into()
}
/// The Locations section: the form that adds a folder, one card per library
/// with its maintenance toggle, and the excluded games.
fn drives(state: &State, embedded: bool) -> Element<'_, Message> {
    let title = "Drives & libraries";
    let subtitle = "Add games from any location and choose which libraries to maintain";
    let mut content = column![
        if embedded {
            column![theme::section_title(title), theme::muted(subtitle)]
                .spacing(4)
                .into()
        } else {
            theme::page_header::<Message>(title, Some(subtitle), None)
        },
        panel(
            column![
                theme::section_title("Add a location"),
                picker([FolderKind::Collection, FolderKind::Game], Some(state.folder_kind), Message::FolderKind),
                theme::muted(match state.folder_kind {
                    FolderKind::Collection => "Each immediate subfolder appears as a game. New subfolders appear when you refresh.",
                    FolderKind::Game => "Show this entire folder as one game."
                }),
                row![
                    input("~/My Games or /mnt/games", &state.folder)
                        .on_input(Message::Folder)
                        .on_submit(Message::AddFolder)
                        .width(Length::Fill),
                    secondary_maybe(
                        "Browse…",
                        (!state.picker_busy)
                            .then_some(Message::Browse(super::dialog::Target::Game))
                    ),
                    action("Add folder", Message::AddFolder)
                ]
                .spacing(8)
                .align_y(Alignment::Center),
                if let Some(error) = &state.folder_error {
                    Element::from(theme::danger_text(error))
                } else {
                    Element::from(Space::new().height(0))
                }
            ]
            .spacing(10)
        )
    ]
    .spacing(theme::PAGE_GAP);
    // Libraries the worker knows, followed by the parent folder of any game
    // that is in none of them. Those are listed as detected libraries.
    let mut paths: Vec<_> = state
        .snapshot
        .libraries
        .iter()
        .map(|l| (l.path.clone(), l.custom, l.folder_kind))
        .collect();
    for game in &state.games {
        let path = game
            .game
            .install_dir
            .parent()
            .unwrap_or(&game.game.install_dir)
            .to_path_buf();
        if !paths.iter().any(|(p, _, _)| *p == path) {
            paths.push((path, false, FolderKind::Game));
        }
    }
    for (path, custom, folder_kind) in paths {
        let automatic = state
            .snapshot
            .libraries
            .iter()
            .any(|l| l.path == path && l.automatic);
        let label = path.display().to_string();
        let remove_path = path.clone();
        content = content.push(panel(
            column![
                row![
                    column![
                        text(label),
                        theme::muted(if custom {
                            folder_kind.to_string()
                        } else {
                            "Detected library".into()
                        })
                    ]
                    .spacing(4)
                    .width(Length::Fill),
                    if custom {
                        secondary(
                            "Remove location",
                            Message::Send(Command::RemoveLibrary(remove_path)),
                        )
                    } else {
                        Space::new().into()
                    }
                ]
                .spacing(10)
                .align_y(Alignment::Center),
                checkbox(automatic)
                    .label("Maintain new installs and updates")
                    .on_toggle(move |enabled| Message::Send(Command::Library(Library {
                        path: path.clone(),
                        automatic: enabled,
                        custom,
                        folder_kind
                    })))
            ]
            .spacing(10),
        ));
    }
    for id in &state.snapshot.excluded {
        content = content.push(panel(
            row![
                theme::muted(format!("Excluded: {}", state.title_of(id))).width(Length::Fill),
                secondary(
                    "Restore",
                    Message::Send(Command::Exclude {
                        id: id.clone(),
                        excluded: false
                    })
                )
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        ));
    }
    content.into()
}

/// The Settings page: jump links, then every section in one scrolling column.
///
/// The container ids here are the ones `Page::section` and `Message::Jump`
/// look for.
fn settings_page(state: &State) -> Element<'_, Message> {
    let links = iced::widget::Row::with_children(
        [
            ("Drives & libraries", "settings-locations"),
            ("Recovery", "settings-recovery"),
            ("Background worker", "settings-worker"),
            ("Automatic maintenance", "settings-maintenance"),
            ("Appearance", "settings-appearance"),
            ("Maximum Space compatibility", "settings-reports"),
            ("About Flummox", "settings-about"),
        ]
        .into_iter()
        .map(|(label, section)| {
            secondary(
                label,
                if section == "settings-recovery" {
                    Message::GoTo(Page::Recovery)
                } else {
                    Message::Jump(section)
                },
            )
        }),
    )
    .spacing(8)
    .wrap();
    column![
        theme::page_header::<Message>("Settings", None, None),
        links,
        container(drives(state, true)).id("settings-locations"),
        container(recovery(state, true)).id("settings-recovery"),
        preferences(state),
    ]
    .spacing(theme::PAGE_GAP * 2.0)
    .into()
}
/// The last Settings sections: worker restart, automatic maintenance,
/// appearance, compatibility reports and About.
fn preferences(state: &State) -> Element<'_, Message> {
    let maintained = state
        .snapshot
        .libraries
        .iter()
        .filter(|library| library.automatic)
        .count();
    // The worker refuses a restart while any job is active or a store exists.
    let restart_blocked = if state.snapshot.jobs.iter().any(|job| job.phase.active()) {
        Some("Jobs are queued or running.")
    } else if !state.snapshot.packs.is_empty() {
        Some("Games are running from Maximum Space stores.")
    } else {
        None
    };
    let mut worker = column![
        theme::section_title("Background worker"),
        theme::muted("After an upgrade, restart when the queue is empty and Maximum Space games have been restored."),
    ]
    .spacing(10);
    if let Some(reason) = restart_blocked {
        worker = worker.push(theme::muted(reason));
    }
    worker = worker.push(secondary_maybe(
        "Restart worker",
        restart_blocked
            .is_none()
            .then_some(Message::Send(Command::Restart)),
    ));
    column![
        container(panel(worker)).id("settings-worker"),
        container(panel(
            row![
                column![
                    theme::section_title("Automatic maintenance"),
                    theme::muted(if maintained == 0 {
                        "Off".into()
                    } else {
                        format!(
                            "{} librar{} maintained",
                            maintained,
                            if maintained == 1 { "y is" } else { "ies are" }
                        )
                    })
                ]
                .spacing(4)
                .width(Length::Fill),
                secondary("Libraries", Message::GoTo(Page::Drives))
            ]
            .spacing(16)
            .align_y(Alignment::Center)
        )).id("settings-maintenance"),
        container(panel(
            column![
                theme::section_title("Appearance"),
                shell::setting_row(
                    "Theme",
                    "Use the desktop theme",
                    picker(
                        [
                            ThemePreference::System,
                            ThemePreference::Dark,
                            ThemePreference::Light
                        ],
                        Some(state.theme),
                        Message::Theme
                    )
                ),
                shell::setting_row(
                    "Motion",
                    match state.motion {
                        MotionPreference::Expressive => "Smooth transitions",
                        MotionPreference::Subtle => "Short transitions",
                        MotionPreference::Reduced => "No transitions",
                    },
                    picker(
                        [
                            MotionPreference::Expressive,
                            MotionPreference::Subtle,
                            MotionPreference::Reduced
                        ],
                        Some(state.motion),
                        Message::Motion
                    )
                )
            ]
            .spacing(18)
        )).id("settings-appearance"),
        container(panel(
            column![
                theme::section_title("Maximum Space compatibility"),
                theme::muted(format!("{} local reports. Analysis checks the exact build and installed files before enabling automatic activation.", state.reports.len())),
                secondary_maybe("Import report…", (!state.picker_busy).then_some(Message::Browse(super::dialog::Target::Report)))
            ].spacing(8)
        )).id("settings-reports"),
        container(shell::about_card(
            format!("Version {}", env!("CARGO_PKG_VERSION")),
            Message::OpenChangelog
        ))
        .id("settings-about")
    ]
    .spacing(theme::PAGE_GAP)
    .into()
}

/// A one-line card for a finished or cancelled job in History. A cancelled
/// job can be retried.
fn completed_job_row(job: &Job) -> Element<'_, Message> {
    let kind = kind_words(job);
    panel(
        row![
            if job.phase == Phase::Completed {
                Element::from(theme::accent_text("✓").size(18))
            } else {
                Element::from(text("○").size(18))
            },
            column![
                text(&job.game.title).size(15),
                theme::muted(match super::app::job_outcome(job) {
                    Some(outcome) => format!(
                        "{} · {} · {} {} · {}s",
                        kind,
                        outcome_words(outcome),
                        job.files_done,
                        if job.files_done == 1 { "file" } else { "files" },
                        job.elapsed
                    ),
                    None => format!(
                        "{} · {} {} · {} · {}s",
                        kind,
                        job.files_done,
                        if job.files_done == 1 { "file" } else { "files" },
                        size(job.bytes_done),
                        job.elapsed
                    ),
                })
            ]
            .spacing(3)
            .width(Length::Fill),
            theme::muted(job.phase.label()),
            if job.phase == Phase::Cancelled {
                secondary("Retry", Message::Send(Command::Retry(job.id)))
            } else {
                Space::new().width(0).into()
            }
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        launchers::Env,
        testutil::{TestResult, check, check_eq},
    };

    #[test]
    fn the_headline_never_contradicts_the_button_under_it() -> TestResult {
        check_eq(
            headline(0, 900_000_000, false, false),
            "About 900 MB to save".to_owned(),
            "a predicted saving is the headline",
        )?;
        check_eq(
            headline(0, 0, false, false),
            "Analyze your games".to_owned(),
            "control: with nothing predicted the prompt stays",
        )?;
        check_eq(
            headline(2_000_000_000, 900_000_000, false, false),
            "2 GB".to_owned(),
            "a saving so far leads",
        )?;
        check_eq(
            headline(0, 900_000_000, true, false),
            "Finding your games…".to_owned(),
            "a scan in progress comes first",
        )
    }

    #[test]
    fn a_job_phase_picks_the_colour_of_its_bar() -> TestResult {
        use theme::Tone;
        for (phase, expected) in [
            (Phase::Running, Tone::Normal),
            (Phase::Completed, Tone::Normal),
            (Phase::Paused, Tone::Warning),
            (Phase::Partial, Tone::Warning),
            (Phase::Failed, Tone::Failed),
            (Phase::Interrupted, Tone::Failed),
        ] {
            check_eq(tone(phase), expected, format!("{phase:?}"))?;
        }
        Ok(())
    }

    #[test]
    fn an_empty_games_page_says_why() -> TestResult {
        let mut state = State::new(Env::from_home("/fixture"));
        state.scanning = true;
        let (title, _, way_out) = empty_games(&state);
        check_eq(title, "Finding your games…", "a first scan")?;
        check(way_out.is_none(), "nothing to press while it runs")?;
        state.scanning = false;
        let (title, _, way_out) = empty_games(&state);
        check_eq(title, "No games found", "an empty library")?;
        check(
            matches!(way_out, Some((_, Message::GoTo(Page::Drives)))),
            "points at adding a folder",
        )?;
        state.games.push(GameRow {
            game: crate::model::Game {
                id: crate::model::GameId::new(crate::model::Launcher::Manual, "a"),
                also: vec![],
                title: "A".into(),
                install_dir: "/fixture/a".into(),
                build: None,
                size_hint: None,
                state: crate::model::InstallState::Idle,
                is_tool: false,
            },
            filesystem: "fixturefs".into(),
            mountpoint: None,
            supported: true,
            native_supported: true,
            pack_supported: false,
            note: None,
            artwork: None,
            cover: None,
        });
        state.query = "zzz".into();
        let (title, _, way_out) = empty_games(&state);
        check_eq(title, "No games match your search", "a search")?;
        check(
            matches!(way_out, Some((_, Message::Query(ref text))) if text.is_empty()),
            "offers to clear it",
        )?;
        state.query.clear();
        let (title, _, way_out) = empty_games(&state);
        check_eq(title, "No games match these filters", "the filters")?;
        check(
            matches!(way_out, Some((_, Message::ClearFilters))),
            "offers to clear them",
        )
    }
}
