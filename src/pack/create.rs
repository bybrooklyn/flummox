//! Builds a new store, verifies it, then publishes it without replacement.

use super::format::*;
use super::{NoObserver, Observer};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{File, Metadata},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

// A boundary needs 21 zero bits, so past the minimum length one occurs on
// average every 2 MiB of content.
const CONTENT_MASK: u64 = (1 << 21) - 1;
// Files below one minimum chunk are candidates for a shared frame.
const SMALL_FILE_LIMIT: u64 = MIN_CHUNK_BYTES as u64;
// A shared frame is kept only when it beats separate chunks by this many bytes.
const GROUP_GAIN_FLOOR: usize = 4096;

// Inputs shared by the small-file grouping passes. `pool` lets the cost
// comparison treat an object already in the pool as free.
#[derive(Clone, Copy)]
struct GroupParams<'a> {
    anchor: &'a crate::safeio::Anchor,
    options: Options,
    cancel: &'a AtomicBool,
    pool: Option<&'a Path>,
    observer: &'a dyn Observer,
}

// The splitmix64 finalizer of the byte. It stands in for a 256-entry gear
// table, and changing it moves every chunk boundary.
fn gear(byte: u8) -> u64 {
    let mut value = u64::from(byte).wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Reads the next content-defined chunk into `output`. Returns false at end
/// of input. A chunk ends at CHUNK_BYTES, at end of input, or once it holds
/// MIN_CHUNK_BYTES and the fingerprint's low 21 bits are zero.
fn read_content_chunk(reader: &mut impl BufRead, output: &mut Vec<u8>) -> std::io::Result<bool> {
    output.clear();
    // The fingerprint restarts with each chunk and shifts left once per byte,
    // so its low 21 bits depend only on the 21 bytes before a boundary. Cut
    // points follow content, and line up again after inserted or removed bytes.
    let mut fingerprint = 0u64;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!output.is_empty());
        }
        let limit = (CHUNK_BYTES - output.len()).min(available.len());
        let bytes = available.get(..limit).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid chunk buffer")
        })?;
        let mut consumed = 0usize;
        let mut boundary = false;
        for byte in bytes {
            fingerprint = (fingerprint << 1).wrapping_add(gear(*byte));
            consumed += 1;
            let length = output.len() + consumed;
            if length == CHUNK_BYTES
                || (length >= MIN_CHUNK_BYTES && fingerprint & CONTENT_MASK == 0)
            {
                boundary = true;
                break;
            }
        }
        output.extend_from_slice(bytes.get(..consumed).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid chunk boundary")
        })?);
        reader.consume(consumed);
        if boundary {
            return Ok(true);
        }
    }
}

/// Maximum tries the useful high-level steps on each unique chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// zstd level tried on every chunk, 1 to 22.
    pub level: i32,
    /// `Some(22)` also tries 15, 19 and 22. Any other value tries that one
    /// extra level. The smallest encoding is kept.
    pub compare_level: Option<i32>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            level: 9,
            compare_level: None,
        }
    }
}
impl Options {
    /// Per-chunk search used by Maximum Space creation and later compaction.
    pub fn maximum() -> Self {
        Self {
            level: 9,
            compare_level: Some(22),
        }
    }
}

// The stat fields compared before and after each read, and across the two
// source snapshots. A difference in any of them aborts the build.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    mode: u32,
    modified: (i64, i64),
    changed: (i64, i64),
    links: u64,
}
impl From<&Metadata> for Stamp {
    fn from(m: &Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.size(),
            mode: m.mode(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
            links: m.nlink(),
        }
    }
}

// One source path as the walk saw it. `link` is a symlink's target.
// `hardlink_to` names the first path, in walk order, with the same inode.
#[derive(Debug, PartialEq, Eq)]
struct Source {
    path: PathBuf,
    stamp: Stamp,
    link: Option<PathBuf>,
    xattrs: Vec<Xattr>,
    hardlink_to: Option<PathBuf>,
}

fn read_xattrs(path: &Path) -> Result<Vec<Xattr>> {
    let mut attributes = Vec::new();
    for name in xattr::list(path)? {
        let value = xattr::get(path, &name)?.context("Extended attribute disappeared")?;
        attributes.push(Xattr {
            name: name.as_bytes().to_vec(),
            value,
        });
    }
    attributes.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(attributes)
}

