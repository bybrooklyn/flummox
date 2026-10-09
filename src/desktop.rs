//! Native desktop preferences and custom locations, with legacy folder migration.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    #[default]
    System,
    Dark,
    Light,
}
/// How far and how long a page moves when it appears. `Reduced` removes the movement.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MotionChoice {
    #[default]
    Normal,
    Subtle,
    Reduced,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocationKind {
    /// The path is one game's folder.
    #[default]
    Game,
    /// Each immediate subdirectory of the path is a game.
    Collection,
}
// Implements `Display` with the labels the pick lists show.
macro_rules! display_choices {
    ($name:ident, $( $variant:ident => $label:literal ),+) => {
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(match self { $( Self::$variant => $label ),+ })
            }
        }
    };
}
display_choices!(ThemeChoice, System => "System", Dark => "Dark", Light => "Light");
display_choices!(MotionChoice, Normal => "Smooth", Subtle => "Subtle", Reduced => "Reduced");
display_choices!(LocationKind, Game => "Single game", Collection => "Games library");
impl MotionChoice {
    /// Length of the page transition.
    pub fn duration(self) -> std::time::Duration {
        std::time::Duration::from_millis(match self {
            Self::Normal => 180,
            Self::Subtle => 120,
            Self::Reduced => 0,
        })
    }
    /// Offset the incoming page starts from.
    pub fn distance(self) -> f32 {
        match self {
            Self::Normal => 12.0,
            Self::Subtle => 6.0,
            Self::Reduced => 0.0,
        }
    }
}
/// A folder the user added: one game, or a library of games.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    #[serde(with = "crate::path_serde")]
    pub path: PathBuf,
    #[serde(default)]
    pub kind: LocationKind,
    /// Background maintenance may queue games under this path. Only the Windows
    /// worker acts on it.
    #[serde(default)]
    pub automatic: bool,
}
/// Contents of `desktop.json`. Every field defaults, so an older file still loads.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub theme: ThemeChoice,
    pub motion: MotionChoice,
    pub locations: Vec<Location>,
    /// Game ids, as text, that get no compression jobs. Restoring them is still allowed.
    pub excluded: Vec<String>,
    pub start_at_login: bool,
    /// Holds every job while true. On Windows the worker owns this value, and a
    /// Settings command from the window does not change it.
    pub maintenance_paused: bool,
}
/// Reads a whole file, failing if it holds more than `limit` bytes. It reads one byte
/// past the limit, so the check does not depend on the size the filesystem reports.
pub fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = vec![];
    std::fs::File::open(path)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "{} exceeds its size limit",
        path.display()
    );
    Ok(bytes)
}
/// True when the error is an I/O "not found", which callers treat as "no file yet".
fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}
/// Renames an unreadable state file to `<name>.corrupt` (then `.corrupt.1`, and so on)
/// so a fresh one can be written. Returns the new path. The contents are kept.
pub fn quarantine(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .context("State file has no name")?
        .to_string_lossy()
        .into_owned();
    for attempt in 0..100u32 {
        let suffix = if attempt == 0 {
            ".corrupt".to_owned()
        } else {
            format!(".corrupt.{attempt}")
        };
        let target = path.with_file_name(format!("{name}{suffix}"));
        if !target.exists() {
            std::fs::rename(path, &target)?;
            return Ok(target);
        }
    }
    anyhow::bail!("Too many quarantined copies of {}", path.display())
}

/// Folders that must never be compressed whole, and the folders that contain them.
#[derive(Debug, Default, Clone)]
pub struct ProtectedFolders {
    /// A game folder may not be one of these or hold one: the system root,
    /// the user profile and the Program Files and ProgramData roots.
    pub roots: Vec<PathBuf>,
    /// A game folder may not be inside one of these: the system root.
    pub trees: Vec<PathBuf>,
}

