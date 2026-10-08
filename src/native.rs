//! Library preferences shared by native Mac and Windows front ends.
use crate::{libraries, model::Game};
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::Result;
use std::path::{Path, PathBuf};

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

pub fn add_folder(path: &Path) -> Result<()> {
    let root = libraries::data_dir()?;
    let mut preferences = crate::desktop::Preferences::load(&root)?;
    preferences.add(path, crate::desktop::LocationKind::Game)?;
    preferences.save(&root)
}
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
    let preferences = crate::desktop::Preferences::load(&root)?;
    let (custom, warnings) = preferences.custom_games();
    catalog.games.extend(custom);
    catalog.warnings.extend(warnings);
    catalog = catalog.finish();
    catalog.games = libraries::remember(&root, catalog.games, |game| preferences.keeps(game))?;
    Ok(catalog)
}
pub fn discover() -> Result<Vec<Game>> {
    Ok(discover_catalog()?.games)
}

/// Opens a platform picker without interpolating paths into scripts.
pub fn pick(artwork: bool) -> Result<Option<PathBuf>> {
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
        if error.contains("(-128)") {
            return Ok(None);
        }
        anyhow::bail!("The file picker could not open: {error}");
    }
    let text = String::from_utf8(output.stdout)?;
    let text = text.trim_end_matches(['\r', '\n']);
    Ok((!text.is_empty()).then(|| PathBuf::from(text)))
}
