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
    let output = std::env::var_os("FLUMMOX_PREVIEW_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temp.path().join("previews"));
    std::fs::create_dir_all(&output).ctx("preview directory")?;
    let mut state = State::new(Env::from_home(temp.path()));
    state.reduced_motion = true;
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
    state.folder = "~/My Games".into();
    state.snapshot.libraries.push(Library {
        path: "/home/player/My Games".into(),
        automatic: false,
        custom: true,
        folder_kind: FolderKind::Collection,
    });
    for (page, name) in [
        (Page::Overview, "overview"),
        (Page::Games, "games"),
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

    state.page = Page::Settings;
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