impl ProtectedFolders {
    /// Reads the Windows locations from the environment, canonical where they
    /// exist. Empty on a system that sets none of these variables.
    pub fn from_environment() -> Self {
        let find = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .and_then(|path| path.canonicalize().ok())
        };
        let trees: Vec<PathBuf> = ["SystemRoot", "windir"]
            .iter()
            .filter_map(|name| find(name))
            .collect();
        let mut roots = trees.clone();
        roots.extend(
            [
                "USERPROFILE",
                "ProgramFiles",
                "ProgramFiles(x86)",
                "ProgramW6432",
                "ProgramData",
            ]
            .iter()
            .filter_map(|name| find(name)),
        );
        Self { roots, trees }
    }

    /// Fails for a filesystem root, for a protected folder or one of its
    /// ancestors, and for anything inside a protected tree. Expects canonical paths.
    pub fn check(&self, path: &Path) -> Result<()> {
        ensure!(
            path.parent().is_some(),
            "Choose a game folder, not a drive."
        );
        ensure!(
            !self.roots.iter().any(|root| root.starts_with(path)),
            "{} is a system or profile folder.",
            path.display()
        );
        ensure!(
            !self.trees.iter().any(|tree| path.starts_with(tree)),
            "{} is inside the Windows folder.",
            path.display()
        );
        Ok(())
    }
}

