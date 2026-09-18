//! Walks an install directory once and decides what to do with each file.
//!
//! The estimator and the compressor share this walk, so an estimate can never
//! describe a different set of files than the job that follows it.

use std::io;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// Default floor for the pack tier: below this, a file's own store object
/// costs more than compressing it saves.
pub const TINY_FILE_BYTES: u64 = 64 * 1024;

/// Floor for filesystems that compress in place.
///
/// There is no per-file cost there, so the only thing that cannot pay off is
/// a file too small to free a whole 4 KiB sector.
pub const NATIVE_TINY_FILE_BYTES: u64 = 4096;

/// Settings for a walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkOpts {
    /// Files at or below this size are skipped.
    pub min_size: u64,
}

impl Default for WalkOpts {
    fn default() -> Self {
        Self {
            min_size: TINY_FILE_BYTES,
        }
    }
}

impl WalkOpts {
    /// Walk settings for a backend that compresses files in place.
    pub fn native() -> Self {
        Self {
            min_size: NATIVE_TINY_FILE_BYTES,
        }
    }
}

/// The store directory a pack-tier game keeps inside its own install folder.
pub const STORE_DIR: &str = ".flummox";

/// One regular file in an install directory.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileEntry {
    /// Path relative to the install directory.
    #[serde(with = "crate::path_serde")]
    pub rel: PathBuf,
    /// Size in bytes.
    pub size: u64,
    /// Inode number, part of the fingerprint used to spot changed files.
    pub ino: u64,
    /// Modification time in nanoseconds.
    pub mtime_ns: i128,
    /// Inode change time in nanoseconds.
    pub ctime_ns: i128,
    /// What the planner decided to do with it.
    pub action: Action,
}

impl FileEntry {
    /// The absolute path, given the install directory it came from.
    pub fn path(&self, install_dir: &Path) -> PathBuf {
        install_dir.join(&self.rel)
    }

    /// Verifies the open file still has the identity captured by the walk.
    pub fn matches_file(&self, file: &std::fs::File) -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        Ok(meta.is_file()
            && self.size == meta.size()
            && self.ino == meta.ino()
            && self.mtime_ns
                == i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec())
            && self.ctime_ns
                == i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()))
    }

    /// Whether this file changed since the fingerprint was taken.
    pub fn changed_since(&self, other: &Self) -> bool {
        self.size != other.size
            || self.ino != other.ino
            || self.mtime_ns != other.mtime_ns
            || self.ctime_ns != other.ctime_ns
    }
}

/// What the planner decided about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Action {
    /// Compress it.
    Compress,
    /// Too small to be worth it.
    SkipTiny,
    /// Already compressed by its own format.
    SkipPrecompressed,
}

impl Action {
    /// Whether a job will touch this file.
    pub fn is_compress(self) -> bool {
        matches!(self, Self::Compress)
    }

    /// The reason shown to the user, for skipped files.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Compress => "compress",
            Self::SkipTiny => "too small",
            Self::SkipPrecompressed => "already compressed",
        }
    }
}

/// Everything one walk found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Every regular file, in walk order.
    pub files: Vec<FileEntry>,
    /// Directories that could not be read, with the reason.
    pub warnings: Vec<String>,
}

impl Inventory {
    /// Total size of every file found.
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Files the planner wants to compress.
    pub fn to_compress(&self) -> impl Iterator<Item = &FileEntry> {
        self.files.iter().filter(|f| f.action.is_compress())
    }

    /// Total size of the files to compress.
    pub fn compressible_bytes(&self) -> u64 {
        self.to_compress().map(|f| f.size).sum()
    }
}

/// Walks `install_dir`.
///
/// Symlinks are recorded but never followed, so a game that links into the
/// user's home cannot drag the walk outside its own directory. Special files
/// and the pack store are skipped.
pub fn walk(install_dir: &Path, opts: &WalkOpts) -> io::Result<Inventory> {
    walk_cancellable(install_dir, opts, None)
}

