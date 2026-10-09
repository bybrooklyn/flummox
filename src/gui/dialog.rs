//! Desktop folder and report selection through installed native dialogs.

use std::{
    io::Read,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::Duration,
};

/// The picker that is open. Kept so that closing the window can close it.
static OPEN_PICKER: Mutex<Option<Child>> = Mutex::new(None);

/// Closes the picker if one is open. Called when the window closes, since the
/// picker is a separate process that would otherwise stay on screen.
pub fn close_open_picker() {
    if let Ok(mut open) = OPEN_PICKER.lock()
        && let Some(child) = open.as_mut()
    {
        let _killed = child.kill();
    }
}

/// Whether the helper exited with the code a cancel gives because it could not
/// reach the display. Zenity uses 1 for both, so the message tells them apart.
fn could_not_open(code: Option<i32>, stderr: &[u8]) -> bool {
    const MARKERS: [&str; 5] = [
        "cannot open display",
        "failed to open display",
        "unable to init server",
        "no protocol specified",
        "could not connect",
    ];
    let text = String::from_utf8_lossy(stderr).to_lowercase();
    code == Some(1) && MARKERS.iter().any(|marker| text.contains(marker))
}

/// Runs a picker to the end and returns its exit code, stdout and stderr.
/// Blocks until it exits or `close_open_picker` kills it.
fn run(mut command: Command) -> std::io::Result<(Option<i32>, Vec<u8>, Vec<u8>)> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    if let Ok(mut open) = OPEN_PICKER.lock() {
        *open = Some(child);
    }
    let code = loop {
        let status = match OPEN_PICKER.lock() {
            Ok(mut open) => open.as_mut().map(Child::try_wait),
            Err(_) => None,
        };
        match status {
            Some(Ok(Some(status))) => break status.code(),
            Some(Ok(None)) => std::thread::sleep(Duration::from_millis(50)),
            Some(Err(error)) => return Err(error),
            None => break None,
        }
    };
    if let Ok(mut open) = OPEN_PICKER.lock() {
        *open = None;
    }
    // The helper has exited, so both pipes hold everything it wrote.
    let (mut out, mut err) = (vec![], vec![]);
    if let Some(pipe) = stdout.as_mut() {
        pipe.read_to_end(&mut out)?;
    }
    if let Some(pipe) = stderr.as_mut() {
        pipe.read_to_end(&mut err)?;
    }
    Ok((code, out, err))
}

/// What the picker is choosing. This sets the title, the file filter and
/// whether a folder or a file is expected.
#[derive(Debug, Clone)]
pub enum Target {
    /// A folder of games to add as a location.
    Game,
    /// The folder for a Maximum store. Holds the game's id string.
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
        Target::Game => "Choose a location",
        Target::Storage(_) => "Choose a folder for the Maximum store",
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
    let mut display_error = false;
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
        match run(command) {
            Ok((code, _, stderr)) if could_not_open(code, &stderr) => {
                display_error = true;
                continue;
            }
            Ok((code, stdout, _)) => return selected(code, &stdout),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("Could not open the folder picker: {error}")),
        }
    }
    if display_error {
        return Err(
            "The desktop file dialog could not reach the display. You can still paste a path."
                .into(),
        );
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

    #[test]
    fn a_missing_display_is_told_apart_from_a_cancel() -> TestResult {
        check(
            could_not_open(Some(1), b"Gtk-WARNING **: cannot open display: :0\n"),
            "zenity's display failure",
        )?;
        check(
            !could_not_open(Some(1), b""),
            "control: a cancel prints nothing",
        )?;
        check(
            !could_not_open(Some(1), b"Gtk-Message: Failed to load module\n"),
            "control: toolkit noise on a cancel is still a cancel",
        )?;
        check(
            !could_not_open(Some(0), b"cannot open display"),
            "a chosen path is never an error",
        )
    }

    #[test]
    fn closing_the_window_kills_an_open_picker() -> TestResult {
        let mut command = Command::new("sleep");
        command.arg("30");
        let worker = std::thread::spawn(move || run(command));
        // The picker registers itself once it has spawned.
        for _ in 0..100 {
            if OPEN_PICKER.lock().is_ok_and(|open| open.is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        close_open_picker();
        let (code, _, _) = worker
            .join()
            .map_err(|_| "picker thread panicked".to_owned())?
            .map_err(|error| error.to_string())?;
        check(code.is_none(), "a killed helper has no exit code")
    }
}
