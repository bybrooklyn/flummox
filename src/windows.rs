//! Native Windows compression entrypoint.
//!
//! WOF keeps logical file bytes and paths unchanged. Writes expand a file, so
//! the maintenance pass can safely reapply compression after launcher updates.

#![allow(unsafe_code)]

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::Read,
    os::windows::io::AsRawHandle,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use windows_sys::Win32::{
    Foundation::{ERROR_COMPRESSION_NOT_BENEFICIAL, GetLastError, HANDLE},
    Storage::FileSystem::{
        FILE_PROVIDER_COMPRESSION_LZX, FILE_PROVIDER_COMPRESSION_XPRESS4K,
        FILE_PROVIDER_COMPRESSION_XPRESS8K, FILE_PROVIDER_COMPRESSION_XPRESS16K,
        FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx,
        WOF_FILE_COMPRESSION_INFO_V1, WOF_PROVIDER_FILE, WofSetFileDataLocation,
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
    /// Restore ordinary NTFS storage for a folder tree.
    Decompress { folder: PathBuf },
}

#[derive(Debug, Clone)]
pub(crate) struct InstalledGame {
    pub title: String,
    pub path: PathBuf,
}

fn bounded_text(path: &Path, limit: u64) -> Result<String> {
    let file = std::fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() <= limit,
        "Launcher manifest is too large"
    );
    let mut text = String::new();
    file.take(limit.saturating_add(1))
        .read_to_string(&mut text)?;
    ensure!(text.len() as u64 <= limit, "Launcher manifest is too large");
    Ok(text)
}

fn default_steamapps() -> BTreeSet<PathBuf> {
    [
        std::env::var_os("ProgramFiles(x86)"),
        std::env::var_os("ProgramFiles"),
    ]
    .into_iter()
    .flatten()
    .map(PathBuf::from)
    .map(|root| root.join("Steam").join("steamapps"))
    .filter(|path| path.is_dir())
    .collect()
}

/// Finds installed Steam games without contacting Steam or the network.
pub(crate) fn discover_steam() -> Vec<InstalledGame> {
    let mut libraries = default_steamapps();
    let roots: Vec<_> = libraries.iter().cloned().collect();
    for steamapps in roots {
        let Ok(text) = bounded_text(&steamapps.join("libraryfolders.vdf"), 16 * 1024 * 1024) else {
            continue;
        };
        let Ok(document) = crate::windows_vdf::parse(&text) else {
            continue;
        };
        for (_, value) in document.entries() {
            let library = match value {
                crate::windows_vdf::Value::Str(path) => Some(path.as_str()),
                crate::windows_vdf::Value::Obj(object) => object.get_str("path"),
            };
            if let Some(path) = library {
                let steamapps = PathBuf::from(path).join("steamapps");
                if steamapps.is_dir() {
                    libraries.insert(steamapps);
                }
            }
        }
    }
    let mut games = Vec::new();
    let mut seen = BTreeSet::new();
    for library in libraries {
        let Ok(entries) = std::fs::read_dir(&library) else {
            continue;
        };
        for item in entries.flatten() {
            let name = item.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("appmanifest_") || !name.ends_with(".acf") {
                continue;
            }
            let Ok(text) = bounded_text(&item.path(), 4 * 1024 * 1024) else {
                continue;
            };
            let Ok(manifest) = crate::windows_vdf::parse(&text) else {
                continue;
            };
            let (Some(title), Some(folder)) =
                (manifest.get_str("name"), manifest.get_str("installdir"))
            else {
                continue;
            };
            let common = library.join("common");
            let path = common.join(folder);
            let Ok(path) = path.canonicalize() else {
                continue;
            };
            let Ok(common) = common.canonicalize() else {
                continue;
            };
            if path.is_dir() && path.starts_with(common) && seen.insert(path.clone()) {
                games.push(InstalledGame {
                    title: title.to_owned(),
                    path,
                });
            }
        }
    }
    games.sort_by(|left, right| left.title.to_lowercase().cmp(&right.title.to_lowercase()));
    games
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
        .open(path)
        .with_context(|| format!("Opening {}", path.display()))
}

fn allocation_size(path: &Path) -> Result<u64> {
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

fn visit_with(
    root: &Path,
    cancel: &AtomicBool,
    mut operation: impl FnMut(&Path) -> Result<bool>,
    mut report: impl FnMut(Progress),
) -> Result<Progress> {
    let root = root
        .canonicalize()
        .with_context(|| format!("Opening {}", root.display()))?;
    ensure!(root.is_dir(), "Choose an installed game folder");
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
    Ok(summary)
}

fn visit(root: &Path, operation: impl FnMut(&Path) -> Result<bool>) -> Result<Progress> {
    visit_with(root, &AtomicBool::new(false), operation, |_| {})
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

/// Applies LZX while reporting measured allocation changes and observing stop.
pub(crate) fn optimize_folder_with(
    folder: &Path,
    cancel: &AtomicBool,
    report: impl FnMut(Progress),
) -> Result<String> {
    visit_with(
        folder,
        cancel,
        |path| compress_file(path, Algorithm::Lzx),
        report,
    )
    .map(|summary| describe(&summary))
}

/// Restores ordinary files while reporting progress and observing stop.
pub(crate) fn restore_folder_with(
    folder: &Path,
    cancel: &AtomicBool,
    report: impl FnMut(Progress),
) -> Result<String> {
    visit_with(folder, cancel, decompress_file, report).map(|summary| describe(&summary))
}

/// Runs the native Windows CLI.
pub fn run() -> Result<()> {
    let summary = match Args::parse().command {
        Command::Compress { folder, algorithm } => {
            visit(&folder, |path| compress_file(path, algorithm))?
        }
        Command::Decompress { folder } => visit(&folder, decompress_file)?,
    };
    println!("{}", describe(&summary));
    Ok(())
}
