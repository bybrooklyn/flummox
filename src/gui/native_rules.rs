//! Decisions of the Windows and macOS window that need no window to check:
//! which control is disabled and why, which job a folder belongs to, and how
//! a location or an exclusion changes the preferences. Compiled on Linux for
//! its tests only.

// The window that calls these is not built on Linux, so its tests are the only
// callers there.
#![cfg_attr(target_os = "linux", allow(dead_code))]

use super::theme::Tone;
use crate::{
    desktop::{Location, LocationKind},
    desktop_jobs::{Job, Phase},
    model::Game,
};
use std::path::{Path, PathBuf};

/// The game whose install directory is `path`.
pub fn game_at<'a>(games: &'a [Game], path: &Path) -> Option<&'a Game> {
    games.iter().find(|game| game.install_dir == path)
}

/// Why Compress is unavailable, or `None` when it can start.
pub fn compress_block(
    folder_empty: bool,
    game: Option<&Game>,
    excluded: bool,
) -> Option<&'static str> {
    if folder_empty {
        Some("Choose a game folder first")
    } else if excluded {
        Some("This game is excluded. Include it again to compress it")
    } else if game.is_some_and(|game| !game.state.is_idle()) {
        Some("This game is busy or updating. Try again when it is idle")
    } else {
        None
    }
}

/// Why Decompress is unavailable. An excluded game can still be restored.
pub fn decompress_block(folder_empty: bool) -> Option<&'static str> {
    folder_empty.then_some("Choose a game folder first")
}

/// Why the compatibility form cannot open for the selected folder.
pub fn qualify_block(game: Option<&Game>) -> Option<&'static str> {
    match game {
        None => Some("Select a game from the list to qualify it"),
        Some(game) if !game.state.is_idle() => Some("This game is busy or updating"),
        Some(_) => None,
    }
}

/// Whether the selected game's latest job ended badly, or the game itself is
/// not ready.
pub fn needs_attention(game: &Game, latest: Option<Phase>) -> bool {
    !game.state.is_idle() || matches!(latest, Some(Phase::Failed | Phase::Interrupted))
}

/// The phase of the newest job for `game`, if any.
pub fn latest_phase(jobs: &[Job], game: &Game) -> Option<Phase> {
    jobs.iter()
        .rev()
        .find(|job| job.game.id == game.id)
        .map(|job| job.phase)
}

/// The active job on `folder`. A running or paused job wins over one that is
/// still waiting.
pub fn job_for_folder<'a>(jobs: &'a [Job], folder: &Path) -> Option<&'a Job> {
    let on_folder = || {
        jobs.iter()
            .rev()
            .filter(move |job| job.phase.active() && job.game.install_dir == folder)
    };
    on_folder()
        .find(|job| job.phase != Phase::Waiting)
        .or_else(|| on_folder().next())
}

/// Whether a retained record for `root` belongs to a job that is still active.
/// Either path may be the parent of the other.
pub fn recovery_in_use(jobs: &[Job], root: &Path) -> bool {
    jobs.iter().any(|job| {
        job.phase.active()
            && (job.game.install_dir.starts_with(root) || root.starts_with(&job.game.install_dir))
    })
}

/// How many jobs are waiting, running or paused.
pub fn active_jobs(jobs: &[Job]) -> usize {
    jobs.iter().filter(|job| job.phase.active()).count()
}

/// The colour a job's phase label draws in.
pub fn job_tone(phase: Phase) -> Tone {
    match phase {
        Phase::Paused => Tone::Warning,
        Phase::Failed | Phase::Interrupted => Tone::Failed,
        _ => Tone::Normal,
    }
}

/// "Compression" or "Decompression".
pub fn operation_words(optimize: bool) -> &'static str {
    if optimize {
        "Compression"
    } else {
        "Decompression"
    }
}

/// The line under a game's title: the phase of its job when one is active or
/// ended badly, else the launcher, else what keeps the game from being used.
pub fn game_status(game: &Game, phase: Option<Phase>) -> String {
    if let Some(phase) = phase
        && (phase.active() || matches!(phase, Phase::Failed | Phase::Interrupted))
    {
        return phase.to_string();
    }
    if game.state.is_idle() {
        return game.id.launcher.label().to_owned();
    }
    let text = game.state.to_string();
    let mut letters = text.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => text,
    }
}

/// What `apply_location` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationChange {
    Added,
    Retyped,
    Unchanged,
}

/// Adds `path` to the locations. A path already listed has its kind changed
/// when `retype` is set and is left alone otherwise.
pub fn apply_location(
    locations: &mut Vec<Location>,
    path: PathBuf,
    kind: LocationKind,
    retype: bool,
) -> LocationChange {
    match locations.iter_mut().find(|location| location.path == path) {
        Some(old) if retype && old.kind != kind => {
            old.kind = kind;
            LocationChange::Retyped
        }
        Some(_) => LocationChange::Unchanged,
        None => {
            locations.push(Location {
                path,
                kind,
                automatic: false,
            });
            LocationChange::Added
        }
    }
}

