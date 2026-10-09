//! Native fixture previews avoid launching the app, providers or storage jobs.
use super::*;
use crate::desktop::Location;
use crate::testutil::{Ctx, TestResult, check, check_eq};

/// A plan one volume short of its requirement, so the review shows a failure.
fn failing_plan() -> crate::storage::SpacePlan {
    crate::storage::SpacePlan {
        retained_original: true,
        requirements: vec![crate::storage::Requirement {
            volume: crate::storage::Volume {
                identity: "fixture".into(),
                path: PathBuf::from("/Games"),
                available: 1_000,
            },
            additional: 4_000_000,
            headroom: 200_000,
            reasons: vec!["The largest file is rewritten in place".into()],
        }],
    }
}

/// Renders the current page of `state` in both themes wide and once narrow.
fn render_all(state: &mut State, output: &Path, name: &str) -> TestResult {
    // A fixture sets the page directly, so no highlight animation ran. Reduced
    // motion makes the sidebar light the entry for the page being drawn.
    let motion = std::mem::replace(&mut state.preferences.motion, MotionChoice::Reduced);
    for choice in [ThemeChoice::Dark, ThemeChoice::Light] {
        state.preferences.theme = choice;
        crate::gui::preview_renderer::render(
            view(state),
            theme(state),
            1100,
            900,
            &output.join(format!("{name}-{choice}.png")),
        )?;
    }
    let narrow = crate::gui::preview_renderer::render(
        view(state),
        theme(state),
        740,
        900,
        &output.join(format!("{name}-narrow.png")),
    );
    state.preferences.motion = motion;
    narrow
}

