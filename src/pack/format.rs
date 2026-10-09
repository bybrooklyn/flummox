//! Bounds and validation for untrusted store indexes and compressed chunks.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Largest decoded chunk, and the largest single `Reader::read`.
pub const CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// Smallest content-defined chunk that is not the last chunk of its file.
pub(super) const MIN_CHUNK_BYTES: usize = 512 * 1024;
/// Fixed header at the start of a store file or a directory store's manifest.
pub(super) const HEADER_BYTES: u64 = 64;
/// Largest encoded index a reader allocates memory for.
pub(super) const MAX_INDEX: u64 = 32 * 1024 * 1024;
/// Most entries an index may hold. `validate` checks this before walking them.
pub(super) const MAX_ENTRIES: usize = 100_000;
/// Most unique chunks an index may hold.
pub(super) const MAX_CHUNKS: usize = 1_000_000;
/// First eight bytes of a store file or manifest.
pub(super) const MAGIC: &[u8; 8] = b"FLUMPK01";
/// First eight bytes of a chunk object in a directory store or pool.
pub(super) const OBJECT_MAGIC: &[u8; 8] = b"FLUMCH01";
/// Header bytes before the payload of a chunk object.
pub(super) const OBJECT_HEADER: u64 = 64;
// Decoded chunks one reader keeps: 8 chunks of at most 4 MiB, so 32 MiB.
const CACHE_CHUNKS: usize = 8;
const MAX_XATTRS_PER_ENTRY: usize = 256;
const MAX_XATTR_BYTES_PER_ENTRY: usize = 1024 * 1024;

/// One extended attribute as raw bytes. Creation stores an entry's list sorted by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Xattr {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

/// One path in the store. The first entry is the root directory, with an empty path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Relative to the game folder. Unix filename bytes survive encoding unchanged.
    #[serde(with = "crate::path_serde")]
    pub path: PathBuf,
    /// Permission bits. Versions before 4 allow only the low nine.
    pub mode: u32,
    pub modified_secs: i64,
    pub modified_nanos: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub xattrs: Vec<Xattr>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::path_serde::option"
    )]
    /// Earlier entry this file shares an inode with. The alias repeats that
    /// entry's kind and metadata, and the target is never itself an alias.
    pub hardlink_to: Option<PathBuf>,
    pub kind: Kind,
}

/// What an entry holds. Chunk numbers are positions in `Index::chunks`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Kind {
    Directory,
    /// A file made of whole chunks, listed in file order.
    File {
        size: u64,
        chunks: Vec<u32>,
    },
    /// One file inside a shared, independently decoded frame.
    SlicedFile {
        size: u64,
        chunk: u32,
        /// Where this file starts inside the decoded frame.
        offset: u32,
    },
    Symlink {
        #[serde(with = "crate::path_serde")]
        target: PathBuf,
    },
}

impl Kind {
    /// Logical length of a regular file. `None` for directories and symlinks.
    pub fn size(&self) -> Option<u64> {
        match self {
            Self::File { size, .. } | Self::SlicedFile { size, .. } => Some(*size),
            Self::Directory | Self::Symlink { .. } => None,
        }
    }
}

/// How a chunk's payload is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Codec {
    /// Stored as is, because zstd did not make it smaller.
    Raw,
    Zstd,
    /// Every decoded byte is zero. No payload is stored.
    Zero,
}

/// Descriptor of one unique chunk. `chunk_name` derives an object's file name from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Chunk {
    /// Byte position in a monolithic store. Always 0 in a directory store.
    pub offset: u64,
    /// Encoded payload length.
    pub stored: u32,
    /// Decoded length.
    pub raw: u32,
    pub codec: Codec,
    /// BLAKE3 of the decoded bytes.
    pub hash: [u8; 32],
}

/// The JSON index. Entries are in parent-before-child order and refer to
/// chunks by position, so neither list can be reordered on its own.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Index {
    pub entries: Vec<Entry>,
    pub chunks: Vec<Chunk>,
}

/// Contents of a directory store's `pool.json`: the pool its objects were linked from.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PoolRecord {
    #[serde(with = "crate::path_serde")]
    pub path: PathBuf,
}

/// Serialized sizes include the header, index, and all unique payload bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub files: u64,
    /// Sum of the sizes of all regular files, counting each hard-link alias.
    pub logical_bytes: u64,
    pub archive_bytes: u64,
    /// Header and index, plus `pool.json` and object headers in a directory store.
    pub metadata_bytes: u64,
    /// Decoded bytes of the unique chunks stored raw.
    #[serde(default)]
    pub raw_bytes: u64,
    /// Decoded bytes of the unique chunks stored as zstd.
    #[serde(default)]
    pub compressed_input_bytes: u64,
    /// Encoded bytes of those zstd chunks.
    #[serde(default)]
    pub compressed_bytes: u64,
    #[serde(default)]
    pub zero_bytes: u64,
    /// Object bytes in this store that another store also hard-links.
    #[serde(default)]
    pub shared_bytes: u64,
    pub unique_chunks: u64,
    /// `logical_bytes` minus the decoded bytes of all unique chunks.
    pub duplicate_bytes: u64,
}

