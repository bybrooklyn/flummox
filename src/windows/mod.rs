//! Native Windows compression entrypoint.
//!
//! WOF keeps logical file bytes and paths unchanged. Writes expand a file, so
//! the maintenance pass can safely reapply compression after launcher updates.

#![allow(unsafe_code)]

mod activity;
pub mod coordinator;
mod ipc;
pub(crate) mod launchers;
mod tray;

use crate::desktop_jobs::Rejected;
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use std::{
    fs::OpenOptions,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use windows_sys::Win32::{
    Foundation::{ERROR_COMPRESSION_NOT_BENEFICIAL, GetLastError, HANDLE},
    Storage::FileSystem::{
        FILE_PROVIDER_COMPRESSION_LZX, FILE_PROVIDER_COMPRESSION_XPRESS4K,
        FILE_PROVIDER_COMPRESSION_XPRESS8K, FILE_PROVIDER_COMPRESSION_XPRESS16K,
        FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx,
        WOF_FILE_COMPRESSION_INFO_V1, WOF_PROVIDER_FILE, WofIsExternalFile, WofSetFileDataLocation,
    },
    System::{IO::DeviceIoControl, Ioctl::FSCTL_DELETE_EXTERNAL_BACKING},
};

// The WOF file-provider algorithms. A doc comment on a `ValueEnum` variant becomes
// its help text, so notes here stay plain comments.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Algorithm {
    Lzx,
    Xpress16k,
    Xpress8k,
    Xpress4k,
}

impl Algorithm {
    fn code(self) -> u32 {
        match self {
            Self::Lzx => FILE_PROVIDER_COMPRESSION_LZX,
            Self::Xpress16k => FILE_PROVIDER_COMPRESSION_XPRESS16K,
            Self::Xpress8k => FILE_PROVIDER_COMPRESSION_XPRESS8K,
            Self::Xpress4k => FILE_PROVIDER_COMPRESSION_XPRESS4K,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "flummox",
    version,
    about = "Compress game folders and keep them playable"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Apply transparent Windows compression to a folder tree.
    Compress {
        folder: PathBuf,
        #[arg(long, value_enum, default_value_t = Algorithm::Lzx)]
        algorithm: Algorithm,
    },
    /// Inspect temporary-space requirements without changing files.
    Analyze {
        folder: PathBuf,
        #[arg(long)]
        restore: bool,
    },
    /// Restore ordinary NTFS storage for a folder tree.
    Decompress { folder: PathBuf },
}

/// Running totals for one pass. `bytes` is logical size. The two allocation fields
/// sum the visited files' allocated size before and after each was processed.
#[derive(Debug, Clone, Default)]
pub(crate) struct Progress {
    pub files: u64,
    pub changed: u64,
    pub skipped: u64,
    pub bytes: u64,
    pub allocation_before: u64,
    pub allocation_after: u64,
}

/// Files a pass could not process. The pass went on without them.
#[derive(Debug, Default)]
struct Failures {
    count: u64,
    /// The first failure's message, for the result line.
    first: Option<String>,
}

/// What an operation did to one file.
enum Outcome {
    Changed,
    /// Already in the requested state.
    Unchanged,
    /// Windows declined to compress it because the result would not be smaller.
    Rejected,
}

/// The HRESULT that wraps a nonzero Win32 error code: failure bit, FACILITY_WIN32.
fn hresult_from_win32(code: u32) -> i32 {
    (0x8007_0000u32 | code) as i32
}

/// Opens a file for read and write with no sharing. The open fails while any other
/// handle to the file exists, such as one held by a running game.
fn file_handle(path: &Path) -> Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path)
        .with_context(|| format!("Opening {}", path.display()))
}

/// Bytes the file occupies on disk, as opposed to its logical length. WOF
/// compression lowers this number and leaves the length alone.
pub(crate) fn allocation_size(path: &Path) -> Result<u64> {
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("Reading storage usage for {}", path.display()))?;
    let mut info = FILE_STANDARD_INFO::default();
    // SAFETY: `info` is a writable FILE_STANDARD_INFO with its exact size,
    // and the file handle remains live for the duration of the call.
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileStandardInfo,
            std::ptr::from_mut(&mut info).cast(),
            u32::try_from(std::mem::size_of_val(&info))?,
        )
    };
    ensure!(result != 0, "Windows could not read allocated file size");
    u64::try_from(info.AllocationSize).context("Windows returned a negative allocation size")
}