#[test]
fn native_pages_render_and_preferences_keep_motion_consistent() -> TestResult {
    let temp = tempfile::tempdir().ctx("native preview fixture")?;
    // Set FLUMMOX_NATIVE_PREVIEW_DIR to keep the PNGs. Otherwise they go to the
    // temp directory and are removed with it.
    let output = std::env::var_os("FLUMMOX_NATIVE_PREVIEW_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| temp.path().join("previews"));
    std::fs::create_dir_all(&output).ctx("preview output")?;
    let mut state = State {
        preferences_loaded: true,
        scanning: false,
        location_input: "~/My Games".into(),
        ..Default::default()
    };
    let game_path = temp.path().join("My Games/Fixture Adventure");
    state.games.push(crate::desktop::manual_game(
        "Fixture Adventure".into(),
        game_path.clone(),
    ));
    state.games.push(crate::desktop::manual_game(
        "Second Fixture".into(),
        temp.path().join("My Games/Second Fixture"),
    ));
    state.folder = game_path.to_str().ctx("fixture folder text")?.into();
    state.preferences.locations.push(Location {
        path: temp.path().join("My Games"),
        kind: LocationKind::Collection,
        automatic: false,
    });
    state.warnings.push("D:\\Library could not be read".into());
    #[cfg(windows)]
    {
        let game = state.games.first().ctx("fixture game")?.clone();
        for (id, phase) in [
            (1, crate::desktop_jobs::Phase::Running),
            (2, crate::desktop_jobs::Phase::Waiting),
            (3, crate::desktop_jobs::Phase::Failed),
            (4, crate::desktop_jobs::Phase::Completed),
            (5, crate::desktop_jobs::Phase::Cancelled),
        ] {
            state.worker.jobs.push(crate::desktop_jobs::Job {
                id,
                game: game.clone(),
                restore: false,
                automatic: false,
                volume: None,
                phase,
                user_paused: false,
                progress: Default::default(),
                message: "Compressing files".into(),
            });
        }
    }
    // Every page in both themes at 1100 px, then once at 740 px, which is under the
    // compact threshold. The folder is the first game's, so its row is selected.
    for page in [Page::Overview, Page::Games, Page::Jobs, Page::Settings] {
        state.page = page;
        render_all(&mut state, &output, page.label())?;
    }
    // The Games page with its advanced tools open.
    state.page = Page::Games;
    state.advanced = true;
    render_all(&mut state, &output, "Games-advanced")?;
    state.advanced = false;
    // A toast over the page, informational and then an error.
    state.info("Found 2 games.");
    render_all(&mut state, &output, "Games-toast")?;
    state.error("This game is excluded. Include it before running a job.");
    render_all(&mut state, &output, "Games-error-toast")?;
    // A plan that does not fit is reviewed above the page.
    state.planned = Some((game_path.clone(), true, failing_plan()));
    render_all(&mut state, &output, "Games-plan")?;
    state.page = Page::Settings;
    render_all(&mut state, &output, "Settings-plan")?;
    state.planned = None;
    // The first scan, with nothing listed yet, and then an empty library.
    let games = std::mem::take(&mut state.games);
    state.page = Page::Games;
    state.scanning = true;
    render_all(&mut state, &output, "Games-scanning")?;
    state.scanning = false;
    render_all(&mut state, &output, "Games-empty")?;
    state.games = games;
    state.page = Page::Settings;
    state.preferences_loaded = false;
    state.preferences_error = Some("desktop.json is not valid".into());
    render_all(&mut state, &output, "Settings-unloaded")?;
    state.preferences_loaded = true;
    state.preferences_error = None;

    state.page = Page::Settings;
    state.preferences.motion = MotionChoice::Reduced;
    let _task = update(&mut state, Message::GoTo(Page::Overview));
    check_eq(state.direction, -1.0, "earlier navigation moves down")?;
    check_eq(
        state.preferences.motion.distance(),
        0.0,
        "reduced motion removes displacement",
    )?;
    let _task = update(&mut state, Message::GoTo(Page::Games));
    check_eq(state.direction, 1.0, "later navigation moves up")?;
    check(!state.games.is_empty(), "navigation preserves the library")
}

#[test]
fn a_toast_leaves_on_dismissal_and_a_scan_does_not_replace_an_error() -> TestResult {
    let mut state = State {
        scanning: true,
        refreshing: true,
        ..Default::default()
    };
    state.preferences.motion = MotionChoice::Reduced;
    state.error("Could not refresh the game list: the library is unreadable");
    let scan = Scan {
        #[cfg(windows)]
        stamp: (0, 0),
        games: vec![],
        warnings: vec![],
        artwork: Default::default(),
        covers: Default::default(),
    };
    let _task = update(&mut state, Message::Scanned(Ok(scan.clone())));
    check(
        state
            .toast
            .status
            .as_ref()
            .is_some_and(|toast| toast.is_error),
        "a quiet scan leaves the error up",
    )?;
    check(!state.refreshing, "the quiet flag is spent")?;
    state.scanning = true;
    let _task = update(&mut state, Message::Scanned(Ok(scan)));
    check(
        state
            .toast
            .status
            .as_ref()
            .is_some_and(|toast| toast.is_error),
        "a announced scan does not replace an error either",
    )?;
    state.scanning = true;
    state.refreshing = true;
    let _task = update(&mut state, Message::Scanned(Err("gone".into())));
    check(!state.refreshing, "the error path spends the quiet flag")?;
    let _task = update(&mut state, Message::Dismiss);
    check(state.toast.status.is_none(), "dismissal hides the toast")
}

#[test]
fn remembering_a_library_keeps_it_a_library() -> TestResult {
    let temp = tempfile::tempdir().ctx("remember fixture")?;
    let mut state = State {
        preferences_loaded: true,
        ..Default::default()
    };
    state.preferences.locations.push(Location {
        path: temp.path().to_path_buf(),
        kind: LocationKind::Collection,
        automatic: false,
    });
    let _task = update(
        &mut state,
        Message::LocationResolved(
            LocationKind::Game,
            Intent::Remember,
            Ok(temp.path().to_path_buf()),
        ),
    );
    check_eq(
        state.preferences.locations.first().map(|found| found.kind),
        Some(LocationKind::Collection),
        "the kind is unchanged",
    )?;
    check_eq(state.preferences.locations.len(), 1, "no second entry")
}

#[test]
fn escape_closes_the_plan_and_leaves_the_wizard() -> TestResult {
    let temp = tempfile::tempdir().ctx("escape fixture")?;
    let game = crate::desktop::manual_game("Escape".into(), temp.path().join("Escape"));
    let corpus = crate::compatibility::Corpus {
        sha256: "ab".repeat(32),
        files: 1,
        bytes: 1,
    };
    let mut state = State {
        planned: Some((game.install_dir.clone(), true, failing_plan())),
        ..State::default()
    };
    state.qualification = Some(crate::qualification::Wizard::new(game, corpus));
    let _task = update(
        &mut state,
        Message::Key(iced::keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Escape),
            location: iced::keyboard::Location::Standard,
            modifiers: iced::keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }),
    );
    check(state.planned.is_none(), "the plan closed")?;
    check(state.qualification.is_some(), "the wizard stayed open")
}

