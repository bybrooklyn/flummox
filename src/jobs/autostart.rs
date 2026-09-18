//! Opted-in maintenance starts with a freedesktop login session.

use anyhow::{Context, Result, ensure};
use std::{io::Write, path::Path};

const MARKER: &str = "# Managed by Flummox library maintenance\n";

fn entry(executable: &Path) -> Result<String> {
    let path = executable
        .to_str()
        .context("The worker path must be UTF-8 for desktop startup")?;
    ensure!(
        !path.chars().any(|c| c.is_control() || c == '='),
        "The worker path cannot be represented in a desktop entry"
    );
    // Desktop string unescaping precedes Exec unquoting.
    let mut quoted = String::new();
    for character in path.chars() {
        match character {
            '\\' => quoted.push_str("\\\\\\\\"),
            '"' | '$' | '`' => {
                quoted.push_str("\\\\");
                quoted.push(character);
            }
            '%' => quoted.push_str("%%"),
            other => quoted.push(other),
        }
    }
    Ok(format!(
        "{MARKER}[Desktop Entry]\nType=Application\nName=Flummox maintenance\nExec=\"{quoted}\" __coordinator\nTerminal=false\nNoDisplay=true\n"
    ))
}

/// Changes only this application's marked entry; unrelated files are refused.
pub(super) fn update(config: &Path, executable: &Path, enabled: bool) -> Result<()> {
    let dir = config.join("autostart");
    let path = dir.join("flummox-background.desktop");
    if path.exists() {
        ensure!(
            std::fs::symlink_metadata(&path)?.is_file(),
            "Startup entry must be a regular file"
        );
        ensure!(
            std::fs::read_to_string(&path)?.starts_with(MARKER),
            "The existing startup entry was not created by Flummox"
        );
    }
    if !enabled {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    let temporary = dir.join(format!(".flummox-{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(entry(executable)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _removed = std::fs::remove_file(&temporary);
    }
    result
}

pub(super) fn configure(executable: &Path, enabled: bool) -> Result<()> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| crate::launchers::Env::current().map(|env| env.home.join(".config")))
        .context("Cannot locate startup settings")?;
    update(&config, executable, enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    #[test]
    fn startup_is_opt_in_and_does_not_replace_foreign_entries() -> TestResult {
        let temp = tempfile::tempdir().ctx("config")?;
        let executable = Path::new("/apps/Flummox space/flummox");
        update(temp.path(), executable, false).ctx("off")?;
        check(
            !temp.path().join("autostart").exists(),
            "off writes no configuration",
        )?;
        update(temp.path(), executable, true).ctx("enable")?;
        let path = temp.path().join("autostart/flummox-background.desktop");
        check(
            std::fs::read_to_string(&path)
                .ctx("entry")?
                .contains("Exec=\"/apps/Flummox space/flummox\" __coordinator"),
            "spaces stay inside one argument",
        )?;
        update(temp.path(), executable, false).ctx("disable")?;
        std::fs::write(&path, "foreign").ctx("foreign entry")?;
        check(
            update(temp.path(), executable, true).is_err(),
            "foreign content is refused",
        )?;
        check_eq(
            std::fs::read_to_string(&path).ctx("entry retained")?,
            "foreign".into(),
            "existing customization survives",
        )
    }
}
