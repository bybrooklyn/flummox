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

#[derive(Debug, Clone, Default)]
pub(crate) struct Progress {
    pub files: u64,
    pub changed: u64,
    pub skipped: u64,
    pub bytes: u64,
    pub allocation_before: u64,
    pub allocation_after: u64,
}

fn hresult_from_win32(code: u32) -> i32 {
    (0x8007_0000u32 | code) as i32
}

fn file_handle(path: &Path) -> Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path)
        .with_context(|| format!("Opening {}", path.display()))
}

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

fn compress_file(path: &Path, algorithm: Algorithm) -> Result<bool> {
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut external = 0;
    let mut provider = 0;
    let mut existing = WOF_FILE_COMPRESSION_INFO_V1 {
        Algorithm: 0,
        Flags: 0,
    };
    let mut size = u32::try_from(std::mem::size_of_val(&existing))?;
    // SAFETY: name is terminated and all output buffers have their exact writable sizes.
    let query = unsafe {
        WofIsExternalFile(
            name.as_ptr(),
            &mut external,
            &mut provider,
            std::ptr::from_mut(&mut existing).cast(),
            &mut size,
        )
    };
    ensure!(
        query >= 0,
        "Windows could not query existing compression for {}",
        path.display()
    );
    ensure!(
        external == 0
            || provider != WOF_PROVIDER_FILE
            || size == u32::try_from(std::mem::size_of_val(&existing))?,
        "Windows returned unfamiliar compression metadata"
    );
    if external != 0 && provider == WOF_PROVIDER_FILE && existing.Algorithm == algorithm.code() {
        return Ok(false);
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
        return Ok(true);
    }
    if result == hresult_from_win32(ERROR_COMPRESSION_NOT_BENEFICIAL) {
        return Ok(false);
    }
    anyhow::bail!(
        "Windows rejected compression for {} with HRESULT 0x{:08x}",
        path.display(),
        result as u32
    )
}

fn decompress_file(path: &Path) -> Result<bool> {
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
        return Ok(true);
    }
    // A file without external backing is already restored.
    // SAFETY: GetLastError reads this thread's Win32 error slot and takes no pointers.
    let error = unsafe { GetLastError() };
    if matches!(error, 1 | 50 | 4390) {
        return Ok(false);
    }
    anyhow::bail!(
        "Windows rejected decompression for {} with error {}",
        path.display(),
        error
    )
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Recovery {
    #[serde(with = "crate::path_serde")]
    pub root: PathBuf,
    pub restore: bool,
    pub volume: crate::storage::Volume,
}

fn journal_path() -> Result<PathBuf> {
    Ok(crate::libraries::data_dir()?.join("windows-job.json"))
}

pub fn recovery() -> Result<Vec<Recovery>> {
    match std::fs::read(journal_path()?) {
        Ok(bytes) => Ok(vec![serde_json::from_slice(&bytes)?]),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(error) => Err(error.into()),
    }
}

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

fn visit_with(
    root: &Path,
    cancel: &AtomicBool,
    restore: bool,
    mut operation: impl FnMut(&Path) -> Result<bool>,
    mut report: impl FnMut(Progress),
) -> Result<Progress> {
    let root = root
        .canonicalize()
        .with_context(|| format!("Opening {}", root.display()))?;
    ensure!(root.is_dir(), "Choose an installed game folder");
    let state = crate::libraries::data_dir()?;
    crate::libraries::private_dir(&state)?;
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
    for record in recovery()? {
        ensure!(
            record.root == root && record.volume.identity == volume.identity,
            "Review the interrupted job before processing another game"
        );
    }
    crate::storage::native_plan(&root, restore)?.recheck()?;
    let mut journal = tempfile::NamedTempFile::new_in(&state)?;
    serde_json::to_writer(
        &mut journal,
        &Recovery {
            root: root.clone(),
            restore,
            volume,
        },
    )?;
    journal.as_file().sync_all()?;
    journal.persist(journal_path()?)?;
    let mut summary = Progress::default();
    for item in walkdir::WalkDir::new(&root).follow_links(false) {
        ensure!(!cancel.load(Ordering::Relaxed), "Operation stopped");
        let item = item?;
        // `DirEntry::metadata` follows links. Check the directory entry first
        // so a file link cannot make an operation escape the selected tree.
        if !item.file_type().is_file() {
            continue;
        }
        let metadata = item.metadata()?;
        if metadata.len() <= 4096 {
            continue;
        }
        summary.files = summary.files.saturating_add(1);
        summary.bytes = summary.bytes.saturating_add(metadata.len());
        summary.allocation_before = summary
            .allocation_before
            .saturating_add(allocation_size(item.path())?);
        if operation(item.path())? {
            summary.changed = summary.changed.saturating_add(1);
        } else {
            summary.skipped = summary.skipped.saturating_add(1);
        }
        summary.allocation_after = summary
            .allocation_after
            .saturating_add(allocation_size(item.path())?);
        report(summary.clone());
    }
    std::fs::remove_file(journal_path()?)?;
    Ok(summary)
}

fn visit(
    root: &Path,
    restore: bool,
    operation: impl FnMut(&Path) -> Result<bool>,
) -> Result<Progress> {
    visit_with(root, &AtomicBool::new(false), restore, operation, |_| {})
}

fn describe(summary: &Progress) -> String {
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
    format!(
        "Processed {} files ({}): {} changed, {} already efficient.{}",
        summary.files,
        humansize::format_size(summary.bytes, humansize::DECIMAL),
        summary.changed,
        summary.skipped,
        change
    )
}

/// Restores ordinary files while reporting progress and observing stop.
pub(crate) fn restore_folder_with(
    folder: &Path,
    cancel: &AtomicBool,
    report: impl FnMut(Progress),
) -> Result<String> {
    visit_with(folder, cancel, true, decompress_file, report).map(|summary| describe(&summary))
}

/// Runs the native Windows CLI.
pub fn run() -> Result<()> {
    let summary = match Args::parse().command {
        Command::Analyze { folder, restore } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::storage::native_plan(&folder, restore)?)?
            );
            return Ok(());
        }
        Command::Compress { folder, algorithm } => {
            visit(&folder, false, |path| compress_file(path, algorithm))?
        }
        Command::Decompress { folder } => visit(&folder, true, decompress_file)?,
    };
    println!("{}", describe(&summary));
    Ok(())
}

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
    .map(|summary| describe(&summary))
}