/// Compresses one file with WOF. A file already using this algorithm, or backed by
/// another provider, is `Unchanged`. One Windows will not shrink is `Rejected`.
fn compress_file(path: &Path, algorithm: Algorithm) -> Result<Outcome> {
    // Query by name before opening anything. Opening a WOF file for write expands
    // it, so a file already in this algorithm must never reach `file_handle`.
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut external = 0;
    let mut provider = 0;
    // A WIM-backed file answers with a longer structure than the file provider's
    // 8 bytes, so the reply buffer is larger than any known provider's.
    let mut reply = [0u64; 32];
    let mut size = u32::try_from(std::mem::size_of_val(&reply))?;
    // SAFETY: name is terminated and all output buffers have their exact writable sizes.
    let query = unsafe {
        WofIsExternalFile(
            name.as_ptr(),
            &mut external,
            &mut provider,
            reply.as_mut_ptr().cast(),
            &mut size,
        )
    };
    ensure!(
        query >= 0,
        "Windows could not query existing compression for {}",
        path.display()
    );
    // Backing by another provider, such as a system image, is not ours to change.
    if external != 0 && provider != WOF_PROVIDER_FILE {
        return Ok(Outcome::Unchanged);
    }
    // A reply from the file provider must be exactly one WOF_FILE_COMPRESSION_INFO_V1.
    ensure!(
        external == 0
            || size == u32::try_from(std::mem::size_of::<WOF_FILE_COMPRESSION_INFO_V1>())?,
        "Windows returned unfamiliar compression metadata"
    );
    // The structure starts with its 32-bit algorithm, which is the low half of the
    // first word on the little-endian targets Windows runs on here.
    let existing = reply
        .first()
        .map_or(0, |word| u32::try_from(word & 0xffff_ffff).unwrap_or(0));
    if external != 0 && existing == algorithm.code() {
        return Ok(Outcome::Unchanged);
    }
    let file = file_handle(path)?;
    let info = WOF_FILE_COMPRESSION_INFO_V1 {
        Algorithm: algorithm.code(),
        Flags: 0,
    };
    // SAFETY: the handle stays open for the call and `info` points to the
    // exact provider structure whose byte length is supplied.
    let result = unsafe {
        WofSetFileDataLocation(
            file.as_raw_handle() as HANDLE,
            WOF_PROVIDER_FILE,
            std::ptr::from_ref(&info).cast(),
            u32::try_from(std::mem::size_of_val(&info))?,
        )
    };
    if result >= 0 {
        return Ok(Outcome::Changed);
    }
    if result == hresult_from_win32(ERROR_COMPRESSION_NOT_BENEFICIAL) {
        return Ok(Outcome::Rejected);
    }
    anyhow::bail!(
        "Windows rejected compression for {} with HRESULT 0x{:08x}",
        path.display(),
        result as u32
    )
}

/// Returns one file to ordinary storage. `Unchanged` if it had no WOF backing.
fn decompress_file(path: &Path) -> Result<Outcome> {
    let file = file_handle(path)?;
    let mut returned = 0u32;
    // SAFETY: the live file handle is synchronous, both optional buffers are
    // null with zero lengths, and `returned` is writable for the call.
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_DELETE_EXTERNAL_BACKING,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if result != 0 {
        return Ok(Outcome::Changed);
    }
    // A file without external backing is already restored.
    // SAFETY: GetLastError reads this thread's Win32 error slot and takes no pointers.
    let error = unsafe { GetLastError() };
    // 1 is ERROR_INVALID_FUNCTION, 50 is ERROR_NOT_SUPPORTED, 342 is
    // ERROR_OBJECT_NOT_EXTERNALLY_BACKED and 4390 is ERROR_NOT_A_REPARSE_POINT.
    // 342 is what a file that was never compressed reports, and without it
    // a restore stopped at the first such file.
    if matches!(error, 1 | 50 | 342 | 4390) {
        return Ok(Outcome::Unchanged);
    }
    anyhow::bail!(
        "Windows rejected decompression for {} with error {}",
        path.display(),
        error
    )
}