/// Excludes or includes a game. Every id the game answers to is removed first,
/// so none stays listed, and `primary` is added back when excluding.
pub fn set_excluded(excluded: &mut Vec<String>, ids: &[String], primary: &str, exclude: bool) {
    excluded.retain(|old| !ids.contains(old));
    if exclude {
        excluded.push(primary.to_owned());
    }
}

/// Whether any of `ids` is excluded.
pub fn is_excluded(excluded: &[String], ids: &[String]) -> bool {
    ids.iter().any(|id| excluded.contains(id))
}

/// The line a finished scan announces, or `None` when it stays quiet. An
/// error on screen is not replaced.
pub fn scan_announcement(count: usize, quiet: bool, error_showing: bool) -> Option<String> {
    (!quiet && !error_showing).then(|| {
        format!(
            "Found {count} game{}.",
            if count == 1 { "" } else { "s" }
        )
    })
}

/// What stands in the way of changing preferences, if anything.
pub fn preferences_note(loaded: bool, error: Option<&str>) -> Option<String> {
    match (loaded, error) {
        (true, _) => None,
        (false, Some(error)) => Some(format!(
            "Preferences could not be loaded, so locations and settings cannot be changed. {error}"
        )),
        (false, None) => Some("Loading preferences…".to_owned()),
    }
}

/// Running totals for the progress box, from either platform's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub changed: u64,
    pub skipped: u64,
    pub bytes: u64,
    pub allocation_before: u64,
    pub allocation_after: u64,
}

