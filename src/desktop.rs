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
    #[default]
    Game,
    Collection,
}
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
display_choices!(LocationKind, Game => "One game", Collection => "Games library");
impl MotionChoice {
    pub fn duration(self) -> std::time::Duration {
        std::time::Duration::from_millis(match self {
            Self::Normal => 180,
            Self::Subtle => 120,
            Self::Reduced => 0,
        })
    }
    pub fn distance(self) -> f32 {
        match self {
            Self::Normal => 12.0,
            Self::Subtle => 6.0,
            Self::Reduced => 0.0,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    #[serde(with = "crate::path_serde")]
    pub path: PathBuf,
    #[serde(default)]
    pub kind: LocationKind,
    #[serde(default)]
    pub automatic: bool,
}
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub theme: ThemeChoice,
    pub motion: MotionChoice,
    pub locations: Vec<Location>,
    pub excluded: Vec<String>,
    pub start_at_login: bool,
    pub maintenance_paused: bool,
}
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
fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}
impl Preferences {
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
    pub fn save(&self, root: &Path) -> Result<()> {
        crate::libraries::private_dir(root)?;
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "Desktop preferences exceed 1 MiB"
        );
        let mut staged = tempfile::NamedTempFile::new_in(root)?;
        staged.write_all(&bytes)?;
        staged.as_file().sync_all()?;
        staged.persist(root.join("desktop.json"))?;
        #[cfg(unix)]
        std::fs::File::open(root)?.sync_all()?;
        Ok(())
    }
    pub fn add(&mut self, path: &Path, kind: LocationKind) -> Result<()> {
        let path = path.canonicalize()?;
        ensure!(
            path.is_dir() && path.parent().is_some(),
            "Choose an existing game or games library"
        );
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
    pub fn remove(&mut self, path: &Path) {
        let resolved = resolved_path(path);
        self.locations
            .retain(|location| resolved_path(&location.path) != resolved);
    }
    pub fn custom_games(&self) -> (Vec<crate::model::Game>, Vec<String>) {
        let mut games = vec![];
        let mut warnings = vec![];
        for location in &self.locations {
            let paths = if location.kind == LocationKind::Game {
                vec![location.path.clone()]
            } else {
                match std::fs::read_dir(&location.path) {
                    Ok(entries) => {
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
                    warnings.push(format!("{} is unavailable", path.display()));
                    continue;
                }
                let title = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Custom game".into());
                let mut game = manual_game(title, path);
                match content_stamp(&game.install_dir) {
                    Ok(stamp) => game.build = Some(format!("local:{stamp}")),
                    Err(error) => {
                        warnings.push(format!("{}: {error}", game.install_dir.display()));
                        game.state = crate::model::InstallState::Broken {
                            detail: "Game files could not be inspected".into(),
                        };
                    }
                }
                games.push(game);
            }
        }
        (games, warnings)
    }
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
pub fn merge(games: Vec<crate::model::Game>) -> Vec<crate::model::Game> {
    let mut merged: Vec<crate::model::Game> = vec![];
    for mut game in games {
        if let Ok(path) = game.install_dir.canonicalize() {
            game.install_dir = path;
        }
        if let Some(previous) = merged
            .iter_mut()
            .find(|old| old.install_dir == game.install_dir)
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
        ensure!(
            count < 250000 && std::time::Instant::now() < deadline,
            "Game metadata scan exceeds its limit"
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
        fingerprint.update(&(name.len() as u64).to_le_bytes());
        fingerprint.update(name);
        fingerprint.update(&metadata.len().to_le_bytes());
        fingerprint.update(
            &metadata
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
                .to_le_bytes(),
        );
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
}