/// [`walk`], stoppable part way through.
///
/// A full install can hold half a million files, and stat-ing all of them on a
/// cold cache takes minutes. Cancelling returns [`io::ErrorKind::Interrupted`]
/// so a caller cannot mistake a partial walk for a complete one and compress
/// only the part that was seen.
pub fn walk_cancellable(
    install_dir: &Path,
    opts: &WalkOpts,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> io::Result<Inventory> {
    use std::os::unix::fs::MetadataExt;

    if !install_dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is not a directory", install_dir.display()),
        ));
    }
    let mut inv = Inventory::default();
    let walker = WalkDir::new(install_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != std::ffi::OsStr::new(STORE_DIR));
    for entry in walker {
        if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cancelled while walking",
            ));
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                inv.warnings.push(e.to_string());
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                inv.warnings.push(e.to_string());
                continue;
            }
        };
        let rel = match entry.path().strip_prefix(install_dir) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        let size = meta.size();
        let action = decide(&rel, size, opts);
        inv.files.push(FileEntry {
            rel,
            size,
            ino: meta.ino(),
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
            ctime_ns: i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
            action,
        });
    }
    Ok(inv)
}

/// Applies the backend size floor before content sampling.
///
/// Content-based checks happen later, during sampling: this is the cheap pass
/// that keeps the walk fast on a 60 GB install.
pub fn decide(_rel: &Path, size: u64, opts: &WalkOpts) -> Action {
    if size <= opts.min_size {
        return Action::SkipTiny;
    }
    Action::Compress
}

/// Extensions commonly associated with encoded content.
///
/// Game archives such as `.pak` are absent: some are compressed
/// and some are not, so sampling decides those.
const PRECOMPRESSED_EXTENSIONS: &[&str] = &[
    // Archives and containers
    "zst", "xz", "gz", "bz2", "7z", "rar", "zip", "lz4", "lzma", "cab", "wim", // Video
    "mp4", "m4v", "mkv", "webm", "avi", "mov", "bik", "bk2", "usm", "wmv", "ogv",
    // Audio
    "mp3", "ogg", "oga", "opus", "flac", "aac", "m4a", "wem", "fsb", "bnk", "xwb",
    // Images
    "jpg", "jpeg", "png", "gif", "webp", "avif", "jxl", "ktx", "ktx2", "basis", "dds",
    // Fonts and misc already-deflated formats
    "woff", "woff2",
];

/// Whether a path's extension hints at encoded content. Sampling decides eligibility.
pub fn is_precompressed_name(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| PRECOMPRESSED_EXTENSIONS.contains(&ext.as_str()))
}

/// Whether a file's leading bytes identify an already-compressed format.
///
/// Used on the sampled head of a file, to catch archives with game-specific
/// extensions.
pub fn is_precompressed_magic(head: &[u8]) -> bool {
    const MAGICS: &[&[u8]] = &[
        &[0x28, 0xB5, 0x2F, 0xFD],       // zstd
        &[0xFD, b'7', b'z', b'X', b'Z'], // xz
        &[0x1F, 0x8B],                   // gzip
        b"BZh",                          // bzip2
        &[b'P', b'K', 0x03, 0x04],       // zip
        &[b'7', b'z', 0xBC, 0xAF],       // 7z
        b"Rar!",                         // rar
        &[0x04, 0x22, 0x4D, 0x18],       // lz4 frame
        &[0xFF, 0xD8, 0xFF],             // jpeg
        &[0x89, b'P', b'N', b'G'],       // png
        b"OggS",                         // ogg
        b"fLaC",                         // flac
        b"KB2f",                         // bink 2
    ];
    MAGICS.iter().any(|m| head.starts_with(m))
}

#[cfg(test)]
mod tests {
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    use super::*;

