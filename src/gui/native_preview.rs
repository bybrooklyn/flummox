//! Native fixture previews avoid launching the app, providers or storage jobs.
use super::*;
use crate::testutil::{Ctx, TestResult, check, check_eq};
#[test]
fn native_pages_render_and_preferences_keep_motion_consistent() -> TestResult {
    let temp = tempfile::tempdir().ctx("native preview fixture")?;
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
    state.folder = game_path.to_str().ctx("fixture folder text")?.into();
    state.preferences.locations.push(Location {
        path: temp.path().join("My Games"),
        kind: LocationKind::Collection,
        automatic: false,
    });
    #[cfg(windows)]
    {
        state.worker.jobs.push(crate::desktop_jobs::Job {
            id: 1,
            game: state.games.first().ctx("fixture game")?.clone(),
            restore: false,
            automatic: false,
            volume: None,
            phase: crate::desktop_jobs::Phase::Running,
            user_paused: false,
            progress: Default::default(),
            message: "Compressing files".into(),
        });
    }
    for page in [Page::Overview, Page::Games, Page::Settings] {
        state.page = page;
        for choice in [ThemeChoice::Dark, ThemeChoice::Light] {
            state.preferences.theme = choice;
            crate::gui::preview_renderer::render(
                view(&state),
                theme(&state),
                1100,
                900,
                &output.join(format!("{}-{choice}.png", page.label())),
            )?;
        }
        crate::gui::preview_renderer::render(
            view(&state),
            theme(&state),
            740,
            900,
            &output.join(format!("{}-narrow.png", page.label())),
        )?;
    }
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

#[cfg(windows)]
#[test]
fn native_snapshots_keep_jobs_on_failure_and_reject_stale_discovery() -> TestResult {
    let temp = tempfile::tempdir().ctx("snapshot fixture")?;
    let game = crate::desktop::manual_game("Current".into(), temp.path().join("Current"));
    let mut state = State {
        scanning: true,
        ..Default::default()
    };
    let current = crate::windows_coordinator::Snapshot {
        epoch: 10,
        revision: 5,
        games: vec![game.clone()],
        ..Default::default()
    };
    let _task = update(&mut state, Message::Worker(Ok(current.clone())));
    let stale = crate::windows_coordinator::Snapshot {
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
        Message::Worker(Ok(crate::windows_coordinator::Snapshot {
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