impl Index {
    /// Checks every structural rule the reader relies on and returns the path table.
    /// `payload_end` is the index offset of a monolithic store. Directory stores pass 0.
    /// A matching index checksum does not replace this: it must run before any read.
    pub(super) fn validate(
        &self,
        payload_end: u64,
        version: u32,
    ) -> Result<BTreeMap<PathBuf, usize>> {
        ensure!(matches!(version, 1..=7), "Unsupported store version");
        // Versions 3, 5 and 7 are directory stores, whose chunks are separate
        // objects with no offset. Versions from 4 allow xattrs, hard links and
        // the setuid, setgid and sticky bits.
        let external_chunks = matches!(version, 3 | 5 | 7);
        let extended_metadata = version >= 4;
        ensure!(
            !self.entries.is_empty() && self.entries.len() <= MAX_ENTRIES,
            "Invalid entry count"
        );
        ensure!(self.chunks.len() <= MAX_CHUNKS, "Too many chunks");
        // In a monolithic store the payloads must tile the bytes between the
        // header and the index in chunk order: no gap, overlap or trailing data.
        let mut next = HEADER_BYTES;
        for chunk in &self.chunks {
            ensure!(
                chunk.raw > 0 && chunk.raw as usize <= CHUNK_BYTES,
                "Invalid decoded chunk length"
            );
            if external_chunks {
                ensure!(
                    chunk.offset == 0,
                    "Shared chunks cannot have archive offsets"
                );
            } else {
                ensure!(chunk.offset == next, "Noncontiguous chunk index");
            }
            match chunk.codec {
                Codec::Raw => ensure!(chunk.stored == chunk.raw, "Invalid raw chunk length"),
                Codec::Zstd => ensure!(
                    chunk.stored > 0 && chunk.stored < chunk.raw,
                    "Invalid compressed length"
                ),
                Codec::Zero => ensure!(chunk.stored == 0, "Invalid zero chunk length"),
            }
            if !external_chunks {
                next = next
                    .checked_add(u64::from(chunk.stored))
                    .context("Chunk offset overflow")?;
                ensure!(next <= payload_end, "Chunk extends into the index");
            }
        }
        if !external_chunks {
            ensure!(next == payload_end, "Unindexed payload bytes");
        }
        let mut paths = BTreeMap::new();
        let mut used = vec![false; self.chunks.len()];
        let mut total = 0u64;
        let mut slices = HashMap::<u32, Vec<(u32, u32)>>::new();
        // `paths` holds only the entries accepted so far, so a parent or a
        // hard-link target is found only when it comes earlier in the index.
        for (number, entry) in self.entries.iter().enumerate() {
            let mode_mask = if extended_metadata { 0o7777 } else { 0o777 };
            ensure!(
                entry.mode & !mode_mask == 0 && entry.modified_nanos < 1_000_000_000,
                "Invalid file metadata"
            );
            // Restoring would create a file that runs with the restoring
            // user's identity, from an index anyone could have written.
            ensure!(
                entry.mode & 0o6000 == 0
                    || !matches!(entry.kind, Kind::File { .. } | Kind::SlicedFile { .. }),
                "The store holds a setuid or setgid file"
            );
            ensure!(
                extended_metadata || (entry.xattrs.is_empty() && entry.hardlink_to.is_none()),
                "Extended metadata requires a newer store version"
            );
            ensure!(
                entry.xattrs.len() <= MAX_XATTRS_PER_ENTRY,
                "Too many extended attributes"
            );
            let mut xattr_bytes = 0usize;
            let mut xattr_names = std::collections::HashSet::new();
            for attribute in &entry.xattrs {
                ensure!(
                    !attribute.name.is_empty()
                        && attribute.name.len() <= 255
                        && !attribute.name.contains(&0),
                    "Invalid extended attribute name"
                );
                ensure!(
                    xattr_names.insert(attribute.name.as_slice()),
                    "Duplicate extended attribute name"
                );
                xattr_bytes = xattr_bytes
                    .checked_add(attribute.name.len())
                    .and_then(|size| size.checked_add(attribute.value.len()))
                    .context("Extended attribute size overflow")?;
            }
            ensure!(
                xattr_bytes <= MAX_XATTR_BYTES_PER_ENTRY,
                "Extended attributes exceed the per-entry limit"
            );
            if number == 0 {
                ensure!(
                    entry.path.as_os_str().is_empty() && matches!(entry.kind, Kind::Directory),
                    "Missing store root"
                );
            } else {
                ensure!(
                    safe_path(&entry.path),
                    "Unsafe store path: {}",
                    entry.path.display()
                );
                let parent = entry.path.parent().context("Missing parent")?;
                let parent_index = paths
                    .get(parent)
                    .copied()
                    .context("Parent must precede its children")?;
                let parent_entry: &Entry =
                    self.entries.get(parent_index).context("Invalid parent")?;
                ensure!(
                    matches!(parent_entry.kind, Kind::Directory),
                    "Parent is not a directory"
                );
            }
            ensure!(
                paths.insert(entry.path.clone(), number).is_none(),
                "Duplicate store path"
            );
            if let Kind::File { size, chunks } = &entry.kind {
                if let Some(target) = &entry.hardlink_to {
                    ensure!(safe_path(target), "Invalid hard-link target");
                    let target_id = paths
                        .get(target)
                        .copied()
                        .context("Hard-link target must precede its alias")?;
                    let target_entry = self
                        .entries
                        .get(target_id)
                        .context("Missing hard-link target")?;
                    ensure!(
                        matches!(
                            &target_entry.kind,
                            Kind::File {
                                size: target_size,
                                chunks: target_chunks
                            } if target_size == size && target_chunks == chunks
                        ) && target_entry.hardlink_to.is_none()
                            && target_entry.mode == entry.mode
                            && target_entry.modified_secs == entry.modified_secs
                            && target_entry.modified_nanos == entry.modified_nanos
                            && target_entry.xattrs == entry.xattrs,
                        "Hard-link metadata differs from its target"
                    );
                }
                let mut remaining = *size;
                // Version 1 cut files every CHUNK_BYTES. Later versions cut on
                // content, where every chunk but the last is at least
                // MIN_CHUNK_BYTES, which bounds the count.
                if version == 1 {
                    ensure!(
                        chunks.len() as u64 == size.div_ceil(CHUNK_BYTES as u64),
                        "Invalid file chunk count"
                    );
                } else {
                    ensure!(
                        chunks.len() as u64 <= size.div_ceil(MIN_CHUNK_BYTES as u64),
                        "Invalid content-defined chunk count"
                    );
                }
                for (ordinal, id) in chunks.iter().enumerate() {
                    let chunk = self.chunks.get(*id as usize).context("Missing chunk")?;
                    *used.get_mut(*id as usize).context("Missing chunk usage")? = true;
                    if version == 1 {
                        ensure!(
                            u64::from(chunk.raw) == remaining.min(CHUNK_BYTES as u64),
                            "Incorrect file chunk length"
                        );
                    } else {
                        ensure!(
                            ordinal + 1 == chunks.len() || chunk.raw as usize >= MIN_CHUNK_BYTES,
                            "Content-defined chunk is too small"
                        );
                        ensure!(
                            u64::from(chunk.raw) <= remaining,
                            "Content-defined chunks exceed the file"
                        );
                    }
                    remaining -= u64::from(chunk.raw);
                }
                ensure!(remaining == 0, "File chunks do not cover its size");
                total = total.checked_add(*size).context("Install size overflow")?;
            }
            if let Kind::SlicedFile {
                size,
                chunk,
                offset,
            } = &entry.kind
            {
                ensure!(
                    version >= 6,
                    "Shared file frames require a newer store version"
                );
                ensure!(
                    *size > 0 && *size < MIN_CHUNK_BYTES as u64,
                    "Invalid shared file length"
                );
                let frame = self.chunks.get(*chunk as usize).context("Missing frame")?;
                if let Some(target) = &entry.hardlink_to {
                    ensure!(safe_path(target), "Invalid hard-link target");
                    let target_id = paths
                        .get(target)
                        .copied()
                        .context("Hard-link target must precede its alias")?;
                    let target_entry = self
                        .entries
                        .get(target_id)
                        .context("Missing hard-link target")?;
                    ensure!(
                        matches!(
                            &target_entry.kind,
                            Kind::SlicedFile {
                                size: target_size,
                                chunk: target_chunk,
                                offset: target_offset
                            } if target_size == size
                                && target_chunk == chunk
                                && target_offset == offset
                        ) && target_entry.hardlink_to.is_none()
                            && target_entry.mode == entry.mode
                            && target_entry.modified_secs == entry.modified_secs
                            && target_entry.modified_nanos == entry.modified_nanos
                            && target_entry.xattrs == entry.xattrs,
                        "Hard-link metadata differs from its target"
                    );
                }
                let end = u64::from(*offset)
                    .checked_add(*size)
                    .context("Shared file range overflow")?;
                ensure!(end <= u64::from(frame.raw), "Shared file exceeds its frame");
                *used
                    .get_mut(*chunk as usize)
                    .context("Missing frame usage")? = true;
                slices
                    .entry(*chunk)
                    .or_default()
                    .push((*offset, u32::try_from(end)?));
                total = total.checked_add(*size).context("Install size overflow")?;
            }
            ensure!(
                entry.hardlink_to.is_none()
                    || matches!(entry.kind, Kind::File { .. } | Kind::SlicedFile { .. }),
                "Only regular files can be hard links"
            );
        }
        // The slices of a shared frame must cover it end to end. A hard-link
        // alias repeats its target's range, so equal ranges are merged first.
        for (id, mut ranges) in slices {
            ranges.sort_unstable();
            ranges.dedup();
            let mut covered = 0u32;
            for (start, end) in ranges {
                ensure!(start == covered, "Shared frame has a gap or overlap");
                covered = end;
            }
            let frame = self
                .chunks
                .get(id as usize)
                .context("Missing shared frame")?;
            ensure!(covered == frame.raw, "Shared frame is not fully mapped");
        }
        ensure!(
            used.iter().all(|used| *used),
            "Unreferenced chunk in store index"
        );
        // Links are checked once every path is known: a target may name a later entry.
        for entry in &self.entries {
            if let Kind::Symlink { target } = &entry.kind {
                self.check_link(&entry.path, target, &paths)?;
            }
        }
        Ok(paths)
    }

