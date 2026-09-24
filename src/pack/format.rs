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

pub const CHUNK_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MIN_CHUNK_BYTES: usize = 512 * 1024;
pub(super) const HEADER_BYTES: u64 = 64;
pub(super) const MAX_INDEX: u64 = 32 * 1024 * 1024;
pub(super) const MAX_ENTRIES: usize = 100_000;
pub(super) const MAX_CHUNKS: usize = 1_000_000;
pub(super) const MAGIC: &[u8; 8] = b"FLUMPK01";
pub(super) const OBJECT_MAGIC: &[u8; 8] = b"FLUMCH01";
pub(super) const OBJECT_HEADER: u64 = 64;
const CACHE_CHUNKS: usize = 8;
const MAX_XATTRS_PER_ENTRY: usize = 256;
const MAX_XATTR_BYTES_PER_ENTRY: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Xattr {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    #[serde(with = "crate::path_serde")]
    pub path: PathBuf,
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
    pub hardlink_to: Option<PathBuf>,
    pub kind: Kind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Kind {
    Directory,
    File {
        size: u64,
        chunks: Vec<u32>,
    },
    /// One file inside a shared, independently decoded frame.
    SlicedFile {
        size: u64,
        chunk: u32,
        offset: u32,
    },
    Symlink {
        #[serde(with = "crate::path_serde")]
        target: PathBuf,
    },
}

impl Kind {
    pub fn size(&self) -> Option<u64> {
        match self {
            Self::File { size, .. } | Self::SlicedFile { size, .. } => Some(*size),
            Self::Directory | Self::Symlink { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Codec {
    Raw,
    Zstd,
    Zero,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Chunk {
    pub offset: u64,
    pub stored: u32,
    pub raw: u32,
    pub codec: Codec,
    pub hash: [u8; 32],
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Index {
    pub entries: Vec<Entry>,
    pub chunks: Vec<Chunk>,
}

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
    pub logical_bytes: u64,
    pub archive_bytes: u64,
    pub metadata_bytes: u64,
    #[serde(default)]
    pub raw_bytes: u64,
    #[serde(default)]
    pub compressed_input_bytes: u64,
    #[serde(default)]
    pub compressed_bytes: u64,
    #[serde(default)]
    pub zero_bytes: u64,
    #[serde(default)]
    pub shared_bytes: u64,
    pub unique_chunks: u64,
    pub duplicate_bytes: u64,
}

impl Index {
    pub(super) fn validate(
        &self,
        payload_end: u64,
        version: u32,
    ) -> Result<BTreeMap<PathBuf, usize>> {
        ensure!(matches!(version, 1..=7), "Unsupported store version");
        let external_chunks = matches!(version, 3 | 5 | 7);
        let extended_metadata = version >= 4;
        ensure!(
            !self.entries.is_empty() && self.entries.len() <= MAX_ENTRIES,
            "Invalid entry count"
        );
        ensure!(self.chunks.len() <= MAX_CHUNKS, "Too many chunks");
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
        for (number, entry) in self.entries.iter().enumerate() {
            let mode_mask = if extended_metadata { 0o7777 } else { 0o777 };
            ensure!(
                entry.mode & !mode_mask == 0 && entry.modified_nanos < 1_000_000_000,
                "Invalid file metadata"
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
        for entry in &self.entries {
            if let Kind::Symlink { target } = &entry.kind {
                self.check_link(&entry.path, target, &paths)?;
            }
        }
        Ok(paths)
    }

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

pub(super) fn safe_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.as_os_str().as_encoded_bytes().len() <= 4096
        && path.components().count() <= 256
        && path.components().all(|c| matches!(c, Component::Normal(_)))
        && !path.as_os_str().as_encoded_bytes().contains(&0)
}

#[derive(Default)]
struct Cache {
    chunks: HashMap<u32, Arc<Vec<u8>>>,
    recent: VecDeque<u32>,
}

/// Holds one immutable store open. Each uncached read verifies its chunk hash.
pub struct Reader {
    backing: Backing,
    pub(super) index: Index,
    paths: BTreeMap<PathBuf, usize>,
    starts: Vec<Vec<u64>>,
    cache: Mutex<Cache>,
    summary: Summary,
    pool: Option<PathBuf>,
}

enum Backing {
    Archive(Mutex<File>),
    Chunks(PathBuf),
}

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
    pub fn pool_path(&self) -> Option<&Path> {
        self.pool.as_deref()
    }
    pub fn entries(&self) -> &[Entry] {
        &self.index.entries
    }
    pub fn entry(&self, path: &Path) -> Option<&Entry> {
        self.paths
            .get(path)
            .and_then(|id| self.index.entries.get(*id))
    }

    pub(super) fn hardlink_aliases(&self, path: &Path) -> Vec<PathBuf> {
        self.index
            .entries
            .iter()
            .filter(|entry| entry.hardlink_to.as_deref() == Some(path))
            .map(|entry| entry.path.clone())
            .collect()
    }

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

    pub(super) fn zero_chunk_len(&self, id: u32) -> Result<Option<u32>> {
        let chunk = self
            .index
            .chunks
            .get(id as usize)
            .context("Missing chunk")?;
        Ok(matches!(chunk.codec, Codec::Zero).then_some(chunk.raw))
    }

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
        for id in 0..self.index.chunks.len() {
            ensure!(!cancel.load(Ordering::Relaxed), "Verification cancelled");
            self.decode(u32::try_from(id)?)?;
        }
        Ok(())
    }

    /// Verifies that a directory still contains the files represented by this store.
    pub fn verify_directory(&self, root: &Path, cancel: &AtomicBool) -> Result<()> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        let root = root.canonicalize()?;
        ensure!(root.is_dir(), "The source must be a directory");
        let mut seen = 0usize;
        let mut identities = Vec::new();
        for item in walkdir::WalkDir::new(&root).follow_links(false) {
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
        }
        ensure!(
            seen == self.entries().len(),
            "The source is missing files contained in the store"
        );
        for (path, dev, ino, size, mtime, mtime_nsec, ctime, ctime_nsec, mode) in identities {
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

fn read_index(file: &mut File) -> Result<(u32, u32, u64, u64, Index)> {
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

fn inspect_object(path: &Path) -> Result<(File, Chunk)> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "Shared chunk is not a file");
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

pub(super) fn validate_object(path: &Path, expected: &Chunk) -> Result<()> {
    let _file = open_object(path, expected)?;
    Ok(())
}

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