/// Journal of the one pass in progress, kept in `windows-job.json`. It is written
/// before the first file is touched and removed after the last. One left on disk
/// means a pass ended early, by error, stop or crash.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Recovery {
    #[serde(with = "crate::path_serde")]
    pub root: PathBuf,
    pub restore: bool,
    pub volume: crate::storage::Volume,
}

fn journal_in(state: &Path) -> PathBuf {
    state.join("windows-job.json")
}

/// The journal of an unfinished pass, if there is one. Never more than one record.
pub fn recovery() -> Result<Vec<Recovery>> {
    recovery_in(&crate::libraries::data_dir()?)
}

fn recovery_in(state: &Path) -> Result<Vec<Recovery>> {
    match std::fs::read(journal_in(state)) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(record) => Ok(vec![record]),
            Err(error) => {
                // A journal that cannot be parsed cannot name a folder to review, and
                // would stop every job. It is kept under another name.
                let aside = crate::desktop::quarantine(&journal_in(state))?;
                tracing::warn!(%error, kept = %aside.display(), "Job journal was unreadable");
                Ok(vec![])
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(error) => Err(error.into()),
    }
}

/// Resolves the journal for `folder` by restoring the whole folder to ordinary
/// storage. `folder` must equal the journaled root exactly, on the same volume.
pub fn recover_folder(folder: &Path) -> Result<()> {
    let record = recovery()?
        .into_iter()
        .find(|record| record.root == folder)
        .context("No interrupted job for this folder")?;
    ensure!(
        crate::storage::volume(folder)?.identity == record.volume.identity,
        "Reconnect the original drive"
    );
    restore_folder_with(folder, &AtomicBool::new(false), |_| {})?;
    Ok(())
}

/// Walks a folder and applies `operation` to every regular file over 4096 bytes,
/// reporting running totals after each. Holds native.lock for the whole pass. A file
/// that fails is counted in `failed` and the pass goes on. Only a stop request or
/// a pass in which every file failed returns an error. The journal is removed
/// whichever way the pass ends, so only a crash leaves one behind. `operation`
/// says what it did to the file.
fn visit_with(
    root: &Path,
    cancel: &AtomicBool,
    restore: bool,
    operation: impl FnMut(&Path) -> Result<Outcome>,
    report: impl FnMut(Progress),
) -> Result<(Progress, Failures)> {
    visit_in(
        &crate::libraries::data_dir()?,
        root,
        cancel,
        restore,
        operation,
        report,
    )
}

