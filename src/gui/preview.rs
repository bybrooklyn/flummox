//! Render fixture desktop states without a window or a game library.

use super::{
    app::{GameRow, Page, State},
    view,
};
use crate::{
    jobs::{FolderKind, Job, Library, Operation, PackTask, Phase},
    launchers::Env,
    model::{Game, GameId, InstallState, Launcher},
    testutil::{Ctx, TestResult},
};

/// Draws the whole window for `state` at the given size into a PNG.
fn render(state: &State, width: u32, height: u32, path: &std::path::Path) -> TestResult {
    super::preview_renderer::render(
        view::view(state),
        super::theme_of(state),
        width,
        height,
        path,
    )
}

#[test]
fn desktop_workflows_render_without_a_display() -> TestResult {
    let temp = tempfile::tempdir().ctx("preview fixture")?;
    // Set FLUMMOX_PREVIEW_DIR to keep the images. Otherwise they go to the
    // temp directory and are removed with it.
    let output = std::env::var_os("FLUMMOX_PREVIEW_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temp.path().join("previews"));
    std::fs::create_dir_all(&output).ctx("preview directory")?;
    let mut state = State::new(Env::from_home(temp.path()));
    state.reduced_motion = true;
    // One supported game with its details open and a finished analysis, so
    // every page has something to show.
    let game = Game {
        id: GameId::new(Launcher::Manual, "fixture"),
        also: vec![],
        title: "Fixture Adventure".into(),
        install_dir: temp.path().join("Fixture Adventure"),
        state: InstallState::Idle,
        size_hint: Some(4_000_000_000),
        build: Some("1".into()),
        is_tool: false,
    };
    state.games.push(GameRow {
        game: game.clone(),
        filesystem: "btrfs".into(),
        mountpoint: Some(temp.path().to_path_buf()),
        supported: true,
        native_supported: true,
        pack_supported: true,
        note: None,
        artwork: None,
        cover: None,
    });
    state.expanded = Some(game.id.to_string());
    state.detail = iced::Animation::new(true);
    state.advanced.insert(game.id.to_string());
    state.snapshot.jobs.push(Job {
        id: 1,
        game: game.clone(),
        operation: Operation::Analyze,
        options: Default::default(),
        phase: Phase::Completed,
        files_done: 140,
        bytes_done: 4_000_000_000,
        files_total: 140,
        bytes_total: 4_000_000_000,
        estimate: Some(crate::estimate::Estimate {
            disk_now: 4_000_000_000,
            disk_after: 3_100_000_000,
            maximum_after: Some(2_400_000_000),
            install_bytes: 4_000_000_000,
            sampled: 28_000_000,
            inspected_files: 140,
            ..Default::default()
        }),
        message: "Analysis complete".into(),
        errors: vec![],
        created: 0,
        elapsed: 8,
        drive_change: None,
        user_paused: false,
        pack: None,
        pack_interruptible: false,
        space_plan: None,
    });
    // More games, so the Games page shows each group of the Worth order and
    // each kind of result: a native pass, a Maximum Space game whose original
    // is still kept, one whose original is deleted, a game with nothing to
    // gain, and one not analyzed yet.
    let extra = |key: &str, title: &str| {
        let game = Game {
            id: GameId::new(Launcher::Manual, key),
            title: title.into(),
            install_dir: temp.path().join(title),
            ..game.clone()
        };
        GameRow {
            game,
            filesystem: "btrfs".into(),
            mountpoint: Some(temp.path().to_path_buf()),
            supported: true,
            native_supported: true,
            pack_supported: true,
            note: None,
            artwork: None,
            cover: None,
        }
    };
    let finished = |id: i64, row: &GameRow, operation: Operation, after: u64| Job {
        id,
        game: row.game.clone(),
        operation,
        options: Default::default(),
        phase: Phase::Completed,
        files_done: 140,
        bytes_done: 4_000_000_000,
        files_total: 140,
        bytes_total: 4_000_000_000,
        estimate: Some(crate::estimate::Estimate {
            disk_now: 4_000_000_000,
            disk_after: after,
            install_bytes: 4_000_000_000,
            sampled: 28_000_000,
            inspected_files: 140,
            ..Default::default()
        }),
        message: "Finished".into(),
        errors: vec![],
        created: 0,
        elapsed: 12,
        drive_change: None,
        user_paused: false,
        pack: None,
        pack_interruptible: false,
        space_plan: None,
    };
    let native = extra("native", "Compressed Quest");
    let flat = extra("flat", "Already Packed Racing");
    let unknown = extra("unknown", "Unchecked Tales");
    let kept = extra("kept", "Maximum With Original");
    let confirmed = extra("confirmed", "Maximum Confirmed");
    state
        .snapshot
        .jobs
        .push(finished(20, &native, Operation::Compress, 2_500_000_000));
    state
        .snapshot
        .jobs
        .push(finished(21, &flat, Operation::Analyze, 4_000_000_000));
    for (row, original) in [(&kept, true), (&confirmed, false)] {
        state.snapshot.packs.push(crate::pack::Install {
            game_path: row.game.install_dir.clone(),
            store_path: temp.path().join("store"),
            writes_path: temp.path().join("updates"),
            backup_path: original.then(|| temp.path().join("original")),
            previous_store_path: None,
            previous_writes_path: None,
            summary: serde_json::from_value(serde_json::json!({
                "files": 140,
                "logical_bytes": 4_000_000_000u64,
                "archive_bytes": 1_900_000_000u64,
                "metadata_bytes": 2_000_000,
                "unique_chunks": 900,
                "duplicate_bytes": 0
            }))
            .ctx("store summary")?,
            phase: crate::pack::InstallPhase::Mounted,
            message: "Writable compressed install is mounted".into(),
        });
    }
    for row in [native, flat, unknown, kept, confirmed] {
        state.games.push(row);
    }
    state.capture_order();
    state.folder = "~/My Games".into();
    state.snapshot.libraries.push(Library {
        path: "/home/player/My Games".into(),
        automatic: false,
        custom: true,
        folder_kind: FolderKind::Collection,
    });
    // Each main page in dark at full width, dark at the narrow width that
    // switches to the compact layout, and light at full width.
    for (page, name) in [
        (Page::Overview, "overview"),
        (Page::Games, "games"),
        (Page::Queue, "jobs"),
        (Page::Drives, "drives"),
        (Page::Settings, "settings"),
    ] {
        state.page = page;
        render(&state, 1100, 900, &output.join(format!("{name}.png")))?;
        render(&state, 720, 900, &output.join(format!("{name}-narrow.png")))?;
        state.theme = crate::jobs::ThemePreference::Light;
        render(&state, 1100, 900, &output.join(format!("{name}-light.png")))?;
        state.theme = crate::jobs::ThemePreference::Dark;
    }
    // The details pane as it first opens, with Advanced closed.
    let advanced = std::mem::take(&mut state.advanced);
    state.page = Page::Games;
    render(&state, 1100, 700, &output.join("games-details.png"))?;
    state.advanced = advanced;
    // The Games list with no row open, so every group heading is in view,
    // then with the last group expanded.
    let open = state.expanded.take();
    state.page = Page::Games;
    render(&state, 1100, 900, &output.join("games-groups.png"))?;
    state.show_low = true;
    render(&state, 1100, 1000, &output.join("games-groups-all.png"))?;
    state.show_low = false;
    // The two states of a game that runs from a store: the original still
    // kept, and the original deleted.
    for (key, name) in [
        ("manual:kept", "maximum-original"),
        ("manual:confirmed", "maximum-confirmed"),
    ] {
        state.expanded = Some(key.into());
        render(&state, 1100, 1250, &output.join(format!("{name}.png")))?;
    }
    state.expanded = open;
    // The storage plan review that precedes a job.
    state.planned = Some((
        crate::jobs::Command::Enqueue {
            game: game.clone(),
            operation: Operation::Compress,
            options: Default::default(),
        },
        crate::storage::SpacePlan {
            retained_original: true,
            requirements: vec![crate::storage::Requirement {
                volume: crate::storage::Volume {
                    identity: "fixture".into(),
                    path: "/Games".into(),
                    available: 10_000_000_000,
                },
                additional: 4_000_000_000,
                headroom: 200_000_000,
                reasons: vec!["Verified store; original retained".into()],
            }],
        },
    ));
    render(&state, 1100, 900, &output.join("space-plan.png"))?;
    state.planned = None;
    state.page = Page::Recovery;
    if let Some(job) = state.snapshot.jobs.first_mut() {
        job.phase = Phase::Interrupted;
        job.message = "Worker stopped; original retained".into();
    }
    render(&state, 1100, 720, &output.join("recovery.png"))?;
    state.page = Page::Games;
    state.qualification = Some(crate::qualification::Wizard::new(
        game.clone(),
        crate::compatibility::Corpus {
            sha256: "a".repeat(64),
            files: 140,
            bytes: 4_000_000_000,
        },
    ));
    render(&state, 720, 900, &output.join("qualification.png"))?;
    state.qualification = None;
    // A running pack job, first interruptible with progress and then in the
    // step that offers no pause or cancel and reports no totals.
    state.snapshot.jobs.push(Job {
        id: 2,
        game,
        operation: Operation::Pack,
        pack: Some(PackTask::Compact),
        phase: Phase::Running,
        message: "Building Maximum Space store".into(),
        files_done: 30,
        bytes_done: 800_000_000,
        files_total: 140,
        bytes_total: 4_000_000_000,
        estimate: None,
        options: Default::default(),
        errors: vec![],
        created: 0,
        elapsed: 12,
        drive_change: None,
        user_paused: false,
        pack_interruptible: true,
        space_plan: None,
    });
    state.page = Page::Queue;
    render(&state, 1100, 720, &output.join("queue.png"))?;
    if let Some(job) = state.snapshot.jobs.last_mut() {
        job.pack_interruptible = false;
        job.files_done = 0;
        job.bytes_done = 0;
        job.files_total = 0;
        job.bytes_total = 0;
        job.message = "Switching stores; this step finishes before stopping".into();
    }
    render(&state, 720, 720, &output.join("queue-switching.png"))?;

    // One job in each phase the Jobs page groups by.
    state.page = Page::Queue;
    state.snapshot_loaded = true;
    let mut job = state
        .snapshot
        .jobs
        .last()
        .ctx("fixture storage job")?
        .clone();
    state.snapshot.jobs.clear();
    job.operation = Operation::Compress;
    job.pack = None;
    job.pack_interruptible = false;
    job.files_total = 140;
    job.bytes_total = 4_000_000_000;
    for (id, title, phase, paused, message) in [
        (3, "Adventure", Phase::Running, false, "Compressing files"),
        (4, "Puzzle", Phase::Paused, true, "Paused by the user"),
        (5, "Racing", Phase::Queued, false, "Waiting for another job"),
        (
            6,
            "Strategy",
            Phase::Partial,
            false,
            "One file needs review",
        ),
        (7, "Arcade", Phase::Completed, false, "Compression complete"),
    ] {
        let done = if phase == Phase::Queued {
            0
        } else if phase == Phase::Completed {
            140
        } else {
            10
        };
        state.snapshot.jobs.push(Job {
            id,
            game: Game {
                id: GameId::new(Launcher::Manual, format!("fixture-{id}")),
                title: title.into(),
                ..job.game.clone()
            },
            phase,
            files_done: done,
            bytes_done: done * 25_000_000,
            user_paused: paused,
            message: message.into(),
            ..job.clone()
        });
    }
    render(&state, 1100, 1800, &output.join("jobs-phases.png"))?;
    render(&state, 720, 1800, &output.join("jobs-phases-narrow.png"))?;
    state.connection_error = Some("Worker disconnected; reconnecting".into());
    render(&state, 1100, 1800, &output.join("jobs-disconnected.png"))
}
