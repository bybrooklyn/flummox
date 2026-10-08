//! Durable native job states and maintenance baselines, independent of IPC.
use crate::{desktop::Preferences, model::Game};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::Path,
};
/// Where a job is in its life. The first three are active, the rest are history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// Queued and not yet started.
    Waiting,
    Running,
    /// Started, then held. The job's message says why.
    Paused,
    Completed,
    /// Stopped by the user, an exclusion or a worker shutdown. Displayed as "Stopped".
    Cancelled,
    Failed,
    /// Was running or paused when the worker stopped. `Queue::load` assigns it.
    Interrupted,
}
impl Phase {
    /// True while the job still occupies its game: waiting, running or paused.
    pub fn active(self) -> bool {
        matches!(self, Self::Waiting | Self::Running | Self::Paused)
    }
}
impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Waiting => "Waiting",
            Self::Running => "Running",
            Self::Paused => "Paused",
            Self::Completed => "Completed",
            Self::Cancelled => "Stopped",
            Self::Failed => "Failed",
            Self::Interrupted => "Interrupted",
        })
    }
}
/// Running totals for one job. Each update replaces the previous one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    /// Files visited so far.
    pub files: u64,
    pub changed: u64,
    pub skipped: u64,
    /// Logical size of the visited files.
    pub bytes: u64,
    /// Allocated size of the visited files before each was processed, and after.
    pub allocation_before: u64,
    pub allocation_after: u64,
}
/// One queued or finished storage operation on one game folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Unique within the queue and kept across restarts and retries.
    pub id: u64,
    pub game: Game,
    /// True restores ordinary storage. False compresses.
    pub restore: bool,
    /// Queued by maintenance. Such a job is cancelled when its location opts out.
    #[serde(default)]
    pub automatic: bool,
    /// The volume the folder was on when queued. The job runs only while a volume
    /// with the same identity is mounted there.
    #[serde(default)]
    pub volume: Option<crate::storage::Volume>,
    pub phase: Phase,
    pub user_paused: bool,
    pub progress: Progress,
    /// The line shown under the job: its current wait reason or its final result.
    pub message: String,
}
/// Contents of `native-queue.json`: the job list and what maintenance has seen.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Queue {
    pub jobs: Vec<Job>,
    /// Per game id: the build last recorded, and whether maintenance covered the
    /// game at that time. `observe` compares new scans against it.
    pub baseline: HashMap<String, (Option<String>, bool)>,
    /// The highest id handed out so far.
    pub next_id: u64,
    /// Hashed paths of the automatic locations as of the last healthy scan. A game
    /// first seen under one of them counts as a new install.
    pub initialized_locations: HashSet<String>,
}
impl Queue {
    /// Loads the queue from `root`, or an empty one if the file does not exist. Jobs
    /// saved as running or paused become `Interrupted`, as do waiting jobs saved
    /// without a volume.
    pub fn load(root: &Path) -> Result<Self> {
        Self::load_inner(&root.join("native-queue.json"))
    }
    /// `load`, except that a file which cannot be parsed is renamed aside and an
    /// empty queue is returned with a note saying where the old one went. A file
    /// that cannot be read at all is still an error, since nothing was set aside.
    pub fn load_or_quarantine(root: &Path) -> Result<(Self, Option<String>)> {
        let path = root.join("native-queue.json");
        match Self::load_inner(&path) {
            Ok(queue) => Ok((queue, None)),
            Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
                let aside = crate::desktop::quarantine(&path)?;
                Ok((
                    Self::default(),
                    Some(format!(
                        "The job history could not be read ({error}). It was kept as {}.",
                        aside.display()
                    )),
                ))
            }
            Err(error) => Err(error),
        }
    }
    fn load_inner(path: &Path) -> Result<Self> {
        let mut queue: Self = match crate::desktop::read_bounded(path, 16 * 1024 * 1024) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Self::default()
            }
            Err(error) => return Err(error),
        };
        ensure!(
            queue.jobs.len() <= 1000,
            "Native job history exceeds its limit"
        );
        for job in &mut queue.jobs {
            if matches!(job.phase, Phase::Running | Phase::Paused)
                || (job.phase == Phase::Waiting && job.volume.is_none())
            {
                job.phase = Phase::Interrupted;
                job.message = "Worker stopped. Review recovery before retrying.".into();
            }
        }
        // Guards against a file whose `next_id` is behind its own jobs.
        queue.next_id = queue.next_id.max(
            queue
                .jobs
                .iter()
                .map(|job| job.id)
                .max()
                .unwrap_or_default(),
        );
        Ok(queue)
    }
    /// Replaces the queue file atomically: temp file, fsync, rename, directory fsync.
    pub fn save(&self, root: &Path) -> Result<()> {
        crate::libraries::private_dir(root)?;
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= 16 * 1024 * 1024,
            "Native job history exceeds 16 MiB"
        );
        let mut file = tempfile::NamedTempFile::new_in(root)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(root.join("native-queue.json"))?;
        #[cfg(unix)]
        std::fs::File::open(root)?.sync_all()?;
        Ok(())
    }
    /// Adds a waiting job and returns its id. If the folder already has an active job
    /// of the same kind, returns that job's id and adds nothing. Fails for a game
    /// that is not idle, for the opposite kind of active job, and past 200 active jobs.
    pub fn enqueue(&mut self, game: Game, restore: bool) -> Result<u64> {
        ensure!(
            game.state.is_idle(),
            "Wait for the launcher or game to finish"
        );
        if let Some(job) = self
            .jobs
            .iter()
            .find(|job| job.phase.active() && job.game.install_dir == game.install_dir)
        {
            ensure!(
                job.restore == restore,
                "Cancel the existing job before switching between compression and restoration"
            );
            return Ok(job.id);
        }
        ensure!(
            self.jobs.iter().filter(|job| job.phase.active()).count() < 200,
            "The queue is full"
        );
        // History is capped at 1000 jobs. Room is made by dropping the oldest completed
        // or cancelled job. Failed and interrupted jobs are never dropped this way.
        while self.jobs.len() >= 1000 {
            if let Some(index) = self
                .jobs
                .iter()
                .position(|job| matches!(job.phase, Phase::Completed | Phase::Cancelled))
            {
                self.jobs.remove(index);
            } else {
                anyhow::bail!("Review the retained job history before adding more jobs");
            }
        }
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Job IDs exhausted"))?;
        let id = self.next_id;
        let volume = Some(crate::storage::volume(&game.install_dir)?);
        self.jobs.push(Job {
            id,
            game,
            restore,
            automatic: false,
            volume,
            phase: Phase::Waiting,
            user_paused: false,
            progress: Progress::default(),
            message: "Waiting to start".into(),
        });
        Ok(id)
    }
    /// Puts a finished job back to `Waiting` under the same id, with its progress
    /// cleared. Fails if the game already has an active job.
    pub fn retry(&mut self, id: u64) -> Result<()> {
        let old = self
            .jobs
            .iter()
            .find(|job| job.id == id)
            .ok_or_else(|| anyhow::anyhow!("Job no longer exists"))?;
        ensure!(!old.phase.active(), "The job is already active");
        ensure!(
            self.jobs.iter().filter(|job| job.phase.active()).count() < 200,
            "The queue is full"
        );
        ensure!(
            !self
                .jobs
                .iter()
                .any(|job| job.phase.active() && job.game.install_dir == old.game.install_dir),
            "This game already has an active job"
        );
        let job = self
            .jobs
            .iter_mut()
            .find(|job| job.id == id)
            .ok_or_else(|| anyhow::anyhow!("Job no longer exists"))?;
        if job.volume.is_none() {
            job.volume = Some(crate::storage::volume(&job.game.install_dir)?);
        }
        job.phase = Phase::Waiting;
        job.user_paused = false;
        job.progress = Progress::default();
        job.message = "Waiting to retry".into();
        Ok(())
    }
    /// `enqueue` for a compression that maintenance asked for. Only a job this call
    /// creates is marked automatic. A job the user already queued keeps its owner.
    pub fn enqueue_automatic(&mut self, game: Game) -> Result<u64> {
        let existing = self
            .jobs
            .iter()
            .any(|job| job.phase.active() && job.game.install_dir == game.install_dir);
        let id = self.enqueue(game, false)?;
        if !existing && let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) {
            job.automatic = true;
        }
        Ok(id)
    }
    /// Records the game's current build as handled, so `observe` stops reporting it.
    pub fn acknowledge(&mut self, game: &Game) {
        for key in baseline_keys(game) {
            self.baseline.insert(key, (game.build.clone(), true));
        }
    }
    /// Records the build of a finished automatic compression as handled, so
    /// `observe` stops reporting it. A job that was stopped by a shutdown, or that
    /// is still waiting or running, leaves the game due. A failed job counts as
    /// handled, because the user can retry it and an automatic retry loop would not end.
    pub fn settle(&mut self, id: u64, shutting_down: bool) {
        let Some(job) = self.jobs.iter().find(|job| job.id == id) else {
            return;
        };
        let handled = match job.phase {
            Phase::Completed | Phase::Failed => true,
            Phase::Cancelled => !shutting_down,
            _ => false,
        };
        if job.automatic && !job.restore && handled {
            let game = job.game.clone();
            self.acknowledge(&game);
        }
    }
    /// Returns the games maintenance should compress now: under an automatic
    /// location, not excluded, idle, and either updated or newly installed. A due
    /// game keeps its old baseline until `acknowledge`, so it is reported again on
    /// the next scan. With `healthy` false nothing is returned and nothing recorded.
    pub fn observe(
        &mut self,
        games: &[Game],
        preferences: &Preferences,
        healthy: bool,
    ) -> Vec<Game> {
        let mut due = vec![];
        if !healthy {
            return due;
        }
        for game in games {
            let enabled =
                preferences.locations.iter().any(|location| {
                    location.automatic && game.install_dir.starts_with(&location.path)
                }) && !game
                    .ids()
                    .any(|id| preferences.excluded.contains(&id.to_string()));
            let keys = baseline_keys(game);
            // Updated: the build differs from a baseline taken while the game was
            // covered. A change made while it was not covered does not count.
            let previous = keys.iter().find_map(|key| self.baseline.get(key)).cloned();
            let changed = previous
                .as_ref()
                .is_some_and(|(build, was_enabled)| *was_enabled && *build != game.build);
            // The first healthy scan for each location establishes a baseline.
            let newly_installed = previous.is_none()
                && preferences.locations.iter().any(|location| {
                    location.automatic
                        && game.install_dir.starts_with(&location.path)
                        && self
                            .initialized_locations
                            .contains(&location_key(&location.path))
                });
            if enabled
                && game.state.is_idle()
                && !preferences.maintenance_paused
                && (changed || newly_installed)
            {
                due.push(game.clone());
                continue;
            }
            // Due but blocked by a pause or a busy game. Leave the baseline alone so
            // the game is still due on a later scan.
            if enabled && (changed || newly_installed) {
                continue;
            }
            for key in keys {
                self.baseline.insert(key, (game.build.clone(), enabled));
            }
        }
        self.initialized_locations = preferences
            .locations
            .iter()
            .filter(|location| location.automatic)
            .map(|location| location_key(&location.path))
            .collect();
        due
    }
}
/// The baseline keys of a game: one per id, and one for its install directory.
/// The directory key lets a folder that loses its launcher record, and comes back
/// under a manual id, keep its baseline.
fn baseline_keys(game: &Game) -> Vec<String> {
    let mut keys: Vec<String> = game.ids().map(ToString::to_string).collect();
    keys.push(format!("dir:{}", location_key(&game.install_dir)));
    keys
}
/// A fixed-length key for a location path, used in `initialized_locations`.
fn location_key(path: &Path) -> String {
    blake3::hash(path.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string()
}
/// Files that Windows reported would not shrink, keyed by path inside the game
/// folder, with the size and modification time seen then. A file that still has
/// both is skipped on the next pass. Losing the record only costs a recompression.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rejected {
    files: HashMap<String, (u64, u64)>,
}
impl Rejected {
    /// The record at `path`, or an empty one if it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        crate::desktop::read_bounded(path, 16 * 1024 * 1024)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
    /// Whether `relative` was rejected at exactly this size and modification time.
    pub fn contains(&self, relative: &str, length: u64, modified: u64) -> bool {
        self.files.get(relative) == Some(&(length, modified))
    }
    pub fn insert(&mut self, relative: String, length: u64, modified: u64) {
        self.files.insert(relative, (length, modified));
    }
    /// Adds every entry of `other`, replacing entries for the same path.
    pub fn merge(&mut self, other: Self) {
        self.files.extend(other.files);
    }
    /// Replaces the record atomically.
    pub fn save(&self, path: &Path) -> Result<()> {
        let directory = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Record path has no folder"))?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        desktop::{Location, LocationKind},
        testutil::{Ctx, TestResult, check, check_eq},
    };
    #[test]
    fn queue_survives_restart_and_maintenance_starts_with_a_baseline() -> TestResult {
        let temp = tempfile::tempdir().ctx("native queue fixture")?;
        let library = temp.path().join("Games");
        let path = library.join("One");
        std::fs::create_dir_all(&path).ctx("game")?;
        let mut game = crate::desktop::manual_game("One".into(), path);
        game.build = Some("1".into());
        let mut queue = Queue::default();
        let id = queue.enqueue(game.clone(), false).ctx("enqueue")?;
        check_eq(
            queue.enqueue(game.clone(), false).ctx("deduplicate")?,
            id,
            "one active job per game",
        )?;
        queue.jobs.first_mut().ctx("job")?.phase = Phase::Running;
        queue.save(temp.path()).ctx("save")?;
        let mut queue = Queue::load(temp.path()).ctx("restart")?;
        check_eq(
            queue.jobs.first().ctx("restarted job")?.phase,
            Phase::Interrupted,
            "running work requires recovery after restart",
        )?;
        let preferences = Preferences {
            locations: vec![Location {
                path: library,
                kind: LocationKind::Collection,
                automatic: true,
            }],
            ..Default::default()
        };
        check(
            queue
                .observe(&[game.clone()], &preferences, true)
                .is_empty(),
            "enabling maintenance does not compress existing games",
        )?;
        game.build = Some("2".into());
        check(
            queue
                .observe(&[game.clone()], &preferences, false)
                .is_empty(),
            "failed discovery cannot trigger maintenance",
        )?;
        check_eq(
            queue.observe(&[game.clone()], &preferences, true).len(),
            1,
            "updated build is queued",
        )?;
        queue.acknowledge(&game);
        check(
            queue.observe(&[game], &preferences, true).is_empty(),
            "unchanged build stays idle",
        )
    }
    #[test]
    fn batch_baselines_pause_and_opt_out_do_not_schedule_existing_games() -> TestResult {
        let temp = tempfile::tempdir().ctx("maintenance fixture")?;
        let library = temp.path().join("Games");
        let mut one = crate::desktop::manual_game("One".into(), library.join("One"));
        let two = crate::desktop::manual_game("Two".into(), library.join("Two"));
        let three = crate::desktop::manual_game("Three".into(), library.join("Three"));
        one.build = Some("1".into());
        let mut settings = Preferences {
            locations: vec![Location {
                path: library,
                kind: LocationKind::Collection,
                automatic: true,
            }],
            ..Default::default()
        };
        let mut queue = Queue::default();
        check(
            queue
                .observe(&[one.clone(), two.clone()], &settings, true)
                .is_empty(),
            "first batch establishes every game baseline",
        )?;
        one.build = Some("2".into());
        settings.maintenance_paused = true;
        let games = vec![one.clone(), two.clone(), three.clone()];
        check(
            queue.observe(&games, &settings, true).is_empty(),
            "paused maintenance queues nothing",
        )?;
        settings.maintenance_paused = false;
        check_eq(
            queue.observe(&games, &settings, true).len(),
            2,
            "updates and new installs survive a pause",
        )?;
        check_eq(
            queue.observe(&games, &settings, true).len(),
            2,
            "an unaccepted update remains due",
        )?;
        queue.acknowledge(&one);
        queue.acknowledge(&three);
        check(
            queue.observe(&games, &settings, true).is_empty(),
            "accepted builds stop being due",
        )?;
        settings.locations.first_mut().ctx("location")?.automatic = false;
        one.build = Some("3".into());
        check(
            queue.observe(&[one.clone()], &settings, true).is_empty(),
            "opted out updates stay idle",
        )?;
        settings.locations.first_mut().ctx("location")?.automatic = true;
        check(
            queue.observe(&[one.clone()], &settings, true).is_empty(),
            "reenabling uses current baseline",
        )?;
        settings.excluded.push(one.id.to_string());
        one.build = Some("4".into());
        check(
            queue.observe(&[one.clone()], &settings, true).is_empty(),
            "excluded games stay idle",
        )?;
        let manual = queue.enqueue(two.clone(), false).ctx("manual job")?;
        check_eq(
            queue.enqueue_automatic(two).ctx("existing automatic job")?,
            manual,
            "automatic discovery deduplicates manual work",
        )?;
        check(
            !queue.jobs.first().ctx("job")?.automatic,
            "existing manual job retains its ownership",
        )
    }
    #[test]
    fn empty_library_baseline_recognizes_the_first_new_install() -> TestResult {
        let temp = tempfile::tempdir().ctx("empty library fixture")?;
        let settings = Preferences {
            locations: vec![Location {
                path: temp.path().into(),
                kind: LocationKind::Collection,
                automatic: true,
            }],
            ..Default::default()
        };
        let mut queue = Queue::default();
        check(
            queue.observe(&[], &settings, true).is_empty(),
            "empty baseline",
        )?;
        let mut game = crate::desktop::manual_game("New".into(), temp.path().join("New"));
        game.state = crate::model::InstallState::Broken {
            detail: "Offline".into(),
        };
        check(
            queue.observe(&[game.clone()], &settings, true).is_empty(),
            "offline new install is deferred",
        )?;
        game.state = crate::model::InstallState::Idle;
        check_eq(
            queue.observe(&[game], &settings, true).len(),
            1,
            "reconnected new installation remains due",
        )
    }
    fn automatic_fixture() -> (Preferences, Game, Queue) {
        let library = std::path::PathBuf::from("/games");
        let mut game = crate::desktop::manual_game("One".into(), library.join("One"));
        game.build = Some("1".into());
        let preferences = Preferences {
            locations: vec![Location {
                path: library,
                kind: LocationKind::Collection,
                automatic: true,
            }],
            ..Default::default()
        };
        let mut queue = Queue::default();
        queue.observe(&[game.clone()], &preferences, true);
        game.build = Some("2".into());
        (preferences, game, queue)
    }
    fn queue_job(queue: &mut Queue, game: &Game, phase: Phase) -> TestResult {
        // Not `enqueue`: the fixture folder does not exist, so no volume can be read.
        queue.next_id += 1;
        queue.jobs.push(Job {
            id: queue.next_id,
            game: game.clone(),
            restore: false,
            automatic: true,
            volume: None,
            phase,
            user_paused: false,
            progress: Progress::default(),
            message: String::new(),
        });
        Ok(())
    }
    #[test]
    fn only_a_finished_or_user_stopped_job_settles_the_update() -> TestResult {
        let (preferences, game, mut queue) = automatic_fixture();
        check_eq(
            queue
                .observe(std::slice::from_ref(&game), &preferences, true)
                .len(),
            1,
            "control: the updated build is due",
        )?;
        queue_job(&mut queue, &game, Phase::Waiting)?;
        queue.settle(queue.next_id, false);
        check_eq(
            queue
                .observe(std::slice::from_ref(&game), &preferences, true)
                .len(),
            1,
            "a waiting job leaves the update due",
        )?;
        queue.jobs.last_mut().ctx("job")?.phase = Phase::Cancelled;
        queue.settle(queue.next_id, true);
        check_eq(
            queue
                .observe(std::slice::from_ref(&game), &preferences, true)
                .len(),
            1,
            "a job stopped by a shutdown leaves the update due",
        )?;
        queue.settle(queue.next_id, false);
        check(
            queue
                .observe(std::slice::from_ref(&game), &preferences, true)
                .is_empty(),
            "a job the user cancelled settles the update",
        )?;
        let (preferences, game, mut queue) = automatic_fixture();
        queue_job(&mut queue, &game, Phase::Completed)?;
        queue.settle(queue.next_id, false);
        check(
            queue.observe(&[game], &preferences, true).is_empty(),
            "a completed job settles the update",
        )
    }
    #[test]
    fn a_game_that_changes_launcher_id_keeps_its_baseline() -> TestResult {
        let (preferences, game, mut queue) = automatic_fixture();
        // The same folder, now known only by a different id.
        let mut renamed = game.clone();
        renamed.id = crate::model::GameId::new(crate::model::Launcher::Steam, "1234");
        renamed.build = Some("1".into());
        check(
            queue.observe(&[renamed], &preferences, true).is_empty(),
            "a known folder under a new id is not a new install",
        )?;
        let fresh = crate::desktop::manual_game("Two".into(), "/games/Two".into());
        check_eq(
            queue.observe(&[fresh], &preferences, true).len(),
            1,
            "control: an unknown folder is a new install",
        )
    }
    #[test]
    fn an_unparseable_queue_is_set_aside_and_an_unreadable_one_is_not() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let path = temp.path().join("native-queue.json");
        std::fs::write(&path, b"{ not json").ctx("write")?;
        check(Queue::load(temp.path()).is_err(), "control: load fails")?;
        let (queue, note) = Queue::load_or_quarantine(temp.path()).ctx("quarantine")?;
        check(queue.jobs.is_empty(), "starts empty")?;
        check(note.is_some(), "the note says where the file went")?;
        check(
            temp.path().join("native-queue.json.corrupt").exists(),
            "the old file is kept",
        )?;
        check(!path.exists(), "the name is free for a new queue")?;
        let (_, note) = Queue::load_or_quarantine(temp.path()).ctx("second load")?;
        check(
            note.is_none(),
            "a missing file is an empty queue, not damage",
        )
    }
    #[test]
    fn rejected_files_match_only_at_the_same_size_and_time() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let path = temp.path().join("rejected.json");
        let mut record = Rejected::default();
        record.insert("a.pak".into(), 100, 7);
        record.save(&path).ctx("save")?;
        let record = Rejected::load(&path);
        check(record.contains("a.pak", 100, 7), "same size and time")?;
        check(!record.contains("a.pak", 101, 7), "size changed")?;
        check(!record.contains("a.pak", 100, 8), "time changed")?;
        check(!record.contains("b.pak", 100, 7), "other path")?;
        std::fs::write(&path, b"garbage").ctx("damage")?;
        check_eq(
            Rejected::load(&path),
            Rejected::default(),
            "an unreadable record is empty",
        )
    }
    #[test]
    fn retry_and_restore_cannot_duplicate_or_replace_active_compression() -> TestResult {
        let temp = tempfile::tempdir().ctx("retry fixture")?;
        let game = crate::desktop::manual_game("Game".into(), temp.path().join("Game"));
        let mut queue = Queue::default();
        let first = queue.enqueue(game.clone(), false).ctx("original job")?;
        check(
            queue.enqueue(game.clone(), true).is_err(),
            "restore reports an active compression conflict",
        )?;
        queue.jobs.first_mut().ctx("first")?.phase = Phase::Failed;
        queue.enqueue(game, false).ctx("replacement job")?;
        check(
            queue.retry(first).is_err(),
            "retry cannot create a second active job for a game",
        )?;
        queue.jobs.last_mut().ctx("second")?.phase = Phase::Cancelled;
        queue.retry(first).ctx("retry after cancellation")?;
        check_eq(
            queue.jobs.first().ctx("retry")?.phase,
            Phase::Waiting,
            "retry retains its persistent ID",
        )
    }
}