/// `visit_with` with the state folder given, so tests keep clear of the real one.
fn visit_in(
    state: &Path,
    root: &Path,
    cancel: &AtomicBool,
    restore: bool,
    mut operation: impl FnMut(&Path) -> Result<Outcome>,
    mut report: impl FnMut(Progress),
) -> Result<(Progress, Failures)> {
    let root = root
        .canonicalize()
        .with_context(|| format!("Opening {}", root.display()))?;
    ensure!(root.is_dir(), "Choose an installed game folder");
    crate::desktop::ProtectedFolders::from_environment().check(&root)?;
    crate::libraries::private_dir(state)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join("native.lock"))?;
    ensure!(
        lock.try_lock().is_ok(),
        "Another Flummox process is working"
    );
    let volume = crate::storage::volume(&root)?;
    // A journal left by a crash blocks every folder except its own on its own
    // volume. That folder may be run again, in either direction.
    for record in recovery_in(state)? {
        ensure!(
            record.root == root && record.volume.identity == volume.identity,
            "Review the interrupted job before processing another game"
        );
    }
    crate::storage::per_file_plan(&root, restore)?.recheck()?;
    let mut journal = tempfile::NamedTempFile::new_in(state)?;
    serde_json::to_writer(
        &mut journal,
        &Recovery {
            root: root.clone(),
            restore,
            volume,
        },
    )?;
    journal.as_file().sync_all()?;
    journal.persist(journal_in(state))?;
    // Files Windows would not shrink are remembered per game folder, so an update
    // pass does not recompress them. Restoring neither reads nor writes the record.
    let rejected_path = state.join(format!(
        "wof-rejected-{}.json",
        blake3::hash(root.as_os_str().as_encoded_bytes()).to_hex()
    ));
    let previous = if restore {
        Rejected::default()
    } else {
        Rejected::load(&rejected_path)
    };
    let mut found = Rejected::default();
    let mut summary = Progress::default();
    let mut failures = Failures::default();
    let outcome = (|| -> Result<()> {
        for item in walkdir::WalkDir::new(&root).follow_links(false) {
            ensure!(!cancel.load(Ordering::Relaxed), "Operation stopped");
            let item = match item {
                Ok(item) => item,
                Err(error) => {
                    note_failure(&mut failures, &anyhow::Error::from(error));
                    continue;
                }
            };
            // `DirEntry::metadata` follows links. Check the directory entry first
            // so a file link cannot make an operation escape the selected tree.
            if !item.file_type().is_file() {
                continue;
            }
            let metadata = match item.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    note_failure(&mut failures, &anyhow::Error::from(error));
                    continue;
                }
            };
            if metadata.len() <= 4096 {
                continue;
            }
            summary.files = summary.files.saturating_add(1);
            summary.bytes = summary.bytes.saturating_add(metadata.len());
            let before = match allocation_size(item.path()) {
                Ok(before) => before,
                Err(error) => {
                    note_failure(&mut failures, &error);
                    report(summary.clone());
                    continue;
                }
            };
            summary.allocation_before = summary.allocation_before.saturating_add(before);
            let relative = item
                .path()
                .strip_prefix(&root)
                .unwrap_or_else(|_| item.path())
                .to_string_lossy()
                .into_owned();
            let known =
                !restore && previous.contains(&relative, metadata.len(), modified_nanos(&metadata));
            let result = if known {
                Ok(Outcome::Rejected)
            } else {
                operation(item.path())
            };
            let after = match result {
                Ok(outcome) => {
                    match outcome {
                        Outcome::Changed => summary.changed = summary.changed.saturating_add(1),
                        Outcome::Unchanged => summary.skipped = summary.skipped.saturating_add(1),
                        Outcome::Rejected => {
                            summary.skipped = summary.skipped.saturating_add(1);
                            // Read again: the record must match the file as it is now.
                            if let Ok(now) = std::fs::metadata(item.path()) {
                                found.insert(relative, now.len(), modified_nanos(&now));
                            }
                        }
                    }
                    allocation_size(item.path()).unwrap_or(before)
                }
                Err(error) => {
                    // A stop request surfaces as an error from the operation.
                    ensure!(!cancel.load(Ordering::Relaxed), "Operation stopped");
                    note_failure(&mut failures, &error);
                    before
                }
            };
            summary.allocation_after = summary.allocation_after.saturating_add(after);
            report(summary.clone());
        }
        ensure!(
            summary.files == 0 || failures.count < summary.files,
            "No file could be processed. {}",
            failures.first.as_deref().unwrap_or("")
        );
        Ok(())
    })();
    if !restore {
        // A stopped pass has not seen every file, so it keeps the older entries.
        let mut kept = if outcome.is_ok() {
            Rejected::default()
        } else {
            previous.clone()
        };
        kept.merge(found);
        if kept != previous
            && let Err(error) = kept.save(&rejected_path)
        {
            tracing::warn!(%error, "Could not save the list of files Windows would not shrink");
        }
    }
    let removed = std::fs::remove_file(journal_in(state)).context("Removing the job journal");
    match outcome {
        Ok(()) => removed.map(|()| (summary, failures)),
        Err(error) => {
            if let Err(removal) = removed {
                tracing::warn!(error = %removal, "Job journal stays after a failed pass");
            }
            Err(error)
        }
    }
}

/// Counts a failed file and keeps the first message for the result line.
fn note_failure(failures: &mut Failures, error: &anyhow::Error) {
    failures.count = failures.count.saturating_add(1);
    if failures.first.is_none() {
        failures.first = Some(format!("{error:#}"));
    }
}

