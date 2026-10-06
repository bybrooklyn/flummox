//! Reads installed launcher paths and GOG games from the Windows registry.
#![allow(unsafe_code)]
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS},
    System::Registry::*,
};
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: this handle was opened by RegOpenKeyExW and is closed once.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn open(root: HKEY, path: &str, view: u32) -> Result<Option<Key>> {
    let path = wide(path);
    let mut key = std::ptr::null_mut();
    // SAFETY: the path is terminated and key points to a writable handle slot.
    let error = unsafe { RegOpenKeyExW(root, path.as_ptr(), 0, KEY_READ | view, &mut key) };
    if error == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    ensure!(
        error == ERROR_SUCCESS,
        "Windows registry returned error {error}"
    );
    Ok(Some(Key(key)))
}
impl Key {
    fn string(&self, name: &str) -> Result<Option<String>> {
        let name = wide(name);
        let mut data = vec![0u16; 32768];
        let mut bytes = u32::try_from(data.len() * 2)?;
        // SAFETY: the key is live, name is terminated and data is writable for bytes bytes.
        let error = unsafe {
            RegGetValueW(
                self.0,
                std::ptr::null(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if error == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        ensure!(
            error == ERROR_SUCCESS && bytes % 2 == 0,
            "Reading registry value failed ({error})"
        );
        let units = data
            .get(..usize::try_from(bytes / 2)?)
            .context("Registry value exceeds buffer")?;
        let end = units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units.len());
        Ok(Some(String::from_utf16(
            units.get(..end).context("Registry string bounds")?,
        )?))
    }
    fn names(&self) -> Result<Vec<String>> {
        let mut names = vec![];
        for index in 0..20000 {
            let mut name = vec![0u16; 256];
            let mut length = u32::try_from(name.len())?;
            // SAFETY: the live key is enumerated into a writable buffer of length units.
            let error = unsafe {
                RegEnumKeyExW(
                    self.0,
                    index,
                    name.as_mut_ptr(),
                    &mut length,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if error == ERROR_NO_MORE_ITEMS {
                return Ok(names);
            }
            ensure!(
                error == ERROR_SUCCESS,
                "Enumerating registry keys failed ({error})"
            );
            names.push(String::from_utf16(
                name.get(..usize::try_from(length)?)
                    .context("Registry key bounds")?,
            )?);
        }
        anyhow::bail!("Registry game count exceeds 20000")
    }
}
pub fn steam_roots() -> (Vec<PathBuf>, Vec<String>) {
    let mut roots: Vec<_> = [
        std::env::var_os("ProgramFiles(x86)"),
        std::env::var_os("ProgramFiles"),
    ]
    .into_iter()
    .flatten()
    .map(|path| PathBuf::from(path).join("Steam"))
    .filter(|path| path.is_dir())
    .collect();
    let mut warnings = vec![];
    for (hive, name) in [
        (HKEY_CURRENT_USER, "SteamPath"),
        (HKEY_LOCAL_MACHINE, "InstallPath"),
    ] {
        for view in [KEY_WOW64_32KEY, KEY_WOW64_64KEY] {
            let result = (|| -> Result<Option<String>> {
                let Some(key) = open(hive, "Software\\Valve\\Steam", view)? else {
                    return Ok(None);
                };
                key.string(name)
            })();
            match result {
                Ok(Some(path)) => {
                    let path = PathBuf::from(path);
                    if !roots.contains(&path) {
                        roots.push(path);
                    }
                }
                Ok(None) => {}
                Err(error) => warnings.push(format!("Steam registry: {error}")),
            }
        }
    }
    (roots, warnings)
}
pub fn gog(catalog: &mut crate::desktop_discovery::Catalog) {
    for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        for view in [KEY_WOW64_32KEY, KEY_WOW64_64KEY] {
            let result = (|| -> Result<()> {
                let Some(root) = open(hive, "Software\\GOG.com\\Games", view)? else {
                    return Ok(());
                };
                for id in root.names()? {
                    let result = (|| -> Result<Option<crate::model::Game>> {
                        let Some(key) = open(root.0, &id, 0)? else {
                            return Ok(None);
                        };
                        let Some(path) = key.string("path")? else {
                            return Ok(None);
                        };
                        let title = key.string("gameName")?.unwrap_or_else(|| id.clone());
                        let path = PathBuf::from(path);
                        ensure!(
                            path.is_absolute() && path.parent().is_some(),
                            "GOG install directory is invalid"
                        );
                        let mut game = crate::desktop::manual_game(title, path);
                        game.id =
                            crate::model::GameId::new(crate::model::Launcher::Gog, id.clone());
                        game.build = key.string("buildId")?.or(key.string("version")?);
                        if !game.install_dir.is_dir() {
                            game.state = crate::model::InstallState::Broken {
                                detail: "Drive or folder unavailable".into(),
                            };
                        }
                        Ok(Some(game))
                    })();
                    match result {
                        Ok(Some(game)) => catalog.games.push(game),
                        Ok(None) => {}
                        Err(error) => catalog.warnings.push(format!("GOG {id}: {error}")),
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                catalog.warnings.push(format!("GOG registry: {error}"));
            }
        }
    }
}
pub fn discover() -> crate::desktop_discovery::Catalog {
    let (roots, warnings) = steam_roots();
    let mut catalog = crate::desktop_discovery::Catalog {
        warnings,
        ..Default::default()
    };
    catalog.steam(roots);
    if let Some(program_data) = std::env::var_os("ProgramData") {
        catalog.epic(&PathBuf::from(program_data).join("Epic/EpicGamesLauncher/Data/Manifests"));
    }
    gog(&mut catalog);
    if let Some(app_data) = std::env::var_os("APPDATA") {
        catalog.heroic(&PathBuf::from(app_data).join("heroic"));
    }
    catalog.finish()
}

/// Updates only the startup entry for this installation.
pub fn startup(enabled: bool) -> Result<()> {
    let executable = std::env::current_exe()?.with_file_name("flummox-gui.exe");
    let command = format!(
        "\"{}\" --background",
        executable
            .to_str()
            .context("Startup executable path cannot be encoded")?
    );
    let path = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let mut handle = std::ptr::null_mut();
    // SAFETY: the path is terminated and handle is a writable registry handle slot.
    let result = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_READ | KEY_SET_VALUE,
            std::ptr::null(),
            &mut handle,
            std::ptr::null_mut(),
        )
    };
    ensure!(
        result == ERROR_SUCCESS,
        "Cannot open login startup settings ({result})"
    );
    let key = Key(handle);
    if let Some(previous) = key.string("Flummox")? {
        ensure!(
            previous.eq_ignore_ascii_case(&command),
            "The Flummox startup entry belongs to another installation"
        );
    }
    let value = wide("Flummox");
    let result = if enabled {
        ensure!(
            executable.is_file(),
            "The installed Flummox GUI could not be found"
        );
        let command = wide(&command);
        // SAFETY: both strings are terminated, the key is live and the exact data byte count is supplied.
        unsafe {
            RegSetValueExW(
                key.0,
                value.as_ptr(),
                0,
                REG_SZ,
                command.as_ptr().cast(),
                u32::try_from(command.len() * 2)?,
            )
        }
    } else {
        // SAFETY: the key is live and value is terminated; only the verified owned entry is removed.
        unsafe { RegDeleteValueW(key.0, value.as_ptr()) }
    };
    ensure!(
        result == ERROR_SUCCESS || (!enabled && result == ERROR_FILE_NOT_FOUND),
        "Updating login startup failed ({result})"
    );
    Ok(())
}