/// Walks the source in sorted order and records every path's metadata. Run
/// before and after a build, the two results must be equal for the store to
/// be published. Refuses device nodes, sockets and FIFOs.
fn snapshot(root: &Path, cancel: &AtomicBool) -> Result<Vec<Source>> {
    let mut result = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
    {
        ensure!(!cancel.load(Ordering::Relaxed), "Store creation cancelled");
        let entry = entry?;
        ensure!(
            result.len() < MAX_ENTRIES && entry.depth() <= 256,
            "Install exceeds the store entry limit"
        );
        let metadata = std::fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.is_dir() || metadata.is_file() || metadata.is_symlink(),
            "Unsupported special file: {}",
            entry.path().display()
        );
        let rel = entry.path().strip_prefix(root)?.to_path_buf();
        ensure!(
            entry.depth() == 0 || safe_path(&rel),
            "Unsupported source path"
        );
        let link = if metadata.is_symlink() {
            Some(std::fs::read_link(entry.path())?)
        } else {
            None
        };
        result.push(Source {
            path: rel,
            stamp: Stamp::from(&metadata),
            link,
            xattrs: if metadata.is_symlink() {
                Vec::new()
            } else {
                read_xattrs(entry.path())?
            },
            hardlink_to: None,
        });
    }
    // Count how many walked paths share each multiply linked inode. If that
    // is fewer than the inode's link count, a link lives outside the game
    // folder and the store could not preserve it.
    let mut counts = HashMap::<(u64, u64), u64>::new();
    for source in &result {
        if source.stamp.mode & libc::S_IFMT == libc::S_IFREG && source.stamp.links > 1 {
            let count = counts
                .entry((source.stamp.device, source.stamp.inode))
                .or_default();
            *count = count.saturating_add(1);
        }
    }
    let mut first = HashMap::<(u64, u64), PathBuf>::new();
    for source in &mut result {
        if source.stamp.mode & libc::S_IFMT != libc::S_IFREG || source.stamp.links <= 1 {
            continue;
        }
        let key = (source.stamp.device, source.stamp.inode);
        ensure!(
            counts.get(&key).copied() == Some(source.stamp.links),
            "A hard-linked file also has links outside the game folder: {}",
            source.path.display()
        );
        if let Some(target) = first.get(&key) {
            source.hardlink_to = Some(target.clone());
        } else {
            first.insert(key, source.path.clone());
        }
    }
    Ok(result)
}

/// Returns the id of the chunk holding `bytes`, storing it through `store`
/// only when no chunk with the same hash and length exists yet. `prepared`
/// passes an encoding already made, so it is not computed twice.
fn intern_chunk(
    index: &mut Index,
    known: &mut HashMap<([u8; 32], u32), u32>,
    bytes: &[u8],
    options: Options,
    prepared: Option<(Codec, Vec<u8>)>,
    store: &mut impl FnMut(u32, Codec, &[u8], [u8; 32]) -> Result<Chunk>,
) -> Result<u32> {
    let count = u32::try_from(bytes.len())?;
    let hash = *blake3::hash(bytes).as_bytes();
    let key = (hash, count);
    if let Some(id) = known.get(&key) {
        return Ok(*id);
    }
    ensure!(
        index.chunks.len() < MAX_CHUNKS,
        "Install exceeds the store chunk limit"
    );
    let encoded = match prepared {
        Some(encoded) => encoded,
        None => encode(bytes, options)?,
    };
    let id = u32::try_from(index.chunks.len())?;
    index
        .chunks
        .push(store(count, encoded.0, &encoded.1, hash)?);
    known.insert(key, id);
    Ok(id)
}