    /// Walks `target` from the link's folder through the index, expanding each
    /// link it meets. Fails on an absolute target, a step above the root, or
    /// more than 40 expansions. A target that names no entry is accepted.
    fn check_link(
        &self,
        path: &Path,
        target: &Path,
        paths: &BTreeMap<PathBuf, usize>,
    ) -> Result<()> {
        ensure!(
            !target.as_os_str().is_empty()
                && target.as_os_str().as_encoded_bytes().len() <= 4096
                && !target.as_os_str().as_encoded_bytes().contains(&0),
            "Invalid symlink target"
        );
        let joined = path.parent().unwrap_or(Path::new("")).join(target);
        let mut pending: VecDeque<_> = joined
            .components()
            .map(|c| c.as_os_str().to_os_string())
            .collect();
        let mut resolved = PathBuf::new();
        let mut links = 0;
        while let Some(part) = pending.pop_front() {
            match Path::new(&part)
                .components()
                .next()
                .context("Empty link component")?
            {
                Component::RootDir | Component::Prefix(_) => {
                    anyhow::bail!("Absolute symlinks are not supported")
                }
                Component::ParentDir => ensure!(resolved.pop(), "Symlink escapes the store"),
                Component::CurDir => {}
                Component::Normal(_) => {
                    resolved.push(&part);
                    if let Some(Entry {
                        kind: Kind::Symlink { target },
                        ..
                    }) = paths.get(&resolved).and_then(|id| self.entries.get(*id))
                    {
                        links += 1;
                        ensure!(links <= 40, "Symlink cycle or excessive chain");
                        resolved.pop();
                        for component in target.components().rev() {
                            pending.push_front(component.as_os_str().to_os_string());
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// True for a relative path of plain names: no `..`, `.`, root or NUL byte,
/// at most 4096 bytes and 256 components. The empty path is refused.
pub(super) fn safe_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.as_os_str().as_encoded_bytes().len() <= 4096
        && path.components().count() <= 256
        && path.components().all(|c| matches!(c, Component::Normal(_)))
        && !path.as_os_str().as_encoded_bytes().contains(&0)
}

// Decoded chunks by id. `recent` lists the same ids, least recently used first.
#[derive(Default)]
struct Cache {
    chunks: HashMap<u32, Arc<Vec<u8>>>,
    recent: VecDeque<u32>,
}

/// Holds one immutable store open. Each uncached read verifies its chunk hash.
pub struct Reader {
    backing: Backing,
    pub(super) index: Index,
    /// Entry position by path, as returned by `Index::validate`.
    paths: BTreeMap<PathBuf, usize>,
    /// Per entry, the file offset where each of its chunks begins. Empty unless `Kind::File`.
    starts: Vec<Vec<u64>>,
    cache: Mutex<Cache>,
    summary: Summary,
    pool: Option<PathBuf>,
}

/// Where payloads are read from.
enum Backing {
    /// A monolithic store. The mutex keeps one seek and read pair together.
    Archive(Mutex<File>),
    /// The `chunks` folder of a directory store, one object file per chunk.
    Chunks(PathBuf),
}

/// Object file name: decoded hash, decoded length, codec number, stored length.
/// Two encodings of the same bytes therefore get different names.
pub(super) fn chunk_name(chunk: &Chunk) -> String {
    format!(
        "{}-{}-{}-{}",
        blake3::Hash::from_bytes(chunk.hash).to_hex(),
        chunk.raw,
        codec_number(chunk.codec),
        chunk.stored
    )
}

impl Reader {
    /// Opens a store file or directory store and validates its whole index.
    /// No payload is read. Call `verify` to check the chunks themselves.
    pub fn open(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.is_file() {
            return Self::from_file(
                std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(path)?,
            );
        }
        ensure!(metadata.is_dir(), "A store must be a file or directory");
        Self::from_directory(path)
    }

    /// Reads a monolithic store (versions 1, 2, 4, 6). The index must end
    /// at the last byte of the file.
    pub(super) fn from_file(mut file: File) -> Result<Self> {
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "A store must be a regular file");
        let len = metadata.len();
        let (version, block, offset, index_len, index) = read_index(&mut file)?;
        ensure!(
            matches!(version, 1 | 2 | 4 | 6) && block as usize == CHUNK_BYTES,
            "Unsupported store version or chunk size"
        );
        ensure!(
            offset.checked_add(index_len) == Some(len),
            "Truncated store or trailing bytes"
        );
        let paths = index.validate(offset, version)?;
        Self::finish(
            Backing::Archive(Mutex::new(file)),
            index,
            paths,
            len,
            HEADER_BYTES + index_len,
            0,
            None,
        )
    }

    /// Reads a directory store (versions 3, 5, 7): `manifest`, `chunks/` and an
    /// optional `pool.json`. Each object's header is checked against the
    /// manifest here. Object payloads are not read.
    fn from_directory(path: &Path) -> Result<Self> {
        let root = path.canonicalize()?;
        let mut manifest = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root.join("manifest"))?;
        let manifest_len = manifest.metadata()?.len();
        let (version, block, offset, index_len, index) = read_index(&mut manifest)?;
        ensure!(
            matches!(version, 3 | 5 | 7) && block as usize == CHUNK_BYTES && offset == HEADER_BYTES,
            "Unsupported shared store version or chunk size"
        );
        ensure!(
            offset.checked_add(index_len) == Some(manifest_len),
            "Truncated shared store manifest"
        );
        let paths = index.validate(0, version)?;
        let chunks = root.join("chunks");
        ensure!(chunks.is_dir(), "Shared store chunks are missing");
        let pool_marker = root.join("pool.json");
        let (pool, sidecar_bytes) = match std::fs::symlink_metadata(&pool_marker) {
            Ok(metadata) => (Some(read_pool_path(&root)?), metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, 0),
            Err(error) => return Err(error.into()),
        };
        let mut archive_bytes = manifest_len + sidecar_bytes;
        let mut shared_bytes = 0u64;
        for chunk in &index.chunks {
            let path = chunks.join(chunk_name(chunk));
            validate_object(&path, chunk)?;
            let object_bytes = OBJECT_HEADER + u64::from(chunk.stored);
            // Two links are this store's and the pool's. A third means another
            // store links the object. With the pool deleted this undercounts.
            if std::fs::symlink_metadata(&path)?.nlink() > 2 {
                shared_bytes = shared_bytes
                    .checked_add(object_bytes)
                    .context("Shared byte count overflow")?;
            }
            archive_bytes = archive_bytes
                .checked_add(object_bytes)
                .context("Shared store size overflow")?;
        }
        let object_metadata = OBJECT_HEADER
            .checked_mul(u64::try_from(index.chunks.len())?)
            .context("Shared store metadata overflow")?;
        Self::finish(
            Backing::Chunks(chunks),
            index,
            paths,
            archive_bytes,
            manifest_len + sidecar_bytes + object_metadata,
            shared_bytes,
            pool,
        )
    }

    /// Builds the per-file chunk offsets and the size summary for a validated index.
    fn finish(
        backing: Backing,
        index: Index,
        paths: BTreeMap<PathBuf, usize>,
        archive_bytes: u64,
        metadata_bytes: u64,
        shared_bytes: u64,
        pool: Option<PathBuf>,
    ) -> Result<Self> {
        let mut starts = Vec::with_capacity(index.entries.len());
        for entry in &index.entries {
            let mut positions = Vec::new();
            if let Kind::File { chunks, .. } = &entry.kind {
                let mut position = 0u64;
                for id in chunks {
                    positions.push(position);
                    let chunk = index.chunks.get(*id as usize).context("Missing chunk")?;
                    position = position
                        .checked_add(u64::from(chunk.raw))
                        .context("File size overflow")?;
                }
            }
            starts.push(positions);
        }
        let mut files = 0;
        let mut logical_bytes = 0;
        for entry in &index.entries {
            if let Kind::File { size, .. } | Kind::SlicedFile { size, .. } = entry.kind {
                files += 1;
                logical_bytes += size;
            }
        }
        let mut raw_bytes = 0u64;
        let mut compressed_input_bytes = 0u64;
        let mut compressed_bytes = 0u64;
        let mut zero_bytes = 0u64;
        for chunk in &index.chunks {
            match chunk.codec {
                Codec::Raw => raw_bytes += u64::from(chunk.raw),
                Codec::Zstd => {
                    compressed_input_bytes += u64::from(chunk.raw);
                    compressed_bytes += u64::from(chunk.stored);
                }
                Codec::Zero => zero_bytes += u64::from(chunk.raw),
            }
        }
        let unique = raw_bytes + compressed_input_bytes + zero_bytes;
        let summary = Summary {
            files,
            logical_bytes,
            archive_bytes,
            metadata_bytes,
            raw_bytes,
            compressed_input_bytes,
            compressed_bytes,
            zero_bytes,
            shared_bytes,
            unique_chunks: index.chunks.len() as u64,
            duplicate_bytes: logical_bytes.saturating_sub(unique),
        };
        Ok(Self {
            backing,
            index,
            paths,
            starts,
            cache: Mutex::new(Cache::default()),
            summary,
            pool,
        })
    }

    pub fn summary(&self) -> &Summary {
        &self.summary
    }
    /// Pool path recorded by a directory store. It comes from `pool.json`,
    /// which the index checksum does not cover.
    pub fn pool_path(&self) -> Option<&Path> {
        self.pool.as_deref()
    }
    /// All entries in index order: the root first, parents before children.
    pub fn entries(&self) -> &[Entry] {
        &self.index.entries
    }
    /// Looks up one entry. The root is the empty path.
    pub fn entry(&self, path: &Path) -> Option<&Entry> {
        self.paths
            .get(path)
            .and_then(|id| self.index.entries.get(*id))
    }

    /// Paths whose `hardlink_to` names `path`. Scans every entry.
    #[cfg(feature = "pack-mount")]
    pub(super) fn hardlink_aliases(&self, path: &Path) -> Vec<PathBuf> {
        self.index
            .entries
            .iter()
            .filter(|entry| entry.hardlink_to.as_deref() == Some(path))
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// Returns one decoded chunk, from the cache when it is there. A miss
    /// decodes and hash-checks it, then evicts the least recently used chunk.
    pub(super) fn chunk(&self, id: u32) -> Result<Arc<Vec<u8>>> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Chunk cache lock poisoned"))?;
        if let Some(bytes) = cache.chunks.get(&id).cloned() {
            cache.recent.retain(|old| *old != id);
            cache.recent.push_back(id);
            return Ok(bytes);
        }
        let bytes = Arc::new(self.decode(id)?);
        if cache.chunks.len() >= CACHE_CHUNKS
            && let Some(old) = cache.recent.pop_front()
        {
            cache.chunks.remove(&old);
        }
        cache.chunks.insert(id, bytes.clone());
        cache.recent.push_back(id);
        Ok(bytes)
    }

    /// Length of a zero chunk, so a writer can seek past it and leave a hole.
    pub(super) fn zero_chunk_len(&self, id: u32) -> Result<Option<u32>> {
        let chunk = self
            .index
            .chunks
            .get(id as usize)
            .context("Missing chunk")?;
        Ok(matches!(chunk.codec, Codec::Zero).then_some(chunk.raw))
    }

    /// Reads, decodes and hash-checks one chunk from storage. Never uses the
    /// cache. A directory store rechecks the object's header on every call.
    fn decode(&self, id: u32) -> Result<Vec<u8>> {
        let chunk = self
            .index
            .chunks
            .get(id as usize)
            .context("Missing chunk")?;
        let mut encoded = vec![0; chunk.stored as usize];
        match &self.backing {
            Backing::Archive(file) if !encoded.is_empty() => {
                let mut file = file
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Store lock poisoned"))?;
                file.seek(SeekFrom::Start(chunk.offset))?;
                file.read_exact(&mut encoded)?;
            }
            Backing::Chunks(root) if !encoded.is_empty() => {
                let path = root.join(chunk_name(chunk));
                let mut file = open_object(&path, chunk)?;
                file.seek(SeekFrom::Start(OBJECT_HEADER))?;
                file.read_exact(&mut encoded)?;
            }
            _ => {}
        }
        let decoded = match chunk.codec {
            Codec::Raw => encoded,
            Codec::Zero => vec![0; chunk.raw as usize],
            Codec::Zstd => {
                let mut decoder = zstd::bulk::Decompressor::new()?;
                // Window log 23 is 8 MiB, which covers any 4 MiB chunk. A frame
                // that asks for a larger window is refused.
                decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(23))?;
                decoder.decompress(&encoded, chunk.raw as usize)?
            }
        };
        ensure!(
            decoded.len() == chunk.raw as usize && blake3::hash(&decoded).as_bytes() == &chunk.hash,
            "Chunk {id} failed verification"
        );
        Ok(decoded)
    }

    /// Reads at most 4 MiB at an arbitrary file offset; EOF returns no bytes.
    pub fn read(&self, path: &Path, offset: u64, length: usize) -> Result<Vec<u8>> {
        ensure!(length <= CHUNK_BYTES, "Read exceeds the per-request limit");
        let entry_id = self.paths.get(path).copied().context("File not found")?;
        let entry = self.index.entries.get(entry_id).context("File not found")?;
        let size = match &entry.kind {
            Kind::File { size, .. } | Kind::SlicedFile { size, .. } => *size,
            _ => anyhow::bail!("Entry is not a regular file"),
        };
        let count = u64::try_from(length)?.min(size.saturating_sub(offset)) as usize;
        // A sliced file sits inside one frame, so one decoded chunk serves the read.
        if let Kind::SlicedFile {
            chunk,
            offset: start,
            ..
        } = &entry.kind
        {
            if count == 0 {
                return Ok(Vec::new());
            }
            let bytes = self.chunk(*chunk)?;
            let start = usize::try_from(u64::from(*start) + offset)?;
            return Ok(bytes
                .get(start..start + count)
                .context("Invalid shared file bounds")?
                .to_vec());
        }
        let Kind::File { chunks, .. } = &entry.kind else {
            anyhow::bail!("Entry is not a regular file")
        };
        let starts = self.starts.get(entry_id).context("Missing file offsets")?;
        let mut result = Vec::with_capacity(count);
        let mut position = offset;
        while result.len() < count {
            // `starts` ascends, so the chunk holding `position` is the last
            // one that begins at or before it.
            let ordinal = starts
                .partition_point(|start| *start <= position)
                .saturating_sub(1);
            let bytes = self.chunk(*chunks.get(ordinal).context("Missing file chunk")?)?;
            let chunk_start = starts
                .get(ordinal)
                .copied()
                .context("Missing chunk offset")?;
            let start = usize::try_from(position.saturating_sub(chunk_start))?;
            let take = (count - result.len()).min(bytes.len().saturating_sub(start));
            ensure!(take > 0, "Invalid chunk range");
            result.extend_from_slice(
                bytes
                    .get(start..start + take)
                    .context("Invalid read bounds")?,
            );
            position += take as u64;
        }
        Ok(result)
    }

    /// Rechecks every unique payload, bypassing cached data.
    pub fn verify(&self, cancel: &AtomicBool) -> Result<()> {
        self.verify_observed(cancel, &super::NoObserver)
    }

    /// `verify`, reporting one step per chunk to `observer`.
    pub fn verify_observed(
        &self,
        cancel: &AtomicBool,
        observer: &dyn super::Observer,
    ) -> Result<()> {
        observer.started(self.index.chunks.len() as u64, 0, "Verifying stored chunks…");
        for id in 0..self.index.chunks.len() {
            observer.checkpoint()?;
            ensure!(!cancel.load(Ordering::Relaxed), "Verification cancelled");
            self.decode(u32::try_from(id)?)?;
            observer.progress(id as u64 + 1, 0, "Verifying stored chunks…");
        }
        Ok(())
    }

    /// Verifies that a directory still contains the files represented by this store.
    pub fn verify_directory(&self, root: &Path, cancel: &AtomicBool) -> Result<()> {
        self.verify_directory_observed(root, cancel, &super::NoObserver)
    }

    /// `verify_directory` with progress. Compares paths, modes, xattrs,
    /// hard-link identity and every byte, then stats each path again and fails
    /// if anything moved while the comparison ran. Activation relies on this.
    pub fn verify_directory_observed(
        &self,
        root: &Path,
        cancel: &AtomicBool,
        observer: &dyn super::Observer,
    ) -> Result<()> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        let root = root.canonicalize()?;
        ensure!(root.is_dir(), "The source must be a directory");
        observer.started(
            self.index.entries.len() as u64,
            0,
            "Comparing the store with installed files…",
        );
        let mut seen = 0usize;
        let mut identities = Vec::new();
        for item in walkdir::WalkDir::new(&root).follow_links(false) {
            observer.checkpoint()?;
            ensure!(!cancel.load(Ordering::Relaxed), "Verification cancelled");
            let item = item?;
            let path = item.path();
            let relative = path.strip_prefix(&root)?;
            let expected = self
                .entry(relative)
                .with_context(|| format!("The source has an extra path: {}", relative.display()))?;
            let metadata = std::fs::symlink_metadata(path)?;
            identities.push((
                path.to_path_buf(),
                metadata.dev(),
                metadata.ino(),
                metadata.size(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
                metadata.mode(),
            ));
            ensure!(
                metadata.mode() & 0o7777 == expected.mode,
                "Permissions changed for {}",
                relative.display()
            );
            if !metadata.is_symlink() {
                let mut attributes = Vec::new();
                for name in xattr::list(path)? {
                    attributes.push(Xattr {
                        name: name.as_bytes().to_vec(),
                        value: xattr::get(path, &name)?
                            .context("Extended attribute disappeared")?,
                    });
                }
                attributes.sort_by(|left, right| left.name.cmp(&right.name));
                ensure!(
                    attributes == expected.xattrs,
                    "Extended attributes changed for {}",
                    relative.display()
                );
            }
            if let Some(target) = &expected.hardlink_to {
                let target_metadata = std::fs::symlink_metadata(root.join(target))?;
                ensure!(
                    (metadata.dev(), metadata.ino())
                        == (target_metadata.dev(), target_metadata.ino()),
                    "Hard-link identity changed for {}",
                    relative.display()
                );
            }
            match &expected.kind {
                Kind::Directory => ensure!(
                    metadata.is_dir(),
                    "{} is no longer a directory",
                    relative.display()
                ),
                Kind::Symlink { target } => ensure!(
                    metadata.is_symlink() && std::fs::read_link(path)? == *target,
                    "Symlink changed: {}",
                    relative.display()
                ),
                Kind::File { size, .. } | Kind::SlicedFile { size, .. } => {
                    ensure!(
                        metadata.is_file() && metadata.len() == *size,
                        "File size changed: {}",
                        relative.display()
                    );
                    let mut source = std::fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                        .open(path)?;
                    let mut offset = 0u64;
                    while offset < *size {
                        observer.checkpoint()?;
                        let count = usize::try_from((*size - offset).min(CHUNK_BYTES as u64))?;
                        let mut source_bytes = vec![0; count];
                        source.read_exact(&mut source_bytes)?;
                        ensure!(
                            self.read(relative, offset, count)? == source_bytes,
                            "File contents changed: {}",
                            relative.display()
                        );
                        offset += count as u64;
                    }
                }
            }
            seen = seen.saturating_add(1);
            observer.progress(seen as u64, 0, "Comparing the store with installed files…");
        }
        ensure!(
            seen == self.entries().len(),
            "The source is missing files contained in the store"
        );
        // Second pass: a path whose inode, size, times or mode differ from
        // what was recorded as it was compared means the tree was written to.
        for (path, dev, ino, size, mtime, mtime_nsec, ctime, ctime_nsec, mode) in identities {
            observer.checkpoint()?;
            ensure!(!cancel.load(Ordering::Relaxed), "Verification cancelled");
            let current = std::fs::symlink_metadata(&path)?;
            ensure!(
                (
                    current.dev(),
                    current.ino(),
                    current.size(),
                    current.mtime(),
                    current.mtime_nsec(),
                    current.ctime(),
                    current.ctime_nsec(),
                    current.mode()
                ) == (dev, ino, size, mtime, mtime_nsec, ctime, ctime_nsec, mode),
                "The installed game changed during verification: {}",
                path.display()
            );
        }
        Ok(())
    }
}

/// Reads the header and the checksummed index. Returns the version, chunk
/// size, index offset, index length and the parsed index, not yet validated.
fn read_index(file: &mut File) -> Result<(u32, u32, u64, u64, Index)> {
    // Header, 64 bytes, integers little-endian: magic[8], version u32,
    // maximum chunk size u32, index offset u64, index length u64, and the
    // BLAKE3 digest of the index bytes[32].
    file.seek(SeekFrom::Start(0))?;
    let mut magic = [0; 8];
    file.read_exact(&mut magic)?;
    ensure!(&magic == MAGIC, "Unknown Flummox store format");
    let version = read_u32(file)?;
    let block = read_u32(file)?;
    let offset = read_u64(file)?;
    let index_len = read_u64(file)?;
    let mut hash = [0; 32];
    file.read_exact(&mut hash)?;
    ensure!(
        offset >= HEADER_BYTES && index_len > 0 && index_len <= MAX_INDEX,
        "Invalid index bounds"
    );
    let end = offset
        .checked_add(index_len)
        .context("Index size overflow")?;
    ensure!(
        end <= file.metadata()?.len(),
        "Index extends beyond the store"
    );
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; usize::try_from(index_len)?];
    file.read_exact(&mut bytes)?;
    ensure!(
        blake3::hash(&bytes).as_bytes() == &hash,
        "Store index checksum mismatch"
    );
    Ok((
        version,
        block,
        offset,
        index_len,
        serde_json::from_slice(&bytes)?,
    ))
}

/// Codec number written in object headers and object file names.
pub(super) fn codec_number(codec: Codec) -> u32 {
    match codec {
        Codec::Raw => 0,
        Codec::Zstd => 1,
        Codec::Zero => 2,
    }
}

fn open_object(path: &Path, expected: &Chunk) -> Result<File> {
    let (file, actual) = inspect_object(path)?;
    ensure!(
        actual == *expected,
        "Shared chunk does not match its manifest"
    );
    Ok(file)
}

/// Opens a chunk object and parses its header into a descriptor with offset 0.
/// The payload is not read, so the hash in the header is not checked here.
fn inspect_object(path: &Path) -> Result<(File, Chunk)> {
    // O_NOFOLLOW refuses a symlink. O_NONBLOCK makes the open of a FIFO return
    // at once, and the file-type check below then rejects it.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "Shared chunk is not a file");
    // Object header, 64 bytes, integers little-endian: magic[8], codec u32,
    // decoded length u32, stored length u32, BLAKE3 of the decoded bytes[32],
    // then 12 zero bytes. The payload is the rest of the file.
    let mut magic = [0; 8];
    file.read_exact(&mut magic)?;
    ensure!(&magic == OBJECT_MAGIC, "Unknown shared chunk format");
    let codec = match read_u32(&mut file)? {
        0 => Codec::Raw,
        1 => Codec::Zstd,
        2 => Codec::Zero,
        _ => anyhow::bail!("Invalid shared chunk codec"),
    };
    let raw = read_u32(&mut file)?;
    let stored = read_u32(&mut file)?;
    let mut hash = [0; 32];
    file.read_exact(&mut hash)?;
    let mut reserved = [0; 12];
    file.read_exact(&mut reserved)?;
    ensure!(
        reserved.iter().all(|byte| *byte == 0),
        "Invalid shared chunk header"
    );
    ensure!(
        file.metadata()?.len() == OBJECT_HEADER + u64::from(stored),
        "Truncated shared chunk"
    );
    Ok((
        file,
        Chunk {
            offset: 0,
            stored,
            raw,
            codec,
            hash,
        },
    ))
}

/// Checks that the object at `path` carries the header `expected` describes.
pub(super) fn validate_object(path: &Path, expected: &Chunk) -> Result<()> {
    let _file = open_object(path, expected)?;
    Ok(())
}

/// Checks that a file in a pool is a chunk object named after its own header.
pub(super) fn validate_pool_object(path: &Path) -> Result<()> {
    let (_file, chunk) = inspect_object(path)?;
    ensure!(
        path.file_name().and_then(|name| name.to_str()) == Some(chunk_name(&chunk).as_str()),
        "Shared pool object name does not match its header"
    );
    Ok(())
}

fn read_pool_path(root: &Path) -> Result<PathBuf> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join("pool.json"))?;
    Ok(serde_json::from_reader::<_, PoolRecord>(file)?.path)
}

fn read_u32(file: &mut File) -> Result<u32> {
    let mut bytes = [0; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}
fn read_u64(file: &mut File) -> Result<u64> {
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