/// Modification time in nanoseconds since 1970, or 0 when it is unavailable.
fn modified_nanos(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
        })
}

/// `visit_with` for the command line: no stop flag and no progress reports.
fn visit(
    root: &Path,
    restore: bool,
    operation: impl FnMut(&Path) -> Result<Outcome>,
) -> Result<(Progress, Failures)> {
    visit_with(root, &AtomicBool::new(false), restore, operation, |_| {})
}

/// The one-line result of a pass. Allocation can rise as well as fall, since a
/// restore gives space back to the files.
fn describe(summary: &Progress, failures: &Failures) -> String {
    let change = if summary.allocation_after <= summary.allocation_before {
        format!(
            " Freed {}.",
            humansize::format_size(
                summary.allocation_before - summary.allocation_after,
                humansize::DECIMAL
            )
        )
    } else {
        format!(
            " Restored {} of allocated storage.",
            humansize::format_size(
                summary.allocation_after - summary.allocation_before,
                humansize::DECIMAL
            )
        )
    };
    let failed = match &failures.first {
        Some(first) => format!(" {} could not be processed. First: {first}", failures.count),
        None => String::new(),
    };
    format!(
        "Processed {} files ({}): {} changed, {} already efficient.{}{}",
        summary.files,
        humansize::format_size(summary.bytes, humansize::DECIMAL),
        summary.changed,
        summary.skipped,
        change,
        failed
    )
}

/// Restores ordinary files while reporting progress and observing stop.
pub(crate) fn restore_folder_with(
    folder: &Path,
    cancel: &AtomicBool,
    report: impl FnMut(Progress),
) -> Result<String> {
    visit_with(folder, cancel, true, decompress_file, report)
        .map(|(summary, failures)| describe(&summary, &failures))
}

/// Runs the native Windows CLI.
pub fn run() -> Result<()> {
    let (summary, failures) = match Args::parse().command {
        Command::Analyze { folder, restore } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::storage::per_file_plan(
                    &folder, restore
                )?)?
            );
            return Ok(());
        }
        Command::Compress { folder, algorithm } => {
            visit(&folder, false, |path| compress_file(path, algorithm))?
        }
        Command::Decompress { folder } => visit(&folder, true, decompress_file)?,
    };
    println!("{}", describe(&summary, &failures));
    Ok(())
}