/// Packs small files into shared frames where that saves space, and returns
/// the `Kind::SlicedFile` for each file it grouped. Files it leaves out are
/// chunked on their own by the caller.
fn group_small_files(
    params: GroupParams<'_>,
    sources: &[Source],
    index: &mut Index,
    known: &mut HashMap<([u8; 32], u32), u32>,
    store: &mut impl FnMut(u32, Codec, &[u8], [u8; 32]) -> Result<Chunk>,
) -> Result<HashMap<PathBuf, Kind>> {
    // First pass: hash every small file to count how many share its content.
    // Hard-link aliases are skipped because they reuse their target's kind.
    let mut counts = HashMap::<([u8; 32], u32), u32>::new();
    let mut fingerprints = HashMap::<PathBuf, ([u8; 32], u32)>::new();
    for source in sources {
        if source.stamp.mode & libc::S_IFMT != libc::S_IFREG
            || source.hardlink_to.is_some()
            || !(1..SMALL_FILE_LIMIT).contains(&source.stamp.size)
        {
            continue;
        }
        params.observer.checkpoint()?;
        ensure!(
            !params.cancel.load(Ordering::Relaxed),
            "Store creation cancelled"
        );
        let mut file = params.anchor.open_with(
            &source.path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK,
        )?;
        ensure!(
            Stamp::from(&file.metadata()?) == source.stamp,
            "{} changed before packing",
            source.path.display()
        );
        let mut bytes = vec![0; usize::try_from(source.stamp.size)?];
        file.read_exact(&mut bytes)?;
        ensure!(
            Stamp::from(&file.metadata()?) == source.stamp,
            "{} changed during packing",
            source.path.display()
        );
        let key = (
            *blake3::hash(&bytes).as_bytes(),
            u32::try_from(bytes.len())?,
        );
        *counts.entry(key).or_default() += 1;
        fingerprints.insert(source.path.clone(), key);
    }
    // Second pass: only files with unique content are grouped, so exact
    // duplicates keep sharing one chunk. Candidates are bucketed by lowercase
    // extension, then cut into frames of at most CHUNK_BYTES in path order.
    let mut by_extension = BTreeMap::<Vec<u8>, Vec<&Source>>::new();
    for source in sources {
        if source.stamp.mode & libc::S_IFMT == libc::S_IFREG
            && source.hardlink_to.is_none()
            && (1..SMALL_FILE_LIMIT).contains(&source.stamp.size)
            && fingerprints
                .get(&source.path)
                .and_then(|key| counts.get(key))
                .copied()
                == Some(1)
        {
            let extension = source
                .path
                .extension()
                .map(OsStrExt::as_bytes)
                .unwrap_or_default()
                .to_ascii_lowercase();
            by_extension.entry(extension).or_default().push(source);
        }
    }
    let mut grouped = HashMap::new();
    for files in by_extension.values() {
        let mut group = Vec::new();
        let mut total = 0u64;
        for source in files {
            if total + source.stamp.size > CHUNK_BYTES as u64 {
                store_group(params, &group, index, known, store, &mut grouped)?;
                group.clear();
                total = 0;
            }
            group.push(*source);
            total += source.stamp.size;
        }
        store_group(params, &group, index, known, store, &mut grouped)?;
    }
    Ok(grouped)
}

/// Builds one candidate frame and stores it only if it costs at least
/// GROUP_GAIN_FLOOR bytes less than encoding its files separately. A chunk
/// already in this store or in the pool counts as free on either side.
fn store_group(
    params: GroupParams<'_>,
    group: &[&Source],
    index: &mut Index,
    known: &mut HashMap<([u8; 32], u32), u32>,
    store: &mut impl FnMut(u32, Codec, &[u8], [u8; 32]) -> Result<Chunk>,
    grouped: &mut HashMap<PathBuf, Kind>,
) -> Result<()> {
    if group.len() < 2 {
        return Ok(());
    }
    let mut frame = Vec::with_capacity(CHUNK_BYTES);
    let mut positions = Vec::with_capacity(group.len());
    let mut individual_cost = 0usize;
    let mut unique = HashSet::new();
    for source in group {
        params.observer.checkpoint()?;
        ensure!(
            !params.cancel.load(Ordering::Relaxed),
            "Store creation cancelled"
        );
        let mut file = params.anchor.open_with(
            &source.path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK,
        )?;
        ensure!(
            Stamp::from(&file.metadata()?) == source.stamp,
            "{} changed before packing",
            source.path.display()
        );
        let offset = u32::try_from(frame.len())?;
        let start = frame.len();
        frame.resize(start + usize::try_from(source.stamp.size)?, 0);
        file.read_exact(frame.get_mut(start..).context("Missing group buffer")?)?;
        ensure!(
            Stamp::from(&file.metadata()?) == source.stamp,
            "{} changed during packing",
            source.path.display()
        );
        let bytes = frame.get(start..).context("Missing group bytes")?;
        let key = (*blake3::hash(bytes).as_bytes(), u32::try_from(bytes.len())?);
        if !known.contains_key(&key) && unique.insert(key) {
            let encoded = encode(bytes, params.options)?;
            let object = Chunk {
                offset: 0,
                stored: u32::try_from(encoded.1.len())?,
                raw: key.1,
                codec: encoded.0,
                hash: key.0,
            };
            let pooled = params.pool.is_some_and(|root| {
                std::fs::symlink_metadata(root.join(chunk_name(&object)))
                    .is_ok_and(|metadata| metadata.is_file())
            });
            if !pooled {
                individual_cost += encoded.1.len();
            }
        }
        positions.push((source.path.clone(), source.stamp.size, offset));
    }
    let encoded = encode(&frame, params.options)?;
    let group_key = (
        *blake3::hash(&frame).as_bytes(),
        u32::try_from(frame.len())?,
    );
    let group_object = Chunk {
        offset: 0,
        stored: u32::try_from(encoded.1.len())?,
        raw: group_key.1,
        codec: encoded.0,
        hash: group_key.0,
    };
    let group_pooled = params.pool.is_some_and(|root| {
        std::fs::symlink_metadata(root.join(chunk_name(&group_object)))
            .is_ok_and(|metadata| metadata.is_file())
    });
    let group_cost = if known.contains_key(&group_key) || group_pooled {
        0
    } else {
        encoded.1.len()
    };
    if group_cost.saturating_add(GROUP_GAIN_FLOOR) > individual_cost {
        return Ok(());
    }
    let id = intern_chunk(index, known, &frame, params.options, Some(encoded), store)?;
    for (path, size, offset) in positions {
        grouped.insert(
            path,
            Kind::SlicedFile {
                size,
                chunk: id,
                offset,
            },
        );
    }
    Ok(())
}