impl Totals {
    /// Space freed so far. Allocation is summed over the files visited, so
    /// this grows with the job.
    pub fn freed(&self) -> u64 {
        self.allocation_before
            .saturating_sub(self.allocation_after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        desktop::manual_game,
        model::{BusyReason, InstallState},
        testutil::{TestResult, check, check_eq},
    };

    fn job(id: u64, folder: &str, phase: Phase) -> Job {
        Job {
            id,
            game: manual_game(format!("Game {id}"), PathBuf::from(folder)),
            restore: false,
            automatic: false,
            volume: None,
            phase,
            user_paused: false,
            progress: Default::default(),
            message: String::new(),
        }
    }

    #[test]
    fn compress_needs_a_folder_an_included_idle_game() -> TestResult {
        let mut game = manual_game("A".into(), PathBuf::from("/games/a"));
        check_eq(
            compress_block(false, Some(&game), false),
            None,
            "control: an idle included game",
        )?;
        check(compress_block(true, None, false).is_some(), "no folder")?;
        check(
            compress_block(false, Some(&game), true).is_some(),
            "excluded game",
        )?;
        game.state = InstallState::UpdatePending;
        check(
            compress_block(false, Some(&game), false).is_some(),
            "a game with an update pending",
        )?;
        check_eq(
            decompress_block(false),
            None,
            "restoring an excluded game is allowed",
        )?;
        check(decompress_block(true).is_some(), "restoring needs a folder")
    }

    #[test]
    fn qualify_needs_a_known_idle_game() -> TestResult {
        let mut game = manual_game("A".into(), PathBuf::from("/games/a"));
        check(qualify_block(None).is_some(), "an unknown folder")?;
        check_eq(qualify_block(Some(&game)), None, "control: a known game")?;
        game.state = InstallState::Busy(BusyReason::Running);
        check(qualify_block(Some(&game)).is_some(), "a busy game")
    }

    #[test]
    fn a_folder_belongs_to_its_own_active_job() -> TestResult {
        let jobs = vec![
            job(1, "/games/a", Phase::Running),
            job(2, "/games/b", Phase::Waiting),
            job(3, "/games/b", Phase::Completed),
        ];
        check_eq(
            job_for_folder(&jobs, Path::new("/games/a")).map(|job| job.id),
            Some(1),
            "the running job on its own folder",
        )?;
        check_eq(
            job_for_folder(&jobs, Path::new("/games/b")).map(|job| job.id),
            Some(2),
            "a waiting job counts and a finished one does not",
        )?;
        check_eq(
            job_for_folder(&jobs, Path::new("/games/c")).map(|job| job.id),
            None,
            "control: a folder with no job",
        )
    }

    #[test]
    fn a_running_job_wins_over_a_waiting_one_on_the_same_folder() -> TestResult {
        let jobs = vec![
            job(1, "/games/a", Phase::Running),
            job(2, "/games/a", Phase::Waiting),
        ];
        check_eq(
            job_for_folder(&jobs, Path::new("/games/a")).map(|job| job.id),
            Some(1),
            "running first",
        )
    }

    #[test]
    fn recovery_is_hidden_while_its_folder_has_a_job() -> TestResult {
        let jobs = vec![job(1, "/games/a", Phase::Running)];
        check(recovery_in_use(&jobs, Path::new("/games/a")), "same folder")?;
        check(
            recovery_in_use(&jobs, Path::new("/games")),
            "a parent of the job's folder",
        )?;
        check(
            !recovery_in_use(&jobs, Path::new("/games/b")),
            "control: another folder",
        )?;
        let finished = vec![job(1, "/games/a", Phase::Completed)];
        check(
            !recovery_in_use(&finished, Path::new("/games/a")),
            "a finished job does not hide it",
        )
    }

    #[test]
    fn remembering_a_listed_folder_leaves_its_kind() -> TestResult {
        let mut locations = vec![Location {
            path: PathBuf::from("/games"),
            kind: LocationKind::Collection,
            automatic: true,
        }];
        check_eq(
            apply_location(
                &mut locations,
                PathBuf::from("/games"),
                LocationKind::Game,
                false,
            ),
            LocationChange::Unchanged,
            "remembering a library",
        )?;
        check_eq(
            locations.first().map(|location| location.kind),
            Some(LocationKind::Collection),
            "the kind is unchanged",
        )?;
        check_eq(
            apply_location(
                &mut locations,
                PathBuf::from("/games"),
                LocationKind::Game,
                true,
            ),
            LocationChange::Retyped,
            "control: adding with a kind retypes it",
        )?;
        check_eq(
            apply_location(
                &mut locations,
                PathBuf::from("/other"),
                LocationKind::Game,
                false,
            ),
            LocationChange::Added,
            "a new path",
        )?;
        check_eq(locations.len(), 2, "two locations")
    }

    #[test]
    fn including_a_game_clears_every_id_it_answers_to() -> TestResult {
        let ids = vec!["steam:1".to_owned(), "steam:2".to_owned()];
        let mut excluded = vec!["steam:2".to_owned(), "gog:9".to_owned()];
        check(is_excluded(&excluded, &ids), "the alias counts")?;
        set_excluded(&mut excluded, &ids, "steam:1", false);
        check_eq(excluded.clone(), vec!["gog:9".to_owned()], "alias removed")?;
        set_excluded(&mut excluded, &ids, "steam:1", true);
        check_eq(
            excluded,
            vec!["gog:9".to_owned(), "steam:1".to_owned()],
            "excluded once, by its primary id",
        )
    }

    #[test]
    fn a_scan_announces_itself_only_when_asked_and_no_error_shows() -> TestResult {
        check_eq(
            scan_announcement(3, false, false),
            Some("Found 3 games.".to_owned()),
            "a manual scan",
        )?;
        check_eq(
            scan_announcement(1, false, false),
            Some("Found 1 game.".to_owned()),
            "one game",
        )?;
        check_eq(scan_announcement(3, true, false), None, "a timer scan")?;
        check_eq(scan_announcement(3, false, true), None, "an error is kept")
    }

    #[test]
    fn unloaded_preferences_give_a_reason() -> TestResult {
        check_eq(preferences_note(true, None), None, "loaded")?;
        check(
            preferences_note(false, Some("bad json"))
                .is_some_and(|text| text.contains("bad json")),
            "a failed load says why",
        )?;
        check(preferences_note(false, None).is_some(), "still loading")
    }

    #[test]
    fn attention_covers_failed_jobs_and_unready_games() -> TestResult {
        let mut game = manual_game("A".into(), PathBuf::from("/games/a"));
        check(!needs_attention(&game, None), "control: an idle game")?;
        check(
            needs_attention(&game, Some(Phase::Failed)),
            "a failed job",
        )?;
        check(
            !needs_attention(&game, Some(Phase::Completed)),
            "a completed job",
        )?;
        game.state = InstallState::UpdatePending;
        check(needs_attention(&game, None), "an update pending")
    }

    #[test]
    fn a_row_shows_the_launcher_or_what_is_wrong() -> TestResult {
        let mut game = manual_game("A".into(), PathBuf::from("/games/a"));
        check_eq(
            game_status(&game, None),
            "Manual".to_owned(),
            "an idle game names its launcher",
        )?;
        check_eq(
            game_status(&game, Some(Phase::Running)),
            "Running".to_owned(),
            "a running job",
        )?;
        game.state = InstallState::UpdatePending;
        check_eq(
            game_status(&game, None),
            "Update pending".to_owned(),
            "worded, not the raw state",
        )
    }

    #[test]
    fn totals_report_the_space_freed_so_far() -> TestResult {
        let totals = Totals {
            allocation_before: 900,
            allocation_after: 400,
            ..Totals::default()
        };
        check_eq(totals.freed(), 500, "before minus after")?;
        check_eq(
            Totals {
                allocation_before: 100,
                allocation_after: 200,
                ..Totals::default()
            }
            .freed(),
            0,
            "control: growth does not underflow",
        )
    }

    #[test]
    fn job_phases_pick_tones() -> TestResult {
        check_eq(job_tone(Phase::Running), Tone::Normal, "running")?;
        check_eq(job_tone(Phase::Paused), Tone::Warning, "paused")?;
        check_eq(job_tone(Phase::Interrupted), Tone::Failed, "interrupted")
    }
}