/// The coordinator's job entry: LZX compression or a restore, with a pause flag
/// checked before each file. A paused job sleeps in 50 ms steps and still ends
/// promptly when `cancel` is raised.
pub(crate) fn folder_controlled(
    folder: &Path,
    restore: bool,
    cancel: &AtomicBool,
    pause: &AtomicBool,
    report: impl FnMut(Progress),
) -> Result<String> {
    visit_with(
        folder,
        cancel,
        restore,
        |path| {
            while pause.load(Ordering::Relaxed) {
                ensure!(!cancel.load(Ordering::Relaxed), "Operation stopped");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            ensure!(!cancel.load(Ordering::Relaxed), "Operation stopped");
            if restore {
                decompress_file(path)
            } else {
                compress_file(path, Algorithm::Lzx)
            }
        },
        report,
    )
    .map(|(summary, failures)| describe(&summary, &failures))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    // A game folder with `count` files of 64 KiB, and a separate state folder.
    fn fixture(count: usize) -> std::result::Result<(tempfile::TempDir, PathBuf, PathBuf), String> {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let game = temp.path().join("game");
        let state = temp.path().join("state");
        std::fs::create_dir(&game).ctx("game folder")?;
        for index in 0..count {
            std::fs::write(game.join(format!("file{index}.dat")), vec![b'x'; 64 * 1024])
                .ctx("game file")?;
        }
        Ok((temp, game, state))
    }

    #[test]
    fn a_file_that_cannot_be_opened_is_counted_and_the_pass_continues() -> TestResult {
        let (_temp, game, state) = fixture(3)?;
        let stop = AtomicBool::new(false);
        // Control: with nothing held open, nothing fails.
        let (_, clean) = visit_in(
            &state,
            &game,
            &stop,
            false,
            |path| compress_file(path, Algorithm::Lzx),
            |_| {},
        )
        .ctx("clean pass")?;
        check_eq(clean.count, 0, "an unlocked folder has no failures")?;
        // An exclusive handle makes the next open of that file fail.
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(game.join("file1.dat"))
            .ctx("hold one file")?;
        let (summary, failures) = visit_in(&state, &game, &stop, true, decompress_file, |_| {})
            .ctx("pass with a held file")?;
        drop(held);
        check_eq(failures.count, 1, "only the held file failed")?;
        check_eq(summary.files, 3, "every file was visited")?;
        check(
            recovery_in(&state).ctx("journal")?.is_empty(),
            "the journal does not outlive a pass that ended normally",
        )
    }

    #[test]
    fn a_stop_or_total_failure_ends_the_pass_without_leaving_a_journal() -> TestResult {
        let (_temp, game, state) = fixture(2)?;
        let stopped = AtomicBool::new(true);
        check(
            visit_in(
                &state,
                &game,
                &stopped,
                false,
                |_| Ok(Outcome::Changed),
                |_| {},
            )
            .is_err(),
            "a stop request ends the pass",
        )?;
        check(
            recovery_in(&state).ctx("journal after stop")?.is_empty(),
            "a stop leaves no journal to block other games",
        )?;
        let running = AtomicBool::new(false);
        check(
            visit_in(
                &state,
                &game,
                &running,
                false,
                |_| Err(anyhow::anyhow!("denied")),
                |_| {},
            )
            .is_err(),
            "a pass where every file fails is an error",
        )?;
        check(
            recovery_in(&state).ctx("journal after failure")?.is_empty(),
            "a failed pass leaves no journal",
        )
    }

    #[test]
    fn files_windows_would_not_shrink_are_skipped_until_they_change() -> TestResult {
        let (_temp, game, state) = fixture(2)?;
        let stop = AtomicBool::new(false);
        visit_in(
            &state,
            &game,
            &stop,
            false,
            |_| Ok(Outcome::Rejected),
            |_| {},
        )
        .ctx("first pass")?;
        let mut calls = 0;
        visit_in(
            &state,
            &game,
            &stop,
            false,
            |_| {
                calls += 1;
                Ok(Outcome::Rejected)
            },
            |_| {},
        )
        .ctx("second pass")?;
        check_eq(calls, 0, "unchanged rejected files are not tried again")?;
        std::fs::write(game.join("file0.dat"), vec![b'y'; 70 * 1024]).ctx("update one file")?;
        let mut calls = 0;
        visit_in(
            &state,
            &game,
            &stop,
            false,
            |_| {
                calls += 1;
                Ok(Outcome::Rejected)
            },
            |_| {},
        )
        .ctx("third pass")?;
        check_eq(calls, 1, "only the file that changed is tried again")
    }

    #[test]
    fn allocation_falls_for_a_compressible_file_and_not_for_a_random_one() -> TestResult {
        let temp = tempfile::tempdir().ctx("fixture")?;
        let zeros = temp.path().join("zeros.dat");
        let noise = temp.path().join("noise.dat");
        std::fs::write(&zeros, vec![0u8; 1024 * 1024]).ctx("zeros")?;
        let mut random = vec![0u8; 1024 * 1024];
        blake3::Hasher::new()
            .update(b"fixture")
            .finalize_xof()
            .fill(&mut random);
        std::fs::write(&noise, &random).ctx("noise")?;
        let zeros_before = allocation_size(&zeros).ctx("zeros before")?;
        let noise_before = allocation_size(&noise).ctx("noise before")?;
        check(
            matches!(
                compress_file(&zeros, Algorithm::Lzx).ctx("zeros")?,
                Outcome::Changed
            ),
            "zeros compress",
        )?;
        compress_file(&noise, Algorithm::Lzx).ctx("noise")?;
        check(
            allocation_size(&zeros).ctx("zeros after")? < zeros_before,
            "AllocationSize falls when WOF compresses a file",
        )?;
        check_eq(
            allocation_size(&noise).ctx("noise after")?,
            noise_before,
            "AllocationSize holds for data that cannot shrink",
        )
    }
}