/// Resolves an output path to (canonical parent, final path) and fails if
/// anything, including a dangling symlink, already exists there.
pub(super) fn destination(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let name = path.file_name().context("Choose a new destination name")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    let target = parent.join(name);
    match std::fs::symlink_metadata(&target) {
        Ok(_) => anyhow::bail!("Destination already exists: {}", target.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok((parent, target))
}

/// Reads the whole source and returns its index with the snapshot it was
/// built from. `store` receives each new unique chunk (decoded length, codec,
/// encoded bytes, hash) and returns the descriptor to record for it.
fn build_index(
    root: &Path,
    options: Options,
    cancel: &AtomicBool,
    pool: Option<&Path>,
    observer: &dyn Observer,
    mut store: impl FnMut(u32, Codec, &[u8], [u8; 32]) -> Result<Chunk>,
) -> Result<(Index, Vec<Source>)> {
    let anchor = crate::safeio::Anchor::open(root)?;
    ensure!(
        anchor.fully_resolved(),
        "Store creation requires safe path resolution"
    );
    let sources = snapshot(root, cancel)?;
    let mut index = Index {
        entries: Vec::new(),
        chunks: Vec::new(),
    };
    let mut known = HashMap::<([u8; 32], u32), u32>::new();
    observer.started(
        sources
            .iter()
            .filter(|s| s.stamp.mode & libc::S_IFMT == libc::S_IFREG)
            .count() as u64,
        sources
            .iter()
            .filter(|s| s.stamp.mode & libc::S_IFMT == libc::S_IFREG)
            .map(|s| s.stamp.size)
            .sum(),
        "Building Maximum Space store",
    );
    observer.checkpoint()?;
    let grouped = group_small_files(
        GroupParams {
            anchor: &anchor,
            options,
            cancel,
            pool,
            observer,
        },
        &sources,
        &mut index,
        &mut known,
        &mut store,
    )?;
    // Kind of each first-seen file, so a later hard-link alias can repeat it.
    let mut primary_files = HashMap::<PathBuf, Kind>::new();
    let mut buffer = Vec::with_capacity(CHUNK_BYTES);
    let mut files_done = 0u64;
    let mut bytes_done = 0u64;
    // Sources are in sorted walk order, which puts parents before children
    // and hard-link targets before aliases, as `Index::validate` requires.
    for source in &sources {
        observer.checkpoint()?;
        ensure!(!cancel.load(Ordering::Relaxed), "Store creation cancelled");
        let kind = if let Some(target) = &source.link {
            Kind::Symlink {
                target: target.clone(),
            }
        } else if source.stamp.mode & libc::S_IFMT == libc::S_IFDIR {
            Kind::Directory
        } else if let Some(target) = &source.hardlink_to {
            let kind = primary_files
                .get(target)
                .cloned()
                .context("Missing hard-link source")?;
            ensure!(
                kind.size() == Some(source.stamp.size),
                "Hard-link size changed"
            );
            kind
        } else if let Some(kind) = grouped.get(&source.path) {
            primary_files.insert(source.path.clone(), kind.clone());
            kind.clone()
        } else {
            // The file is opened beneath the anchored root and its stat is
            // compared with the snapshot before the first and after the last
            // read, so a swapped or rewritten file aborts the build.
            let mut file = anchor.open_with(
                &source.path,
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK,
            )?;
            ensure!(
                Stamp::from(&file.metadata()?) == source.stamp,
                "{} changed before packing",
                source.path.display()
            );
            let mut chunks = Vec::new();
            {
                let mut reader = BufReader::with_capacity(MIN_CHUNK_BYTES, &mut file);
                while read_content_chunk(&mut reader, &mut buffer)? {
                    observer.checkpoint()?;
                    ensure!(!cancel.load(Ordering::Relaxed), "Store creation cancelled");
                    let bytes = buffer.as_slice();
                    let id =
                        intern_chunk(&mut index, &mut known, bytes, options, None, &mut store)?;
                    chunks.push(id);
                }
            }
            ensure!(
                Stamp::from(&file.metadata()?) == source.stamp,
                "{} changed during packing",
                source.path.display()
            );
            let kind = Kind::File {
                size: source.stamp.size,
                chunks,
            };
            primary_files.insert(source.path.clone(), kind.clone());
            kind
        };
        if source.stamp.mode & libc::S_IFMT == libc::S_IFREG {
            files_done += 1;
            bytes_done = bytes_done.saturating_add(source.stamp.size);
            observer.progress(files_done, bytes_done, "Building Maximum Space store");
        }
        index.entries.push(Entry {
            path: source.path.clone(),
            mode: source.stamp.mode & 0o7777,
            modified_secs: source.stamp.modified.0,
            modified_nanos: u32::try_from(source.stamp.modified.1)?,
            xattrs: source.xattrs.clone(),
            hardlink_to: source.hardlink_to.clone(),
            kind,
        });
    }
    Ok((index, sources))
}

/// Reads all source files, sharing identical chunks inside this store.
/// Existing destinations and destinations inside the source are refused.
pub fn create(
    root: &Path,
    output: &Path,
    options: Options,
    cancel: &AtomicBool,
) -> Result<Summary> {
    create_observed(root, output, options, cancel, &NoObserver)
}

/// `create` with progress. Writes a version 6 monolithic store. Nothing
/// appears at `output` until every chunk has been read back and the source
/// has been walked again and found unchanged.
pub fn create_observed(
    root: &Path,
    output: &Path,
    options: Options,
    cancel: &AtomicBool,
    observer: &dyn Observer,
) -> Result<Summary> {
    ensure!(
        (1..=22).contains(&options.level)
            && options.compare_level.is_none_or(|n| (1..=22).contains(&n)),
        "Store levels must be from 1 to 22"
    );
    let root = crate::jobs::validate_folder(root)?;
    let (parent, target) = destination(output)?;
    let mut space = crate::storage::SpacePlan {
        retained_original: true,
        ..Default::default()
    };
    space.add(
        crate::storage::volume(&parent)?,
        crate::storage::pack_bound(&crate::storage::inventory(&root)?)?,
        "Verified store; source retained",
    )?;
    space.recheck()?;
    ensure!(
        !target.starts_with(&root),
        "Keep the store outside its source folder"
    );
    // Order: placeholder header, payloads, index, then the real header over
    // the placeholder, because the header records where the index landed.
    let mut staged = tempfile::NamedTempFile::new_in(&parent)?;
    staged.write_all(&[0; HEADER_BYTES as usize])?;
    let mut position = HEADER_BYTES;
    let (index, sources) = build_index(
        &root,
        options,
        cancel,
        None,
        observer,
        |raw, codec, encoded, hash| {
            staged.write_all(encoded)?;
            let chunk = Chunk {
                offset: position,
                stored: u32::try_from(encoded.len())?,
                raw,
                codec,
                hash,
            };
            position = position
                .checked_add(encoded.len() as u64)
                .context("Store size overflow")?;
            Ok(chunk)
        },
    )?;
    index.validate(position, 6)?;
    let bytes = serde_json::to_vec(&index)?;
    ensure!(
        bytes.len() as u64 <= MAX_INDEX,
        "Store index exceeds 32 MiB"
    );
    staged.write_all(&bytes)?;
    staged.seek(SeekFrom::Start(0))?;
    staged.write_all(MAGIC)?;
    staged.write_all(&6u32.to_le_bytes())?;
    staged.write_all(&(CHUNK_BYTES as u32).to_le_bytes())?;
    staged.write_all(&position.to_le_bytes())?;
    staged.write_all(&(bytes.len() as u64).to_le_bytes())?;
    staged.write_all(blake3::hash(&bytes).as_bytes())?;
    staged.as_file().sync_all()?;
    // Durable steps: sync the staged file, reopen and decode every chunk,
    // compare a fresh source snapshot, publish without replacement, then
    // sync the parent. An earlier failure drops the staged file.
    let reader = Reader::from_file(staged.reopen()?)?;
    observer.started(0, 0, "Verifying stored bytes");
    reader.verify_observed(cancel, observer)?;
    observer.started(0, 0, "Checking source files before publication");
    observer.checkpoint()?;
    ensure!(
        snapshot(&root, cancel)? == sources,
        "Source tree changed; retry after the game and launcher finish writing"
    );
    ensure!(!cancel.load(Ordering::Relaxed), "Store creation cancelled");
    staged.persist_noclobber(&target).map_err(|e| e.error)?;
    File::open(parent)?.sync_all()?;
    Ok(reader.summary().clone())
}

/// Creates or opens a pool. An existing folder must belong to this user and
/// hold only pool entries, and is then set to mode 0700.
fn pool_dir(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .context("Creating the shared chunk pool")?;
    }
    let path = path.canonicalize()?;
    ensure!(path.is_dir(), "The shared chunk pool must be a directory");
    ensure!(
        std::fs::symlink_metadata(&path)?.uid() == nix::unistd::geteuid().as_raw(),
        "The shared chunk pool must belong to the current user"
    );
    ensure_only_pool_entries(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

/// Fails unless everything in `pool` is something a pool holds.
///
/// A store records its pool's path, and that record can be edited. Without
/// this, compaction would change the mode of whatever folder it named and
/// pruning would delete that folder's `.tmp` files.
fn ensure_only_pool_entries(pool: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    for entry in std::fs::read_dir(pool)? {
        let entry = entry?;
        let name = entry.file_name();
        ensure!(
            std::fs::symlink_metadata(entry.path())?.is_file(),
            "{} holds something other than chunk files, so it is not a shared chunk pool",
            pool.display()
        );
        if name.as_bytes() == b".lock" || name.as_bytes().starts_with(b".tmp") {
            continue;
        }
        validate_pool_object(&entry.path()).with_context(|| {
            format!(
                "{} holds other files, so it is not a shared chunk pool",
                pool.display()
            )
        })?;
    }
    Ok(())
}

/// Takes the pool's exclusive lock, waiting for it if needed. Creation holds
/// it for a whole build and pruning for a whole pass, so pruning never sees
/// an object that a build has published but not yet linked into its store.
fn lock_pool(pool: &Path) -> Result<File> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(pool.join(".lock"))?;
    lock.lock()?;
    Ok(lock)
}

/// Puts one chunk object in the pool and returns its path. An object already
/// there is reused after its header is checked. Its payload is not reread.
/// The caller must hold the pool lock.
fn publish_object(pool: &Path, chunk: &Chunk, encoded: &[u8]) -> Result<PathBuf> {
    let target = pool.join(chunk_name(chunk));
    match std::fs::symlink_metadata(&target) {
        Ok(_) => {
            validate_object(&target, chunk)?;
            return Ok(target);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // Header layout matches `inspect_object` in format.rs. The object is
    // synced and made read-only before it is published without replacement.
    let mut staged = tempfile::NamedTempFile::new_in(pool)?;
    staged.write_all(OBJECT_MAGIC)?;
    staged.write_all(&codec_number(chunk.codec).to_le_bytes())?;
    staged.write_all(&chunk.raw.to_le_bytes())?;
    staged.write_all(&chunk.stored.to_le_bytes())?;
    staged.write_all(&chunk.hash)?;
    staged.write_all(&[0; 12])?;
    staged.write_all(encoded)?;
    staged.as_file().sync_all()?;
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o400))?;
    match staged.persist_noclobber(&target) {
        Ok(_) => File::open(pool)?.sync_all()?,
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_object(&target, chunk)?;
        }
        Err(error) => return Err(error.error.into()),
    }
    Ok(target)
}

/// Writes a version 7 manifest: the 64-byte header, then the index at offset 64.
fn write_manifest(path: &Path, index: &Index) -> Result<()> {
    index.validate(0, 7)?;
    let bytes = serde_json::to_vec(index)?;
    ensure!(
        bytes.len() as u64 <= MAX_INDEX,
        "Store index exceeds 32 MiB"
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(MAGIC)?;
    file.write_all(&7u32.to_le_bytes())?;
    file.write_all(&(CHUNK_BYTES as u32).to_le_bytes())?;
    file.write_all(&HEADER_BYTES.to_le_bytes())?;
    file.write_all(&(bytes.len() as u64).to_le_bytes())?;
    file.write_all(blake3::hash(&bytes).as_bytes())?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Creates a self-contained directory store whose chunk files share allocation
/// with other stores built from the same pool.
pub fn create_shared(
    root: &Path,
    output: &Path,
    pool: &Path,
    options: Options,
    cancel: &AtomicBool,
) -> Result<Summary> {
    create_shared_observed(root, output, pool, options, cancel, &NoObserver)
}

/// `create_shared` with progress. Writes a version 7 directory store. The
/// pool and the store's parent must be on one filesystem. A failed build
/// leaves its new objects in the pool with one link, for `prune_shared_pool`.
pub fn create_shared_observed(
    root: &Path,
    output: &Path,
    pool: &Path,
    options: Options,
    cancel: &AtomicBool,
    observer: &dyn Observer,
) -> Result<Summary> {
    ensure!(
        (1..=22).contains(&options.level)
            && options
                .compare_level
                .is_none_or(|level| (1..=22).contains(&level)),
        "Store levels must be from 1 to 22"
    );
    let root = crate::jobs::validate_folder(root)?;
    let (parent, target) = destination(output)?;
    let mut space = crate::storage::SpacePlan {
        retained_original: true,
        ..Default::default()
    };
    space.add(
        crate::storage::volume(&parent)?,
        crate::storage::pack_bound(&crate::storage::inventory(&root)?)?,
        "Verified store; source retained",
    )?;
    space.recheck()?;
    ensure!(
        !target.starts_with(&root),
        "Keep the store outside its source folder"
    );
    let pool = pool_dir(pool)?;
    let _pool_lock = lock_pool(&pool)?;
    ensure!(
        !pool.starts_with(&root) && !root.starts_with(&pool),
        "Keep the shared pool separate from the game folder"
    );
    ensure!(
        !pool.starts_with(&target) && !target.starts_with(&pool),
        "Keep the store outside its shared pool"
    );
    ensure!(
        std::fs::symlink_metadata(&parent)?.dev() == std::fs::symlink_metadata(&pool)?.dev(),
        "The shared pool and store must be on the same filesystem"
    );
    let staged = tempfile::Builder::new()
        .prefix(".flummox-shared-")
        .tempdir_in(&parent)?;
    let chunks_path = staged.path().join("chunks");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&chunks_path)?;
    let (index, sources) = build_index(
        &root,
        options,
        cancel,
        Some(&pool),
        observer,
        |raw, codec, encoded, hash| {
            let chunk = Chunk {
                offset: 0,
                stored: u32::try_from(encoded.len())?,
                raw,
                codec,
                hash,
            };
            // The store reads through its own link, so it stays complete if
            // the pool entry is later pruned or the pool is removed.
            let object = publish_object(&pool, &chunk, encoded)?;
            std::fs::hard_link(&object, chunks_path.join(chunk_name(&chunk)))?;
            Ok(chunk)
        },
    )?;
    write_manifest(&staged.path().join("manifest"), &index)?;
    let pool_record = PoolRecord { path: pool.clone() };
    let mut pool_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(staged.path().join("pool.json"))?;
    serde_json::to_writer(&mut pool_file, &pool_record)?;
    pool_file.sync_all()?;
    // Durable steps: sync both staged folders, reopen and decode every
    // chunk, compare a fresh source snapshot, rename the folder into place
    // without replacement, then sync the parent.
    File::open(&chunks_path)?.sync_all()?;
    File::open(staged.path())?.sync_all()?;
    let reader = Reader::open(staged.path())?;
    observer.started(0, 0, "Verifying stored bytes");
    reader.verify_observed(cancel, observer)?;
    observer.started(0, 0, "Checking source files before publication");
    observer.checkpoint()?;
    ensure!(
        snapshot(&root, cancel)? == sources,
        "Source tree changed; retry after the game and launcher finish writing"
    );
    ensure!(!cancel.load(Ordering::Relaxed), "Store creation cancelled");
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        staged.path(),
        rustix::fs::CWD,
        &target,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    File::open(parent)?.sync_all()?;
    Ok(reader.summary().clone())
}

/// What one `prune_shared_pool` pass removed.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct PoolPruneSummary {
    pub objects: u64,
    /// Allocated size of the removed files, from their block counts.
    pub reclaimed_bytes: u64,
}

/// Removes pool directory entries that no shared store still hard-links.
/// Refuses a folder that holds anything a pool would not, and holds the pool
/// lock for the whole pass. Leftover `.tmp` files are removed too.
pub fn prune_shared_pool(pool: &Path) -> Result<PoolPruneSummary> {
    use std::os::unix::ffi::OsStrExt;

    let pool = pool
        .canonicalize()
        .context("Finding the shared chunk pool")?;
    ensure!(pool.is_dir(), "The shared chunk pool must be a directory");
    ensure!(
        std::fs::symlink_metadata(&pool)?.uid() == nix::unistd::geteuid().as_raw(),
        "The shared chunk pool must belong to the current user"
    );
    ensure_only_pool_entries(&pool)?;
    let _pool_lock = lock_pool(&pool)?;
    let mut summary = PoolPruneSummary {
        objects: 0,
        reclaimed_bytes: 0,
    };
    for entry in std::fs::read_dir(&pool)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.as_bytes() == b".lock" {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        ensure!(metadata.is_file(), "Unexpected entry in the shared pool");
        // A link count of 1 means the pool's own name is the last one: every
        // store that linked this object has been deleted.
        if metadata.nlink() == 1 {
            if !name.as_bytes().starts_with(b".tmp") {
                validate_pool_object(&entry.path())?;
            }
            summary.objects = summary.objects.saturating_add(1);
            summary.reclaimed_bytes = summary
                .reclaimed_bytes
                .saturating_add(metadata.blocks().saturating_mul(512));
            std::fs::remove_file(entry.path())?;
        }
    }
    File::open(&pool)?.sync_all()?;
    Ok(summary)
}

/// Picks the smallest encoding among the configured levels. All-zero input
/// stores nothing, and input zstd cannot shrink is stored raw, so a payload
/// is never longer than its chunk.
fn encode(bytes: &[u8], options: Options) -> Result<(Codec, Vec<u8>)> {
    if bytes.iter().all(|byte| *byte == 0) {
        return Ok((Codec::Zero, Vec::new()));
    }
    let mut encoded = zstd::bulk::compress(bytes, options.level)?;
    let levels: &[i32] = if options.compare_level == Some(22) {
        &[15, 19, 22]
    } else {
        &[]
    };
    for level in levels
        .iter()
        .copied()
        .chain(options.compare_level.filter(|level| *level != 22))
        .filter(|level| *level != options.level)
    {
        let alternate = zstd::bulk::compress(bytes, level)?;
        if alternate.len() < encoded.len() {
            encoded = alternate;
        }
    }
    if encoded.len() < bytes.len() {
        Ok((Codec::Zstd, encoded))
    } else {
        Ok((Codec::Raw, bytes.to_vec()))
    }
}

/// Compares grouped and separate encoding on a bounded sample, without writing a store.
pub fn sample_small_files(
    root: &Path,
    inventory: &crate::inventory::Inventory,
    budget: u64,
    cancel: &AtomicBool,
    observer: &dyn Observer,
) -> Result<SmallFileSample> {
    let anchor = crate::safeio::Anchor::open(root)?;
    ensure!(
        anchor.fully_resolved(),
        "Safe path resolution is unavailable"
    );
    let mut files: Vec<_> = inventory
        .files
        .iter()
        .filter(|file| (1..SMALL_FILE_LIMIT).contains(&file.size))
        .collect();
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    let mut sources = Vec::new();
    let mut bytes = 0u64;
    let mut seen = HashSet::new();
    for entry in files {
        observer.checkpoint()?;
        ensure!(
            !cancel.load(Ordering::Relaxed),
            "Small-file sampling stopped"
        );
        if bytes.saturating_add(entry.size) > budget {
            continue;
        }
        let file = anchor.open_file(&entry.rel)?;
        ensure!(
            entry.matches_file(&file)?,
            "File changed during small-file sampling"
        );
        let metadata = file.metadata()?;
        if !seen.insert((metadata.dev(), metadata.ino())) {
            continue;
        }
        sources.push(Source {
            path: entry.rel.clone(),
            stamp: Stamp::from(&metadata),
            link: None,
            xattrs: vec![],
            hardlink_to: None,
        });
        bytes += entry.size;
    }
    let mut index = Index {
        entries: vec![],
        chunks: vec![],
    };
    let mut known = HashMap::new();
    let options = Options {
        level: 19,
        compare_level: None,
    };
    let groups = group_small_files(
        GroupParams {
            anchor: &anchor,
            options,
            cancel,
            pool: None,
            observer,
        },
        &sources,
        &mut index,
        &mut known,
        &mut |raw, codec, encoded, hash| {
            Ok(Chunk {
                raw,
                codec,
                stored: u32::try_from(encoded.len())?,
                offset: 0,
                hash,
            })
        },
    )?;
    let mut separate = 0u64;
    for source in &sources {
        if !groups.contains_key(&source.path) {
            continue;
        }
        observer.checkpoint()?;
        ensure!(
            !cancel.load(Ordering::Relaxed),
            "Small-file sampling stopped"
        );
        let mut file = anchor.open_file(&source.path)?;
        let mut data = vec![0; usize::try_from(source.stamp.size)?];
        file.read_exact(&mut data)?;
        ensure!(
            Stamp::from(&file.metadata()?) == source.stamp,
            "File changed during small-file sampling"
        );
        separate += encode(&data, options)?.1.len() as u64;
    }
    let grouped: u64 = index
        .chunks
        .iter()
        .map(|chunk| u64::from(chunk.stored))
        .sum();
    Ok(SmallFileSample {
        files: sources.len() as u64,
        bytes,
        grouped_files: groups.len() as u64,
        extra_payload_saving: separate.saturating_sub(grouped),
    })
}

/// Result of `sample_small_files`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmallFileSample {
    /// Small files read, after the byte budget and one per inode.
    pub files: u64,
    pub bytes: u64,
    /// Files that grouping placed in a shared frame.
    pub grouped_files: u64,
    /// Payload bytes of those files encoded separately, minus their frames.
    pub extra_payload_saving: u64,
}
