//! Library preferences shared by native Mac and Windows front ends.
use crate::{libraries, model::Game};
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Turns typed or pasted text into a path: trims whitespace and quotes, turns a
/// backslash-escaped space into a space, and expands a leading `~/`. It does not
/// touch the filesystem, so the result may not exist.
pub fn folder_path(input: &str) -> PathBuf {
    let text = input.trim().trim_matches(['\"', '\'']);
    let text = text.replace("\\ ", " ");
    if let Some(relative) = text.strip_prefix("~/")
        && let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
    {
        return PathBuf::from(home).join(relative);
    }
    PathBuf::from(text)
}

/// Adds an existing folder to the saved locations as one game.
pub fn add_folder(path: &Path) -> Result<()> {
    let root = libraries::data_dir()?;
    let mut preferences = crate::desktop::Preferences::load(&root)?;
    preferences.add(path, crate::desktop::LocationKind::Game)?;
    preferences.save(&root)
}
/// Launcher discovery plus the user's own locations, merged by install directory,
/// then passed through the remembered-library cache. The cache keeps a game on a
/// disconnected drive listed as broken. Reads and rewrites files in the data directory.
pub fn discover_catalog() -> Result<crate::desktop_discovery::Catalog> {
    #[cfg(windows)]
    let mut catalog = crate::windows::launchers::discover();
    #[cfg(target_os = "macos")]
    let mut catalog = {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let mut catalog = crate::desktop_discovery::Catalog::default();
        catalog.steam(vec![home.join("Library/Application Support/Steam")]);
        catalog.heroic(&home.join("Library/Application Support/heroic"));
        catalog
    };
    let root = libraries::data_dir()?;
    // A preferences file or cache that cannot be used costs the custom folders or
    // the remembered offline games, never the whole catalog.
    let preferences = match crate::desktop::Preferences::load(&root) {
        Ok(preferences) => Some(preferences),
        Err(error) => {
            catalog
                .warnings
                .push(format!("Reading saved locations: {error}"));
            None
        }
    };
    if let Some(preferences) = &preferences {
        let (custom, warnings) = preferences.custom_games();
        catalog.games.extend(custom);
        catalog.warnings.extend(warnings);
    }
    catalog = catalog.finish();
    let keep = |game: &Game| preferences.as_ref().is_none_or(|p| p.keeps(game));
    match libraries::remember(&root, catalog.games.clone(), keep) {
        Ok(games) => catalog.games = games,
        Err(error) => catalog
            .warnings
            .push(format!("Remembering offline libraries: {error}")),
    }
    Ok(catalog)
}
/// The games of `discover_catalog`, without its warnings or artwork roots.
pub fn discover() -> Result<Vec<Game>> {
    Ok(discover_catalog()?.games)
}

/// Opens a platform picker without interpolating paths into scripts.
/// `artwork` picks a PNG or JPEG file. Otherwise it picks a folder. Blocks until the
/// dialog closes and returns `Ok(None)` when the user cancels.
pub fn pick(artwork: bool) -> Result<Option<PathBuf>> {
    // The dialog runs in PowerShell. -STA is required because Windows Forms dialogs
    // need a single-threaded apartment. Console output is switched to UTF-8 without
    // a byte order mark so a non-ASCII path arrives intact. A cancelled dialog
    // prints nothing.
    #[cfg(windows)]
    let output = {
        let picker = if artwork {
            "$d=New-Object System.Windows.Forms.OpenFileDialog; $d.Filter='Images|*.png;*.jpg;*.jpeg'; if($d.ShowDialog() -eq 'OK'){[Console]::WriteLine($d.FileName)}"
        } else {
            "$d=New-Object System.Windows.Forms.FolderBrowserDialog; if($d.ShowDialog() -eq 'OK'){[Console]::WriteLine($d.SelectedPath)}"
        };
        std::process::Command::new("powershell.exe").args(["-NoProfile", "-STA", "-Command"]).arg(format!("[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); Add-Type -AssemblyName System.Windows.Forms; {picker}")).output()?
    };
    #[cfg(target_os = "macos")]
    let output = std::process::Command::new("osascript")
        .args([
            "-e",
            if artwork {
                "POSIX path of (choose file of type {\"public.png\", \"public.jpeg\"})"
            } else {
                "POSIX path of (choose folder)"
            },
        ])
        .output()?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        // osascript reports a cancelled dialog as error -128.
        if error.contains("(-128)") {
            return Ok(None);
        }
        anyhow::bail!("The file picker could not open: {error}");
    }
    let text = String::from_utf8(output.stdout)?;
    let text = text.trim_end_matches(['\r', '\n']);
    Ok((!text.is_empty()).then(|| PathBuf::from(text)))
}
