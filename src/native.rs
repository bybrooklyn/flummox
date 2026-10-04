//! Library preferences shared by native Mac and Windows front ends.
#[cfg(target_os = "macos")]
use crate::macos as backend;
#[cfg(windows)]
use crate::windows as backend;
use crate::{
    libraries,
    model::{Game, GameId, InstallState, Launcher},
};
use anyhow::{Context, Result, ensure};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

fn folders() -> Result<Vec<PathBuf>> {
    let path = libraries::data_dir()?.join("folders.json");
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Reading custom folders"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(error) => Err(error.into()),
    }
}
pub fn add_folder(path: &Path) -> Result<()> {
    let path = path.canonicalize()?;
    ensure!(
        path.is_dir() && path.parent().is_some(),
        "Choose an installed game folder"
    );
    let mut folders = folders()?;
    if !folders.contains(&path) {
        folders.push(path);
    }
    let root = libraries::data_dir()?;
    libraries::private_dir(&root)?;
    let mut file = tempfile::NamedTempFile::new_in(&root)?;
    serde_json::to_writer(&mut file, &folders)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(root.join("folders.json"))?;
    Ok(())
}

pub fn discover() -> Result<Vec<Game>> {
    let mut games: Vec<_> = backend::discover_steam()
        .into_iter()
        .map(|game| {
            let mut model = game_model(game.title, game.path, Launcher::Steam);
            if let Some(id) = game.app_id {
                model.id.key = id.to_string();
            }
            model.build = game.build;
            model
        })
        .collect();
    for path in folders()? {
        if !path.is_dir() {
            continue;
        }
        let title = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Custom game".into());
        games.push(game_model(title, path, Launcher::Manual));
    }
    libraries::remember(&libraries::data_dir()?, games, |_| true)
}

fn game_model(title: String, path: PathBuf, launcher: Launcher) -> Game {
    Game {
        id: GameId::new(
            launcher,
            blake3::hash(path.as_os_str().as_encoded_bytes())
                .to_hex()
                .to_string(),
        ),
        also: vec![],
        title,
        install_dir: path,
        build: None,
        size_hint: None,
        state: InstallState::Idle,
        is_tool: false,
    }
}
