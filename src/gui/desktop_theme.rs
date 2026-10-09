//! The desktop's light or dark preference, read once before the window opens.
//!
//! iced answers the same question only after the window exists, so the first
//! frame would otherwise assume dark. iced's own answer replaces this one.

use iced::theme::Mode;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long `gsettings` may take before the answer is dark.
const LIMIT: Duration = Duration::from_millis(300);

/// The preference from `GTK_THEME`, then `gsettings`, otherwise dark.
/// Blocks for at most [`LIMIT`]. Call it before the application starts.
pub fn initial_mode() -> Mode {
    let from_env = std::env::var("GTK_THEME")
        .ok()
        .and_then(|value| parse_gtk_theme(&value));
    from_env
        .or_else(|| {
            let output = run_limited(
                "gsettings",
                &["get", "org.gnome.desktop.interface", "color-scheme"],
                LIMIT,
            )?;
            parse_color_scheme(&output)
        })
        .unwrap_or(Mode::Dark)
}

/// `Adwaita:dark` is dark and `Adwaita:light` is light. Any other value says
/// nothing about the variant.
fn parse_gtk_theme(value: &str) -> Option<Mode> {
    let (_, variant) = value.trim().rsplit_once(':')?;
    match variant.trim() {
        "dark" => Some(Mode::Dark),
        "light" => Some(Mode::Light),
        _ => None,
    }
}

/// The output of `gsettings get ... color-scheme`. GNOME draws `'default'`
/// light.
fn parse_color_scheme(output: &str) -> Option<Mode> {
    match output.trim().trim_matches('\'') {
        "prefer-dark" => Some(Mode::Dark),
        "prefer-light" | "default" => Some(Mode::Light),
        _ => None,
    }
}

/// Runs a program and returns its output when it exits successfully within
/// `limit`. A program that runs longer is killed. A missing program, a
/// failing one and a timeout all return `None`.
fn run_limited(program: &str, args: &[&str], limit: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let began = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if began.elapsed() < limit => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestResult, check, check_eq};

    #[test]
    fn gtk_theme_names_the_variant_after_a_colon() -> TestResult {
        for (input, want) in [
            ("Adwaita:dark", Some(Mode::Dark)),
            ("Adwaita:light", Some(Mode::Light)),
            (" Breeze:dark\n", Some(Mode::Dark)),
            ("Adwaita", None),
            ("Adwaita:", None),
            ("Adwaita:contrast", None),
            ("", None),
        ] {
            check_eq(parse_gtk_theme(input), want, format!("{input:?}"))?;
        }
        Ok(())
    }

    #[test]
    fn the_color_scheme_is_read_from_gsettings_output() -> TestResult {
        for (input, want) in [
            ("'prefer-dark'\n", Some(Mode::Dark)),
            ("'prefer-light'\n", Some(Mode::Light)),
            ("'default'\n", Some(Mode::Light)),
            ("prefer-dark", Some(Mode::Dark)),
            ("''\n", None),
            ("'surprise'\n", None),
            ("", None),
        ] {
            check_eq(parse_color_scheme(input), want, format!("{input:?}"))?;
        }
        Ok(())
    }

    #[test]
    fn a_slow_program_is_killed_at_the_limit() -> TestResult {
        let began = Instant::now();
        let slow = run_limited("sleep", &["5"], Duration::from_millis(100));
        check(slow.is_none(), "a program past the limit gives no answer")?;
        check(
            began.elapsed() < Duration::from_secs(2),
            format!("and the wait ended at the limit: {:?}", began.elapsed()),
        )?;
        let fast = run_limited("echo", &["'prefer-dark'"], Duration::from_secs(5));
        check_eq(
            fast.as_deref(),
            Some("'prefer-dark'\n"),
            "control: a quick program returns its output",
        )?;
        check(
            run_limited("flummox-no-such-program", &[], LIMIT).is_none(),
            "a missing program gives no answer",
        )?;
        check(
            run_limited("false", &[], Duration::from_secs(5)).is_none(),
            "a failing program gives no answer",
        )
    }
}