impl Preferences {
    /// Loads `desktop.json` from `root`. Without one it migrates the older
    /// `folders.json` list of game folders, and with neither it returns the defaults.
    /// A malformed or oversized file is an error.
    pub fn load(root: &Path) -> Result<Self> {
        match read_bounded(&root.join("desktop.json"), 1024 * 1024) {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes).context("Reading desktop preferences");
            }
            Err(error) if missing(&error) => {}
            Err(error) => return Err(error),
        }
        let bytes = match read_bounded(&root.join("folders.json"), 1024 * 1024) {
            Ok(bytes) => bytes,
            Err(error) if missing(&error) => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let folders: Vec<PathBuf> =
            serde_json::from_slice(&bytes).context("Reading remembered game folders")?;
        let mut settings = Self::default();
        for path in folders {
            if !settings
                .locations
                .iter()
                .any(|location| location.path == path)
            {
                settings.locations.push(Location {
                    path,
                    kind: LocationKind::Game,
                    automatic: false,
                });
            }
        }
        Ok(settings)
    }
    /// Replaces `desktop.json` atomically: temp file, fsync, rename, directory fsync.
    /// Refuses to write more than `load` would read back.
    pub fn save(&self, root: &Path) -> Result<()> {
        crate::libraries::private_dir(root)?;
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "Settings are too large to save."
        );
        let mut staged = tempfile::NamedTempFile::new_in(root)?;
        staged.write_all(&bytes)?;
        staged.as_file().sync_all()?;
        staged.persist(root.join("desktop.json"))?;
        #[cfg(unix)]
        std::fs::File::open(root)?.sync_all()?;
        Ok(())
    }
    /// Adds a location, or changes the kind of one already listed. The path must
    /// exist and is stored canonical. A filesystem root is refused.
    pub fn add(&mut self, path: &Path, kind: LocationKind) -> Result<()> {
        let path = path.canonicalize()?;
        ensure!(path.is_dir(), "Choose an existing game or games library.");
        ProtectedFolders::from_environment().check(&path)?;
        if let Some(old) = self
            .locations
            .iter_mut()
            .find(|location| location.path == path)
        {
            old.kind = kind;
        } else {
            self.locations.push(Location {
                path,
                kind,
                automatic: false,
            });
        }
        Ok(())
    }
    /// Removes a location from the list. Nothing on disk is deleted. Paths are
    /// compared resolved, so a location on a disconnected drive can still be removed.
    pub fn remove(&mut self, path: &Path) {
        let resolved = resolved_path(path);
        self.locations
            .retain(|location| resolved_path(&location.path) != resolved);
    }
    /// The games under the user's locations, plus one warning per path that could
    /// not be read. Each game's build is a metadata stamp of its files, so a change
    /// on disk shows up as a new build.
    pub fn custom_games(&self) -> (Vec<crate::model::Game>, Vec<String>) {
        let mut games = vec![];
        let mut warnings = vec![];
        let protected = ProtectedFolders::from_environment();
        for location in &self.locations {
            let paths = if location.kind == LocationKind::Game {
                vec![location.path.clone()]
            } else {
                match std::fs::read_dir(&location.path) {
                    Ok(entries) => {
                        // `DirEntry::file_type` does not follow links, so a linked
                        // folder is left out. Sorted so the order is stable.
                        let mut paths: Vec<_> = entries
                            .filter_map(|entry| entry.ok())
                            .filter(|entry| {
                                entry
                                    .file_type()
                                    .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
                            })
                            .map(|entry| entry.path())
                            .collect();
                        paths.sort();
                        paths
                    }
                    Err(error) => {
                        warnings.push(format!("{}: {error}", location.path.display()));
                        continue;
                    }
                }
            };
            for path in paths {
                if !path.is_dir() {
                    warnings.push(format!(
                        "{} is not available. Check that its drive is connected.",
                        path.display()
                    ));
                    continue;
                }
                let title = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Custom game".into());
                let mut game = manual_game(title, path);
                // Listed but not startable, so a library placed over a drive root
                // cannot offer the Windows folder as a game.
                if let Ok(resolved) = game.install_dir.canonicalize()
                    && protected.check(&resolved).is_err()
                {
                    game.state = crate::model::InstallState::Broken {
                        detail: "System folders cannot be compressed.".into(),
                    };
                    games.push(game);
                    continue;
                }
                match content_stamp(&game.install_dir) {
                    Ok(stamp) => game.build = Some(format!("local:{stamp}")),
                    // Includes a tree too large to stamp within its limits. The game
                    // stays listed and cannot start a job. No warning is added, since
                    // a warning would hold maintenance for every other game too.
                    Err(error) => {
                        game.state = crate::model::InstallState::Broken {
                            detail: format!("Game files could not be inspected: {error}"),
                        };
                    }
                }
                games.push(game);
            }
        }
        (games, warnings)
    }
    /// Whether a cached game should stay remembered. Launcher games always do. A
    /// manual game does only while a location still covers its folder.
    pub fn keeps(&self, game: &crate::model::Game) -> bool {
        game.id.launcher != crate::model::Launcher::Manual
            || self.locations.iter().any(|location| {
                let path = resolved_path(&location.path);
                let game_path = resolved_path(&game.install_dir);
                match location.kind {
                    LocationKind::Game => game_path == path,
                    LocationKind::Collection => game_path.parent() == Some(path.as_path()),
                }
            })
    }
}
/// Resolves existing ancestors so offline paths retain their directory aliases.
fn resolved_path(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|ancestor| ancestor.exists())
        .and_then(|ancestor| {
            Some(
                ancestor
                    .canonicalize()
                    .ok()?
                    .join(path.strip_prefix(ancestor).ok()?),
            )
        })
        .unwrap_or_else(|| path.to_path_buf())
}
/// A game with no launcher. Its id is a hash of the path bytes as given, so the same
/// spelling of a path always yields the same id and a different spelling does not.
pub fn manual_game(title: String, path: PathBuf) -> crate::model::Game {
    crate::model::Game {
        id: crate::model::GameId::new(
            crate::model::Launcher::Manual,
            blake3::hash(path.as_os_str().as_encoded_bytes())
                .to_hex()
                .to_string(),
        ),
        also: vec![],
        title,
        install_dir: path,
        build: None,
        size_hint: None,
        state: crate::model::InstallState::Idle,
        is_tool: false,
    }
}
/// Collapses games that share a canonical install directory into the first one seen.
/// It collects the other ids in `also`, and a later non-idle state replaces its own.
pub fn merge(games: Vec<crate::model::Game>) -> Vec<crate::model::Game> {
    let mut merged: Vec<crate::model::Game> = vec![];
    for mut game in games {
        if let Ok(path) = game.install_dir.canonicalize() {
            game.install_dir = path;
        }
        if let Some(previous) = merged
            .iter_mut()
            .find(|old| crate::model::same_install_dir(&old.install_dir, &game.install_dir))
        {
            for id in game.ids() {
                if !previous.ids().any(|old| old == id) {
                    previous.also.push(id.clone());
                }
            }
            if !game.state.is_idle() {
                previous.state = game.state;
            }
        } else {
            merged.push(game);
        }
    }
    merged
}
/// Tracks custom-game file changes without reading file contents or following links.
pub fn content_stamp(root: &Path) -> Result<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut fingerprint = blake3::Hasher::new();
    for (count, entry) in walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .enumerate()
    {
        // Bounded at 250000 entries and 5 seconds. Past either the stamp is an error.
        ensure!(
            count < 250000 && std::time::Instant::now() < deadline,
            "This folder has too many files to check. Choose the game's own folder."
        );
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let metadata = entry.metadata()?;
        let name = entry
            .path()
            .strip_prefix(root)?
            .as_os_str()
            .as_encoded_bytes();
        // Per file: relative name, size and mtime. The name's length is hashed first
        // so one file's fields cannot be read as part of the next file's name.
        fingerprint.update(&(name.len() as u64).to_le_bytes());
        fingerprint.update(name);
        fingerprint.update(&metadata.len().to_le_bytes());
        // A missing or pre-1970 timestamp hashes as zero, so one odd file does not
        // make the whole game unstampable.
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_nanos());
        fingerprint.update(&modified.to_le_bytes());
    }
    Ok(fingerprint.finalize().to_hex().to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    #[test]
    fn legacy_folders_migrate_and_collections_remove_without_deleting() -> TestResult {
        let temp = tempfile::tempdir().ctx("preferences fixture")?;
        let state = temp.path().join("state");
        std::fs::create_dir(&state).ctx("state")?;
        let collection = temp.path().join("My Games");
        let game = collection.join("First Game");
        std::fs::create_dir_all(&game).ctx("game")?;
        std::fs::write(
            state.join("folders.json"),
            serde_json::to_vec(&vec![game.clone()]).ctx("legacy JSON")?,
        )
        .ctx("legacy folders")?;
        let mut settings = Preferences::load(&state).ctx("migration")?;
        let before = settings
            .custom_games()
            .0
            .first()
            .ctx("legacy game")?
            .id
            .clone();
        settings
            .add(&collection, LocationKind::Collection)
            .ctx("collection")?;
        settings.theme = ThemeChoice::Light;
        settings.motion = MotionChoice::Reduced;
        settings.save(&state).ctx("save")?;
        let mut settings = Preferences::load(&state).ctx("reload")?;
        check_eq(settings.motion, MotionChoice::Reduced, "motion persists")?;
        let games = merge(settings.custom_games().0);
        check_eq(games.len(), 1, "overlapping collection deduplicates")?;
        check_eq(
            &games.first().ctx("merged game")?.id,
            &before,
            "legacy game identity survives",
        )?;
        settings.remove(&game);
        settings.remove(&collection);
        check(
            !settings.keeps(games.first().ctx("removed game")?),
            "removed locations leave discovery cache",
        )?;
        check(game.is_dir(), "removing a location leaves games intact")
    }
    #[test]
    fn preferences_reject_oversized_and_malformed_files() -> TestResult {
        let temp = tempfile::tempdir().ctx("preferences fixture")?;
        std::fs::write(temp.path().join("desktop.json"), b"not json").ctx("malformed")?;
        check(
            Preferences::load(temp.path()).is_err(),
            "malformed preferences reported",
        )?;
        std::fs::write(
            temp.path().join("desktop.json"),
            vec![b' '; 1024 * 1024 + 1],
        )
        .ctx("oversized")?;
        check(
            Preferences::load(temp.path()).is_err(),
            "oversized preferences reported",
        )
    }
    #[test]
    fn custom_game_stamps_detect_updates_and_new_files() -> TestResult {
        let temp = tempfile::tempdir().ctx("custom game stamp fixture")?;
        let file = temp.path().join("payload.dat");
        std::fs::write(&file, b"original").ctx("original payload")?;
        let first = content_stamp(temp.path()).ctx("original stamp")?;
        check_eq(
            content_stamp(temp.path()).ctx("unchanged stamp")?,
            first.clone(),
            "unchanged metadata stays stable",
        )?;
        std::fs::write(&file, b"updated and longer").ctx("update")?;
        let second = content_stamp(temp.path()).ctx("updated stamp")?;
        check(first != second, "modified files change the stamp")?;
        std::fs::write(temp.path().join("new.dat"), b"new").ctx("new file")?;
        check(
            second != content_stamp(temp.path()).ctx("new file stamp")?,
            "new files change the stamp",
        )
    }
    fn protected() -> ProtectedFolders {
        ProtectedFolders {
            roots: vec![
                "/c/Windows".into(),
                "/c/Users/me".into(),
                "/c/Program Files".into(),
            ],
            trees: vec!["/c/Windows".into()],
        }
    }
    #[test]
    fn system_profile_and_program_roots_and_their_ancestors_are_refused() -> TestResult {
        for refused in [
            "/",
            "/c",
            "/c/Users",
            "/c/Users/me",
            "/c/Windows",
            "/c/Windows/System32",
            "/c/Program Files",
        ] {
            check(
                protected().check(Path::new(refused)).is_err(),
                format!("{refused} must be refused"),
            )?;
        }
        // Control: ordinary game folders pass the same check.
        for allowed in [
            "/c/Program Files/Some Game",
            "/c/Users/me/Games",
            "/d/Games/One",
        ] {
            protected()
                .check(Path::new(allowed))
                .ctx(format!("{allowed} must be accepted"))?;
        }
        Ok(())
    }
    #[test]
    fn unreadable_state_is_renamed_aside_and_never_overwritten() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let file = temp.path().join("native-queue.json");
        std::fs::write(&file, b"first").ctx("write")?;
        let first = quarantine(&file).ctx("first quarantine")?;
        std::fs::write(&file, b"second").ctx("rewrite")?;
        let second = quarantine(&file).ctx("second quarantine")?;
        check(!file.exists(), "the original name is free again")?;
        check_eq(
            std::fs::read(&first).ctx("first copy")?,
            b"first".to_vec(),
            "first copy kept",
        )?;
        check_eq(
            std::fs::read(&second).ctx("second copy")?,
            b"second".to_vec(),
            "second copy kept",
        )
    }
    #[test]
    fn a_file_with_a_timestamp_before_1970_does_not_stop_stamping() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let path = temp.path().join("old.dat");
        std::fs::write(&path, b"data").ctx("write")?;
        let before_epoch = std::time::UNIX_EPOCH - std::time::Duration::from_secs(86_400);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .ctx("open")?
            .set_modified(before_epoch)
            .ctx("set mtime before 1970")?;
        content_stamp(temp.path()).ctx("stamp with an old file")?;
        Ok(())
    }
    #[test]
    fn a_location_cannot_be_a_filesystem_root() -> TestResult {
        let mut settings = Preferences::default();
        check(
            settings
                .add(Path::new("/"), LocationKind::Collection)
                .is_err(),
            "the drive root is refused",
        )?;
        check(settings.locations.is_empty(), "and nothing is stored")?;
        let fixture = tempfile::tempdir().ctx("fixture")?;
        settings
            .add(fixture.path(), LocationKind::Collection)
            .ctx("control: an ordinary folder")?;
        check_eq(settings.locations.len(), 1, "is stored")
    }
}
