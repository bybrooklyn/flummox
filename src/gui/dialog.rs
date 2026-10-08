//! Desktop folder and report selection through installed native dialogs.

use std::{path::PathBuf, process::Command};

/// What the picker is choosing. This sets the title, the file filter and
/// whether a folder or a file is expected.
#[derive(Debug, Clone)]
pub enum Target {
    /// A folder of games to add as a location.
    Game,
    /// The folder for a Maximum Space store. Holds the game's id string.
    Storage(String),
    /// A compatibility report, as a JSON file.
    Report,
    /// An image for a game. Holds the game's id string.
    Artwork(String),
}

/// Turns a dialog's exit code and stdout into a path.
///
/// Exit code 1 is the user cancelling and gives `Ok(None)`. The bytes are
/// kept as they are apart from one trailing newline, so names that are not
/// UTF-8 or that end in spaces survive.
fn selected(code: Option<i32>, bytes: &[u8]) -> Result<Option<PathBuf>, String> {
    use std::os::unix::ffi::OsStringExt;
    if code == Some(1) {
        return Ok(None);
    }
    if code != Some(0) {
        return Err(
            "The desktop file dialog could not open. You can still paste a folder path.".into(),
        );
    }
    let path = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(std::ffi::OsString::from_vec(
        path.to_vec(),
    ))))
}

/// Opens a native picker and waits for it to close.
///
/// Blocks until the user answers, so call it off the window thread.
/// `Ok(None)` means the user cancelled. The error text is shown to the user.
pub fn choose(target: &Target) -> Result<Option<PathBuf>, String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        return Err(
            "Folder browsing needs a desktop session. Paste the folder path to continue.".into(),
        );
    }
    let report = matches!(target, Target::Report);
    let artwork = matches!(target, Target::Artwork(_));
    let title = match target {
        Target::Game => "Choose a games location",
        Target::Storage(_) => "Choose where Maximum Space stores should live",
        Target::Report => "Import a compatibility report",
        Target::Artwork(_) => "Choose local game artwork",
    };
    // Try the desktop's own dialog first and fall back to the other when the
    // program is not installed.
    let kde = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_lowercase()
        .contains("kde");
    let programs = if kde {
        ["kdialog", "zenity"]
    } else {
        ["zenity", "kdialog"]
    };
    for program in programs {
        let mut command = Command::new(program);
        if program == "kdialog" {
            command.arg("--title").arg(title);
            if artwork {
                command.args(["--getopenfilename", ".", "*.png *.jpg *.jpeg"]);
            } else if report {
                command.args(["--getopenfilename", ".", "*.json"]);
            } else {
                command.args(["--getexistingdirectory", "."]);
            }
        } else {
            command.arg("--file-selection").arg("--title").arg(title);
            if artwork {
                command.arg("--file-filter=Images | *.png *.jpg *.jpeg");
            } else if report {
                command.arg("--file-filter=JSON reports | *.json");
            } else {
                command.arg("--directory");
            }
        }
        match command.output() {
            Ok(output) => return selected(output.status.code(), &output.stdout),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("Could not open the folder picker: {error}")),
        }
    }
    Err("Install Zenity or KDialog to browse folders. You can still paste a path.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestResult, check, check_eq};
    #[test]
    fn cancellation_and_paths_are_preserved() -> TestResult {
        check(
            selected(Some(1), b"/ignored\n")?.is_none(),
            "cancel leaves selection unchanged",
        )?;
        check_eq(
            selected(Some(0), b"/games/ leading and trailing \n")?,
            Some(PathBuf::from("/games/ leading and trailing ")),
            "spaces in names survive selection",
        )?;
        check(selected(Some(2), b"").is_err(), "dialog failure is visible")
    }
}
