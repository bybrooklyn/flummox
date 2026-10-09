//! Current-user process checks used to defer native storage operations.
#![allow(unsafe_code)]
use anyhow::{Result, ensure};
use windows_sys::Win32::{
    Foundation::*,
    Security::Authorization::ConvertSidToStringSidW,
    Security::*,
    System::{RemoteDesktop::*, Threading::*},
};
/// Formats a SID as text such as `S-1-5-21-...`. Reads at most 256 UTF-16 units.
fn sid_string(sid: PSID) -> Result<String> {
    let mut pointer = std::ptr::null_mut();
    // SAFETY: WTS provides a live SID until WTSFreeMemory; pointer is an output slot.
    let result = unsafe { ConvertSidToStringSidW(sid, &mut pointer) };
    ensure!(result != 0, "Cannot identify a desktop process");
    let mut units = vec![];
    for offset in 0..256 {
        // SAFETY: ConvertSidToStringSidW returns a terminated UTF-16 allocation.
        let value = unsafe { *pointer.wrapping_add(offset) };
        if value == 0 {
            break;
        }
        units.push(value);
    }
    // SAFETY: the string was allocated by ConvertSidToStringSidW and is no longer used.
    unsafe {
        LocalFree(pointer.cast());
    }
    Ok(String::from_utf16(&units)?)
}
/// Returns a reason to hold storage work, or `None`. It looks only at this user's
/// processes: one whose executable is inside a game folder, or one of Heroic's
/// download tools. A process that cannot be opened or named also returns a reason.
pub fn busy(games: &[crate::model::Game]) -> Result<Option<String>> {
    let user = crate::windows::ipc::user_sid()?;
    let mut pointer = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: WTS writes an allocated array and its element count into these slots.
    let result = unsafe {
        WTSEnumerateProcessesW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut pointer, &mut count)
    };
    ensure!(result != 0, "Process information is unavailable");
    // Frees the WTS array on every return path below.
    struct Processes(*mut WTS_PROCESS_INFOW);
    impl Drop for Processes {
        fn drop(&mut self) {
            // SAFETY: this pointer came from WTSEnumerateProcessesW and is freed once.
            unsafe {
                WTSFreeMemory(self.0.cast());
            }
        }
    }
    let _owned = Processes(pointer);
    ensure!(count <= 100000, "Process list exceeds its limit");
    if count == 0 {
        return Ok(None);
    }
    // Game folders are resolved once per check, and each distinct process path once,
    // since many processes share an executable. A folder that no longer resolves
    // cannot hold a running process and is left out.
    let roots: Vec<(&crate::model::Game, std::path::PathBuf)> = games
        .iter()
        .filter_map(|game| Some((game, game.install_dir.canonicalize().ok()?)))
        .collect();
    let mut resolved: std::collections::HashMap<std::path::PathBuf, std::path::PathBuf> =
        std::collections::HashMap::new();
    // SAFETY: the successful API call allocated count initialized WTS_PROCESS_INFOW entries.
    let processes = unsafe { std::slice::from_raw_parts(pointer, usize::try_from(count)?) };
    for process in processes {
        // Skip this process, processes with no owner SID, and other users' processes.
        if process.ProcessId == std::process::id() || process.pUserSid.is_null() {
            continue;
        }
        if sid_string(process.pUserSid)? != user {
            continue;
        }
        // SAFETY: the PID is a value from the current WTS enumeration; no pointers are passed.
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process.ProcessId) };
        if handle.is_null() {
            return Ok(Some("process information is unavailable".into()));
        }
        // 32768 UTF-16 units covers the longest path Windows produces. On success
        // `length` is the number of units written, without the terminator.
        let mut path = vec![0u16; 32768];
        let mut length = u32::try_from(path.len())?;
        // SAFETY: handle is live and path is writable for length UTF-16 units.
        let result =
            unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length) };
        // SAFETY: OpenProcess returned this handle and this code closes it once.
        unsafe {
            CloseHandle(handle);
        }
        if result == 0 {
            return Ok(Some("process information is unavailable".into()));
        }
        let path = std::path::PathBuf::from(String::from_utf16(
            path.get(..usize::try_from(length)?)
                .ok_or_else(|| anyhow::anyhow!("Process path exceeds buffer"))?,
        )?);
        // Both forms come from Windows final paths; this also normalizes verbatim prefixes.
        let path = match resolved.get(&path) {
            Some(known) => known.clone(),
            None => {
                let canonical = path.canonicalize()?;
                resolved.insert(path, canonical.clone());
                canonical
            }
        };
        if let Some((game, _)) = roots.iter().find(|(_, root)| path.starts_with(root)) {
            return Ok(Some(format!("{} is running", game.title)));
        }
        let executable = path
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        // The command line tools Heroic runs for its Epic, GOG and Amazon stores.
        if ["legendary.exe", "gogdl.exe", "nile.exe"].contains(&executable.as_str()) {
            return Ok(Some("a launcher is installing or updating games".into()));
        }
    }
    Ok(None)
}
