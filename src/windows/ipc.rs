//! Current-user named pipe transport with bounded frames and remote rejection.
#![allow(unsafe_code)]
use anyhow::{Context, Result, ensure};
use std::{
    io::Write,
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::{Pipes::*, Threading::*},
};
/// Frees a `LocalAlloc` allocation on drop.
struct Local(*mut core::ffi::c_void);
impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: the allocation was returned by a Win32 LocalAlloc-backed API.
        unsafe {
            LocalFree(self.0);
        }
    }
}
/// The current user's SID as text, such as `S-1-5-21-...`. It names the pipe and
/// is the only identity the pipe's access list admits.
pub fn user_sid() -> Result<String> {
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle without pointers.
    let process = unsafe { GetCurrentProcess() };
    let mut handle = std::ptr::null_mut();
    // SAFETY: the current process pseudo-handle is valid and handle is a writable slot.
    let result = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut handle) };
    ensure!(result != 0, "Cannot read the current user's token");
    // SAFETY: the successful token handle is uniquely transferred to an owned file for closing.
    let token = unsafe { std::fs::File::from_raw_handle(handle.cast()) };
    // The first call has no buffer and exists to learn the size. Its failure status
    // is expected, so only `needed` is examined.
    let mut needed = 0;
    // SAFETY: the sizing call writes only needed and does not access a null buffer.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle().cast(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    ensure!(
        needed > 0 && needed <= 65536,
        "Invalid current-user token size"
    );
    let mut data = vec![0u64; usize::try_from(needed)?.div_ceil(8)];
    // SAFETY: data has enough bytes and u64 alignment for TOKEN_USER on supported x64 Windows.
    let result = unsafe {
        GetTokenInformation(
            token.as_raw_handle().cast(),
            TokenUser,
            data.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    ensure!(result != 0, "Cannot read the current user's identity");
    // SAFETY: the successful call filled a TOKEN_USER in the aligned buffer.
    let sid = unsafe { (*data.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut string = std::ptr::null_mut();
    // SAFETY: sid remains inside the live token buffer and string is a writable output slot.
    let result = unsafe { ConvertSidToStringSidW(sid, &mut string) };
    ensure!(result != 0, "Cannot format the user's identity");
    let _owned = Local(string.cast());
    let mut units = vec![];
    for offset in 0..256 {
        // SAFETY: ConvertSidToStringSidW returns an allocated terminated UTF-16 SID string.
        let unit = unsafe { *string.wrapping_add(offset) };
        if unit == 0 {
            return Ok(String::from_utf16(&units)?);
        }
        units.push(unit);
    }
    anyhow::bail!("Current-user SID exceeds its limit")
}
/// UTF-16 with a terminating zero, as the wide Win32 calls expect.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
/// The pipe name. It contains the user's SID, so each user gets a separate pipe.
/// Test builds append FLUMMOX_IPC_TEST_NAME so a fixture never reaches a real worker.
fn name() -> Result<String> {
    let base = format!("\\\\.\\pipe\\flummox-{}-v1", user_sid()?);
    #[cfg(test)]
    if let Ok(suffix) = std::env::var("FLUMMOX_IPC_TEST_NAME") {
        return Ok(format!("{base}-{suffix}"));
    }
    Ok(base)
}
/// Creates the server end of the pipe. Fails if any process already serves the name.
/// The handle is nonblocking and serves one client at a time. Poll it with `accept`.
pub fn listener() -> Result<std::fs::File> {
    listener_named(&name()?)
}
fn listener_named(pipe_name: &str) -> Result<std::fs::File> {
    // A protected DACL with one entry: generic read and write for this user's SID.
    // The 1 passed below is SDDL_REVISION_1.
    let sddl = wide(&format!("D:P(A;;GRGW;;;{})", user_sid()?));
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: sddl is terminated and descriptor is a writable output slot.
    let result = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    ensure!(
        result != 0,
        "Cannot restrict the coordinator pipe to this user"
    );
    let _owned = Local(descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())?,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let name = wide(pipe_name);
    // FILE_FLAG_FIRST_PIPE_INSTANCE makes creation fail when the name is taken.
    // PIPE_NOWAIT makes connect, read and write return at once. The numbers are one
    // instance, 64 KiB buffers each way and a 1000 ms default client wait.
    // SAFETY: the name and security descriptor stay live until CreateNamedPipe copies them.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65536,
            65536,
            1000,
            &attributes,
        )
    };
    ensure!(
        handle != INVALID_HANDLE_VALUE,
        "Cannot create coordinator pipe: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the successful pipe handle is uniquely transferred to this owned file.
    Ok(unsafe { std::fs::File::from_raw_handle(handle.cast()) })
}
/// Opens the client end, nonblocking. Fails at once if no worker is listening or
/// the worker is serving another client.
pub fn connect() -> Result<std::fs::File> {
    connect_named(&name()?)
}
fn connect_named(name: &str) -> Result<std::fs::File> {
    // SECURITY_IDENTIFICATION lets the server learn who the client is and stops it
    // from acting with the client's rights.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
        .open(name)?;
    let mode = PIPE_NOWAIT;
    // SAFETY: file owns a live pipe and mode points to a valid DWORD.
    let result = unsafe {
        SetNamedPipeHandleState(
            file.as_raw_handle().cast(),
            &mode,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    ensure!(result != 0, "Cannot set pipe timeout mode");
    Ok(file)
}
/// Polls for a client. `Ok(true)` means one is connected. `Ok(false)` means none
/// yet, so call again after a short sleep.
pub fn accept(file: &std::fs::File) -> Result<bool> {
    // SAFETY: file owns a live non-overlapped pipe handle.
    let result = unsafe { ConnectNamedPipe(file.as_raw_handle().cast(), std::ptr::null_mut()) };
    if result != 0 {
        // In PIPE_NOWAIT mode this only makes the instance available to clients.
        return Ok(false);
    }
    // SAFETY: GetLastError takes no pointers and reads this thread's error slot.
    match unsafe { GetLastError() } {
        ERROR_PIPE_CONNECTED => Ok(true),
        ERROR_PIPE_LISTENING => Ok(false),
        // The last client closed its end and the instance was never reset.
        ERROR_NO_DATA => {
            disconnect(file);
            Ok(false)
        }
        error => anyhow::bail!("Accepting coordinator client failed ({error})"),
    }
}
/// Drops the current client so the single instance can take the next one. A
/// failure is not reported.
pub fn disconnect(file: &std::fs::File) {
    // SAFETY: file owns a live named pipe handle.
    unsafe {
        DisconnectNamedPipe(file.as_raw_handle().cast());
    }
}
/// Fills `bytes` from a nonblocking pipe, sleeping 5 ms whenever it is empty, until
/// `deadline`. A closed connection is an error.
fn read_exact_wait(
    file: &mut std::fs::File,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> Result<()> {
    while !bytes.is_empty() {
        ensure!(Instant::now() < deadline, "Coordinator response timed out");
        let mut count = 0;
        // SAFETY: file owns a synchronous handle; bytes is writable for its stated
        // length, count is a live output slot, and no OVERLAPPED pointer is used.
        let read = unsafe {
            ReadFile(
                file.as_raw_handle().cast(),
                bytes.as_mut_ptr(),
                u32::try_from(bytes.len())?,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        let result = if read != 0 {
            Ok(usize::try_from(count)?)
        } else {
            Err(std::io::Error::last_os_error())
        };
        // std::fs::File maps ERROR_NO_DATA to EOF. A nonblocking pipe needs to
        // distinguish that temporary empty buffer from ERROR_BROKEN_PIPE.
        match result {
            Ok(0) => anyhow::bail!("Coordinator connection closed"),
            Ok(count) => bytes = bytes.get_mut(count..).context("Pipe read bounds")?,
            // 232 is ERROR_NO_DATA and 536 is ERROR_PIPE_LISTENING: nothing to read yet.
            Err(error) if matches!(error.raw_os_error(), Some(232 | 536)) => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
/// Reads one frame: a 4-byte little-endian length, then that many bytes of JSON.
/// The length is checked against 16 MiB before anything is allocated. The whole
/// frame must arrive within 3 seconds.
pub fn receive<T: serde::de::DeserializeOwned>(file: &mut std::fs::File) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut header = [0u8; 4];
    read_exact_wait(file, &mut header, deadline)?;
    let length = usize::try_from(u32::from_le_bytes(header))?;
    ensure!(
        length <= 16 * 1024 * 1024,
        "Coordinator frame exceeds 16 MiB"
    );
    let mut bytes = vec![0; length];
    read_exact_wait(file, &mut bytes, deadline)?;
    Ok(serde_json::from_slice(&bytes)?)
}
/// Writes one frame in the format `receive` reads, within 3 seconds. A nonblocking
/// pipe accepts what fits in its buffer, so the loop retries the remainder.
pub fn send<T: serde::Serialize>(file: &mut std::fs::File, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "Coordinator frame exceeds 16 MiB"
    );
    let header = u32::try_from(bytes.len())?.to_le_bytes();
    let deadline = Instant::now() + Duration::from_secs(3);
    for mut remaining in [header.as_slice(), bytes.as_slice()] {
        while !remaining.is_empty() {
            ensure!(Instant::now() < deadline, "Coordinator write timed out");
            match file.write(remaining) {
                Ok(0) => std::thread::sleep(Duration::from_millis(5)),
                Ok(count) => remaining = remaining.get(count..).context("Pipe write bounds")?,
                // The same two codes as in `read_exact_wait`: the pipe is not ready.
                Err(error) if matches!(error.raw_os_error(), Some(232 | 536)) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

/// Connects to a fixture worker's pipe by suffix, retrying for up to 5 seconds
/// while that worker starts.
#[cfg(test)]
pub(crate) fn test_connect(suffix: &str) -> Result<std::fs::File> {
    let name = format!("{}-{suffix}", name()?);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match connect_named(&name) {
            Ok(file) => return Ok(file),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check};
    use std::io::{Seek, SeekFrom};
    #[test]
    fn an_empty_pipe_waits_for_a_delayed_reply() -> TestResult {
        let temp = tempfile::tempdir().ctx("delayed pipe fixture")?;
        let suffix = temp
            .path()
            .file_name()
            .ctx("pipe suffix")?
            .to_string_lossy();
        let name = format!("\\\\.\\pipe\\flummox-delayed-{suffix}");
        let mut server = listener_named(&name).ctx("delayed pipe listener")?;
        let client = std::thread::spawn(move || -> Result<bool> {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut file = loop {
                match connect_named(&name) {
                    Ok(file) => break file,
                    Err(error) if Instant::now() >= deadline => return Err(error),
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            };
            send(&mut file, &true)?;
            let reply: bool = receive(&mut file)?;
            send(&mut file, &true)?;
            Ok(reply)
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !accept(&server).ctx("accept delayed pipe")? {
            check(Instant::now() < deadline, "delayed client connects")?;
            std::thread::sleep(Duration::from_millis(5));
        }
        check(
            receive::<bool>(&mut server).ctx("request")?,
            "request received",
        )?;
        // The client is now reading an empty pipe. It must wait, not report EOF.
        std::thread::sleep(Duration::from_millis(100));
        send(&mut server, &true).ctx("delayed reply")?;
        check(
            receive::<bool>(&mut server).ctx("acknowledgement")?,
            "reply read",
        )?;
        disconnect(&server);
        let reply = client
            .join()
            .map_err(|_| "pipe client thread stopped".to_owned())?
            .ctx("client reads delayed reply")?;
        check(reply, "empty pipe waits instead of reporting EOF")
    }
    #[test]
    fn frames_reject_truncation_and_excessive_lengths() -> TestResult {
        let mut file = tempfile::tempfile().ctx("frame fixture")?;
        file.write_all(&u32::MAX.to_le_bytes())
            .ctx("oversized header")?;
        file.seek(SeekFrom::Start(0)).ctx("rewind")?;
        check(
            receive::<serde_json::Value>(&mut file).is_err(),
            "oversized frame is rejected before allocation",
        )?;
        file.set_len(0).ctx("clear fixture")?;
        file.seek(SeekFrom::Start(0)).ctx("rewind")?;
        file.write_all(&5u32.to_le_bytes())
            .ctx("truncated header")?;
        file.write_all(b"{").ctx("truncated JSON")?;
        file.seek(SeekFrom::Start(0)).ctx("rewind")?;
        check(
            receive::<serde_json::Value>(&mut file).is_err(),
            "truncated frame is rejected",
        )?;
        check(
            user_sid().ctx("current user")?.starts_with("S-1-"),
            "pipe identity uses the current Windows SID",
        )
    }
    #[test]
    fn pipe_acl_grants_only_the_current_user_and_prevents_duplicate_servers() -> TestResult {
        let suffix = tempfile::tempdir().ctx("pipe ACL fixture")?;
        let unique = suffix
            .path()
            .file_name()
            .ctx("pipe suffix")?
            .to_string_lossy();
        let name = format!("\\\\.\\pipe\\flummox-acl-{unique}");
        let listener = listener_named(&name).ctx("isolated pipe")?;
        check(
            listener_named(&name).is_err(),
            "a second server cannot take the pipe name",
        )?;
        let mut descriptor = std::ptr::null_mut();
        let mut acl = std::ptr::null_mut();
        // SAFETY: listener owns a live kernel object; ACL and descriptor are output slots.
        let result = unsafe {
            GetSecurityInfo(
                listener.as_raw_handle().cast(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        check(
            result == ERROR_SUCCESS,
            "pipe security descriptor can be inspected",
        )?;
        let _owned = Local(descriptor);
        check(!acl.is_null(), "pipe uses an explicit restricted DACL")?;
        let mut information = ACL_SIZE_INFORMATION::default();
        // SAFETY: GetSecurityInfo returned a live ACL and information is a writable fixed-size slot.
        let result = unsafe {
            GetAclInformation(
                acl,
                (&mut information as *mut ACL_SIZE_INFORMATION).cast(),
                u32::try_from(std::mem::size_of::<ACL_SIZE_INFORMATION>()).ctx("ACL size")?,
                AclSizeInformation,
            )
        };
        check(
            result != 0 && information.AceCount == 1,
            "pipe grants one identity access",
        )?;
        let mut ace = std::ptr::null_mut();
        // SAFETY: the live ACL contains one ACE and ace is a writable output slot.
        let result = unsafe { GetAce(acl, 0, &mut ace) };
        check(result != 0 && !ace.is_null(), "pipe ACE can be inspected")?;
        // SAFETY: the security descriptor was created with one ACCESS_ALLOWED_ACE.
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        check(
            allowed.Header.AceType
                == windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE as u8,
            "only the current user is allowed",
        )?;
        let sid = std::ptr::addr_of!(allowed.SidStart).cast_mut().cast();
        let mut sid_text = std::ptr::null_mut();
        // SAFETY: SidStart belongs to the ACL's initialized allowed ACE and text is an output slot.
        let result = unsafe { ConvertSidToStringSidW(sid, &mut sid_text) };
        check(result != 0, "allowed SID can be read")?;
        let _sid_owned = Local(sid_text.cast());
        let mut units = vec![];
        for offset in 0..256 {
            // SAFETY: the successful SID conversion returned a terminated UTF-16 allocation.
            let unit = unsafe { *sid_text.wrapping_add(offset) };
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        check(
            String::from_utf16(&units).ctx("ACL SID")? == user_sid().ctx("current user")?,
            "pipe access belongs only to the current user",
        )
    }
}