#[test]
fn a_failed_save_restores_the_last_saved_preferences() -> TestResult {
    let mut state = State {
        preferences_loaded: true,
        ..Default::default()
    };
    state.preferences.theme = ThemeChoice::Light;
    state.in_flight = Some(state.preferences.clone());
    state.preferences_dirty = true;
    state.refresh_after_save = true;
    let _task = update(
        &mut state,
        Message::PreferencesSaved(Err("disk full".into())),
    );
    check_eq(
        state.preferences.theme,
        ThemeChoice::System,
        "the unsaved choice is undone",
    )?;
    check(
        !state.preferences_dirty && !state.refresh_after_save,
        "queued work is dropped",
    )?;
    check(state.toast.status.is_some(), "the failure is announced")
}

#[cfg(windows)]
#[test]
fn a_refused_command_is_a_toast_and_not_a_lost_connection() -> TestResult {
    let mut state = State::default();
    let _task = update(
        &mut state,
        Message::Commanded(Err("This game is excluded. Include it before running a job.".into())),
    );
    check(state.worker_error.is_none(), "not a connection error")?;
    check(
        state
            .toast
            .status
            .as_ref()
            .is_some_and(|toast| toast.is_error && toast.text.contains("excluded")),
        "shown as a toast",
    )?;
    check(
        state.toast.deadline.is_some(),
        "a refusal leaves after a few seconds",
    )
}

#[cfg(windows)]
#[test]
fn native_snapshots_keep_jobs_on_failure_and_reject_stale_discovery() -> TestResult {
    let temp = tempfile::tempdir().ctx("snapshot fixture")?;
    let game = crate::desktop::manual_game("Current".into(), temp.path().join("Current"));
    let mut state = State {
        scanning: true,
        ..Default::default()
    };
    let current = coordinator::Snapshot {
        epoch: 10,
        revision: 5,
        games: vec![game.clone()],
        ..Default::default()
    };
    let _task = update(&mut state, Message::Worker(Ok(current.clone())));
    // Same epoch, lower revision, and no games: must be ignored.
    let stale = coordinator::Snapshot {
        epoch: 10,
        revision: 4,
        ..Default::default()
    };
    let _task = update(&mut state, Message::Worker(Ok(stale)));
    check_eq(
        state.games.len(),
        1,
        "older snapshots do not empty the library",
    )?;
    let _task = update(&mut state, Message::Worker(Err("Disconnected".into())));
    check_eq(
        state.games.len(),
        1,
        "poll failure retains the last valid library",
    )?;
    // A scan stamped before the snapshot on screen, with a different game list.
    let _task = update(
        &mut state,
        Message::Scanned(Ok(Scan {
            stamp: (10, 4),
            games: vec![],
            warnings: vec![],
            artwork: Default::default(),
            covers: Default::default(),
        })),
    );
    check_eq(
        state.games.first().ctx("retained game")?.title.clone(),
        game.title,
        "late image scan cannot clear current games",
    )?;
    let _task = update(
        &mut state,
        Message::Worker(Ok(coordinator::Snapshot {
            revision: 6,
            stopping: true,
            ..current
        })),
    );
    check(
        !state.worker_enabled,
        "stopping the worker disables automatic polling",
    )
}

#[test]
fn library_search_sort_and_updates_survive_navigation() -> TestResult {
    let temp = tempfile::tempdir().ctx("library controls fixture")?;
    let mut state = State {
        scanning: false,
        ..Default::default()
    };
    let one = crate::desktop::manual_game("Zebra".into(), temp.path().join("Zebra"));
    let mut two = crate::desktop::manual_game("Adventure".into(), temp.path().join("Adventure"));
    two.build = Some("1".into());
    replace_games(&mut state, vec![one.clone(), two.clone()]);
    check_eq(
        filtered_games(&state)
            .first()
            .ctx("sorted first")?
            .title
            .clone(),
        "Adventure".to_owned(),
        "default title ordering",
    )?;
    // A second list with a new build for one game is what marks it updated.
    two.build = Some("2".into());
    replace_games(&mut state, vec![one, two]);
    let _task = update(&mut state, Message::Filter(GameFilter::Updated));
    check_eq(
        filtered_games(&state).len(),
        1,
        "changed build appears in updated filter",
    )?;
    let _task = update(&mut state, Message::Query("ADVEN".into()));
    let _task = update(&mut state, Message::GoTo(Page::Settings));
    let _task = update(&mut state, Message::GoTo(Page::Games));
    check_eq(
        filtered_games(&state).len(),
        1,
        "navigation preserves search and filter",
    )
}