    #[test]
    fn walks_a_tree_and_classifies_files() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("data")).ctx("create data/")?;
        std::fs::write(dir.join("data/big.dat"), vec![0u8; 200 * 1024]).ctx("write big.dat")?;
        std::fs::write(dir.join("data/movie.bik"), vec![0u8; 200 * 1024]).ctx("write movie.bik")?;
        std::fs::write(dir.join("small.cfg"), b"x").ctx("write small.cfg")?;
        // The pack store is never part of an inventory.
        std::fs::create_dir_all(dir.join(STORE_DIR)).ctx("create the store dir")?;
        std::fs::write(
            dir.join(STORE_DIR).join("manifest.gcm"),
            vec![0u8; 100 * 1024],
        )
        .ctx("write the store manifest")?;

        let inv = walk(dir, &WalkOpts::default()).ctx("walk the tree")?;
        let mut names: Vec<String> = inv
            .files
            .iter()
            .map(|f| f.rel.display().to_string())
            .collect();
        names.sort();
        let expected: Vec<String> = ["data/big.dat", "data/movie.bik", "small.cfg"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        check_eq(names, expected, "the files the walk found")?;

        let by = |name: &str| -> Result<Action, String> {
            inv.files
                .iter()
                .find(|f| f.rel == Path::new(name))
                .map(|f| f.action)
                .ctx(format!("no inventory entry for {name}"))
        };
        check_eq(
            by("data/big.dat")?,
            Action::Compress,
            "a big plain file is compressed",
        )?;
        check_eq(
            by("data/movie.bik")?,
            Action::Compress,
            "a video name alone cannot exclude its contents",
        )?;
        check_eq(by("small.cfg")?, Action::SkipTiny, "a tiny file is skipped")?;
        check_eq(inv.compressible_bytes(), 400 * 1024, "compressible bytes")?;
        check_eq(inv.total_bytes(), 400 * 1024 + 1, "total bytes")
    }

    #[test]
    fn does_not_follow_symlinks_out_of_the_tree() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).ctx("create outside/")?;
        std::fs::write(outside.join("secret.dat"), vec![0u8; 200 * 1024])
            .ctx("write secret.dat")?;
        let game = tmp.path().join("game");
        std::fs::create_dir_all(&game).ctx("create game/")?;
        std::os::unix::fs::symlink(&outside, game.join("link")).ctx("symlink into outside/")?;

        let inv = walk(&game, &WalkOpts::default()).ctx("walk the game dir")?;
        check(
            inv.files.is_empty(),
            format!("symlinked tree must not be walked: {inv:?}"),
        )
    }

    #[test]
    fn the_native_floor_keeps_files_a_pack_store_would_skip() -> TestResult {
        let tmp = tempfile::tempdir().ctx("tempdir")?;
        let dir = tmp.path();
        // 32 KiB: too small to be worth its own pack object, but on btrfs it
        // still frees whole sectors.
        std::fs::write(dir.join("mid.dat"), vec![b'a'; 32 * 1024]).ctx("write mid.dat")?;
        std::fs::write(dir.join("sector.dat"), vec![b'a'; 2048]).ctx("write sector.dat")?;

        let pack = walk(dir, &WalkOpts::default()).ctx("walk with the pack floor")?;
        check_eq(
            pack.compressible_bytes(),
            0,
            "the pack floor skips both files",
        )?;

        let native = walk(dir, &WalkOpts::native()).ctx("walk with the native floor")?;
        check_eq(
            native.compressible_bytes(),
            32 * 1024,
            "the native floor keeps the 32 KiB file",
        )
    }

    #[test]
    fn fingerprints_detect_a_rewritten_file() -> TestResult {
        let a = FileEntry {
            rel: PathBuf::from("x"),
            size: 10,
            ino: 1,
            mtime_ns: 5,
            ctime_ns: 5,
            action: Action::Compress,
        };
        check(
            !a.changed_since(&a.clone()),
            "an identical fingerprint is unchanged",
        )?;
        let b = FileEntry {
            mtime_ns: 6,
            ..a.clone()
        };
        check(b.changed_since(&a), "a newer mtime counts as changed")
    }

    #[test]
    fn magic_bytes_catch_archives_with_game_extensions() -> TestResult {
        check(
            is_precompressed_magic(&[0x28, 0xB5, 0x2F, 0xFD, 0, 0]),
            "zstd magic",
        )?;
        check(is_precompressed_magic(b"OggS...."), "ogg magic")?;
        check(
            !is_precompressed_magic(b"RIFF...."),
            "RIFF is not a compressed container",
        )?;
        check(
            is_precompressed_name(Path::new("a/b/c.BIK")),
            "a .BIK name, case-insensitively",
        )?;
        check(
            !is_precompressed_name(Path::new("a/b/c.pak")),
            "a .pak name is left to sampling",
        )
    }
}
