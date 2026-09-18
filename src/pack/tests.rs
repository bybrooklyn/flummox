use super::*;
use crate::testutil::{Ctx, TestResult, check, check_eq};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        ffi::OsStringExt,
        fs::{MetadataExt, PermissionsExt, symlink},
    },
    path::Path,
    sync::atomic::AtomicBool,
};

#[test]
fn maximum_policy_survives_update_compaction() -> TestResult {
    check_eq(
        Options::maximum(),
        Options {
            level: 9,
            compare_level: Some(22),
        },
        "Maximum Space always compares the full high-level set",
    )
}

fn noise(count: usize) -> Vec<u8> {
    let mut seed = 42u64;
    (0..count)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        })
        .collect()
}

#[test]
fn round_trip_preserves_bytes_modes_links_and_non_utf8_paths() -> TestResult {
    let temp = tempfile::tempdir().ctx("fixture")?;
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("data/empty")).ctx("directories")?;
    let mut bytes = noise(256 * 1024).repeat(20);
    bytes.extend_from_slice(b"last bytes");
    fs::write(source.join("data/main.bin"), &bytes).ctx("main file")?;
    fs::write(source.join("duplicate.bin"), &bytes).ctx("duplicate")?;
    fs::hard_link(source.join("data/main.bin"), source.join("hard-copy.bin")).ctx("hard link")?;
    xattr::set(
        source.join("data/main.bin"),
        "user.flummox-test",
        b"metadata survives",
    )
    .ctx("xattr")?;
    fs::write(source.join("empty"), []).ctx("empty file")?;
    fs::write(source.join("zero-filled"), vec![0; CHUNK_BYTES * 2]).ctx("zero file")?;
    let raw_name = std::ffi::OsString::from_vec(vec![b'n', 0xff]);
    fs::write(source.join(&raw_name), b"non utf8 filename").ctx("filename")?;
    fs::write(
        source.join("play"),
        b"#!/bin/sh\nprintf 'fixture-game-ok\\n'\n",
    )
    .ctx("executable")?;
    fs::set_permissions(source.join("play"), fs::Permissions::from_mode(0o755))
        .ctx("executable mode")?;
    fs::set_permissions(
        source.join("data/empty"),
        fs::Permissions::from_mode(0o1750),
    )
    .ctx("special mode")?;
    symlink("../duplicate.bin", source.join("data/link")).ctx("internal relative link")?;
    let output = temp.path().join("game.flumpack");
    let cancel = AtomicBool::new(false);
    let report = create(&source, &output, Options::default(), &cancel).ctx("create")?;
    check(
        report.archive_bytes < report.logical_bytes / 3,
        "distant repetition and duplicates shrink with index included",
    )?;
    check(
        report.duplicate_bytes >= bytes.len() as u64,
        "duplicate encoded files share chunks",
    )?;
    check(report.zero_bytes > 0, "zero chunks need no archive payload")?;
    check_eq(
        report.raw_bytes + report.compressed_input_bytes + report.zero_bytes,
        report.logical_bytes - report.duplicate_bytes,
        "codec inputs account for every unique source byte",
    )?;
    check_eq(
        report.metadata_bytes + report.raw_bytes + report.compressed_bytes,
        report.archive_bytes,
        "stored payload and metadata account for the archive",
    )?;
    let reader = Reader::open(&output).ctx("open")?;
    for offset in [
        0,
        CHUNK_BYTES as u64 - 17,
        bytes.len() as u64 - 5,
        bytes.len() as u64,
        u64::MAX,
    ] {
        let read = reader
            .read(Path::new("data/main.bin"), offset, 128)
            .ctx("random read")?;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let expected = bytes
            .get(start..(start + 128).min(bytes.len()))
            .ctx("expected slice")?;
        check_eq(read.as_slice(), expected, "cross-frame reads and EOF")?;
    }
    let restored = temp.path().join("restored");
    restore(&output, &restored, &cancel).ctx("restore")?;
    check_eq(
        fs::read(restored.join("data/main.bin")).ctx("restored payload")?,
        bytes.clone(),
        "all source bytes",
    )?;
    check_eq(
        fs::read(restored.join("duplicate.bin")).ctx("restored duplicate")?,
        bytes.clone(),
        "deduplicated bytes",
    )?;
    check_eq(
        fs::metadata(restored.join("data/main.bin"))
            .ctx("hard-link source")?
            .ino(),
        fs::metadata(restored.join("hard-copy.bin"))
            .ctx("hard-link alias")?
            .ino(),
        "hard-link identity",
    )?;
    check_eq(
        xattr::get(restored.join("hard-copy.bin"), "user.flummox-test").ctx("restored xattr")?,
        Some(b"metadata survives".to_vec()),
        "extended attribute",
    )?;
    check_eq(
        fs::read(source.join("data/main.bin")).ctx("source")?,
        bytes,
        "source retained",
    )?;
    check_eq(
        fs::read(restored.join(&raw_name)).ctx("non utf8")?,
        b"non utf8 filename".to_vec(),
        "name preserved",
    )?;
    check_eq(
        fs::read_link(restored.join("data/link")).ctx("link")?,
        Path::new("../duplicate.bin").to_path_buf(),
        "link preserved",
    )?;
    check(
        restored.join("data/empty").is_dir(),
        "empty directory preserved",
    )?;
    check_eq(
        fs::metadata(restored.join("data/empty"))
            .ctx("special mode")?
            .mode()
            & 0o7777,
        0o1750,
        "special permission bits",
    )?;
    let zero = fs::metadata(restored.join("zero-filled")).ctx("zero metadata")?;
    check_eq(zero.len(), (CHUNK_BYTES * 2) as u64, "zero file length")?;
    check(
        zero.blocks() * 512 < zero.len() / 8,
        "whole zero chunks restore as sparse holes",
    )?;
    check_eq(
        fs::metadata(restored.join("play"))
            .ctx("mode")?
            .permissions()
            .mode()
            & 0o777,
        0o755,
        "executable bit",
    )?;
    check_eq(
        fs::metadata(restored.join("data/main.bin"))
            .ctx("mtime")?
            .modified()
            .ctx("mtime")?,
        fs::metadata(source.join("data/main.bin"))
            .ctx("source mtime")?
            .modified()
            .ctx("source mtime")?,
        "mtime preserved",
    )
}

#[test]
fn content_defined_chunks_reuse_data_after_an_insertion() -> TestResult {
    let temp = tempfile::tempdir().ctx("fixture")?;
    let source = temp.path().join("source");
    fs::create_dir(&source).ctx("source")?;
    let base = noise(12 * 1024 * 1024);
    fs::write(source.join("base.bin"), &base).ctx("base")?;
    let mut shifted = vec![b'x'; 64 * 1024];
    shifted.extend_from_slice(&base);
    fs::write(source.join("shifted.bin"), shifted).ctx("shifted")?;
    let output = temp.path().join("store");
    let report = create(
        &source,
        &output,
        Options::default(),
        &AtomicBool::new(false),
    )
    .ctx("create")?;
    check(
        report.duplicate_bytes > 6 * 1024 * 1024,
        format!("shifted content should converge on shared boundaries: {report:?}"),
    )?;
    let reader = Reader::open(&output).ctx("reader")?;
    check_eq(
        reader
            .read(Path::new("shifted.bin"), 64 * 1024, CHUNK_BYTES)
            .ctx("shifted read")?,
        base.get(..CHUNK_BYTES).ctx("expected bytes")?.to_vec(),
        "random reads use variable chunk offsets",
    )
}

#[test]
fn version_one_fixed_chunk_stores_remain_readable() -> TestResult {
    use format::{Chunk, Codec, HEADER_BYTES, Index, MAGIC};

    let temp = tempfile::tempdir().ctx("fixture")?;
    let path = temp.path().join("version-one.store");
    let data = noise(CHUNK_BYTES + 257);
    let mut file = fs::File::create(&path).ctx("store")?;
    file.write_all(&[0; HEADER_BYTES as usize])
        .ctx("header placeholder")?;
    let mut position = HEADER_BYTES;
    let mut chunks = Vec::new();
    let mut ids = Vec::new();
    for bytes in data.chunks(CHUNK_BYTES) {
        let id = u32::try_from(chunks.len()).ctx("chunk id")?;
        ids.push(id);
        file.write_all(bytes).ctx("payload")?;
        chunks.push(Chunk {
            offset: position,
            stored: u32::try_from(bytes.len()).ctx("stored length")?,
            raw: u32::try_from(bytes.len()).ctx("raw length")?,
            codec: Codec::Raw,
            hash: *blake3::hash(bytes).as_bytes(),
        });
        position += bytes.len() as u64;
    }
    let index = Index {
        entries: vec![
            Entry {
                path: Path::new("").into(),
                mode: 0o755,
                modified_secs: 0,
                modified_nanos: 0,
                xattrs: Vec::new(),
                hardlink_to: None,
                kind: Kind::Directory,
            },
            Entry {
                path: Path::new("data").into(),
                mode: 0o644,
                modified_secs: 0,
                modified_nanos: 0,
                xattrs: Vec::new(),
                hardlink_to: None,
                kind: Kind::File {
                    size: data.len() as u64,
                    chunks: ids,
                },
            },
        ],
        chunks,
    };
    index.validate(position, 1).ctx("version one index")?;
    let encoded = serde_json::to_vec(&index).ctx("index JSON")?;
    file.write_all(&encoded).ctx("index")?;
    file.seek(SeekFrom::Start(0)).ctx("header seek")?;
    file.write_all(MAGIC).ctx("magic")?;
    file.write_all(&1u32.to_le_bytes()).ctx("version")?;
    file.write_all(&(CHUNK_BYTES as u32).to_le_bytes())
        .ctx("chunk size")?;
    file.write_all(&position.to_le_bytes())
        .ctx("index offset")?;
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .ctx("index length")?;
    file.write_all(blake3::hash(&encoded).as_bytes())
        .ctx("index hash")?;
    file.sync_all().ctx("store sync")?;

    let reader = Reader::open(&path).ctx("version one reader")?;
    let offset = CHUNK_BYTES as u64 - 32;
    check_eq(
        reader.read(Path::new("data"), offset, 128).ctx("read")?,
        data.get(CHUNK_BYTES - 32..CHUNK_BYTES + 96)
            .ctx("expected")?
            .to_vec(),
        "fixed-chunk random reads remain compatible",
    )
}

#[test]
fn shared_stores_reuse_pool_objects_and_remain_self_contained() -> TestResult {
    let temp = tempfile::tempdir().ctx("fixture")?;
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    let pool = temp.path().join("pool");
    let first_store = temp.path().join("first.flumpack");
    let second_store = temp.path().join("second.flumpack");
    fs::create_dir(&first).ctx("first source")?;
    fs::create_dir(&second).ctx("second source")?;
    let shared = b"common game payload".repeat(300_000);
    fs::write(first.join("common.bin"), &shared).ctx("first payload")?;
    fs::write(first.join("first-only.bin"), noise(700_000)).ctx("first unique payload")?;
    fs::write(second.join("common.bin"), &shared).ctx("second payload")?;
    fs::write(second.join("unique.bin"), b"second game only").ctx("unique payload")?;
    let cancel = AtomicBool::new(false);
    let first_report = create_shared(&first, &first_store, &pool, Options::default(), &cancel)
        .ctx("first shared store")?;
    check_eq(
        first_report.shared_bytes,
        0,
        "the first store seeds the pool",
    )?;
    let second_report = create_shared(&second, &second_store, &pool, Options::default(), &cancel)
        .ctx("second shared store")?;
    check(
        second_report.shared_bytes > 0,
        "the second store shares existing chunk allocation",
    )?;
    check_eq(
        second_report.metadata_bytes + second_report.raw_bytes + second_report.compressed_bytes,
        second_report.archive_bytes,
        "directory-store files are fully accounted",
    )?;
    check(
        Reader::open(&first_store)
            .ctx("reopen first")?
            .summary()
            .shared_bytes
            > 0,
        "both stores report chunks shared with another game",
    )?;
    fs::remove_dir_all(&first_store).ctx("remove first store")?;
    let pruned = prune_shared_pool(&pool).ctx("prune pool")?;
    check(
        pruned.objects > 0,
        "unreferenced pool objects are reclaimed",
    )?;
    fs::remove_dir_all(&pool).ctx("remove pool links")?;
    let restored = temp.path().join("restored");
    restore(&second_store, &restored, &cancel).ctx("restore without pool")?;
    check_eq(
        fs::read(restored.join("common.bin")).ctx("restored shared payload")?,
        shared,
        "store hard links remain independently readable",
    )
}

#[test]
fn corruption_truncation_and_cancellation_never_publish_a_restore() -> TestResult {
    let temp = tempfile::tempdir().ctx("fixture")?;
    let source = temp.path().join("source");
    fs::create_dir(&source).ctx("source")?;
    fs::write(source.join("noise"), noise(128 * 1024)).ctx("noise control")?;
    let output = temp.path().join("store");
    let cancel = AtomicBool::new(false);
    let report = create(&source, &output, Options::default(), &cancel).ctx("create")?;
    check(
        report.archive_bytes > report.logical_bytes,
        "incompressible control includes metadata overhead",
    )?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&output)
        .ctx("fault injection")?;
    file.seek(SeekFrom::Start(format::HEADER_BYTES))
        .ctx("first payload")?;
    let mut first = [0];
    file.read_exact(&mut first).ctx("read byte")?;
    file.seek(SeekFrom::Start(format::HEADER_BYTES))
        .ctx("payload")?;
    file.write_all(&[first.first().copied().ctx("byte")? ^ 1])
        .ctx("corrupt")?;
    let reader = Reader::open(&output).ctx("valid index with damaged payload")?;
    check(reader.verify(&cancel).is_err(), "bit rot is detected")?;
    let destination = temp.path().join("restored");
    check(
        restore(&output, &destination, &cancel).is_err(),
        "corrupt restore fails",
    )?;
    check(!destination.exists(), "incomplete restore never published")?;
    file.set_len(report.archive_bytes - 1).ctx("truncate")?;
    check(Reader::open(&output).is_err(), "truncated index refused")?;
    let cancelled = AtomicBool::new(true);
    let stopped = temp.path().join("stopped");
    check(
        create(&source, &stopped, Options::default(), &cancelled).is_err(),
        "cancelled build fails",
    )?;
    check(!stopped.exists(), "cancelled build never published")
}

#[test]
fn existing_destinations_and_escaping_links_are_preserved_or_rejected() -> TestResult {
    let temp = tempfile::tempdir().ctx("fixture")?;
    let source = temp.path().join("source");
    fs::create_dir(&source).ctx("source")?;
    fs::write(source.join("data"), b"fixture").ctx("data")?;
    let output = temp.path().join("store");
    let cancel = AtomicBool::new(false);
    create(&source, &output, Options::default(), &cancel).ctx("create")?;
    let original = fs::read(&output).ctx("original")?;
    check(
        create(&source, &output, Options::default(), &cancel).is_err(),
        "existing store refused",
    )?;
    check_eq(
        fs::read(&output).ctx("retained")?,
        original,
        "existing store preserved",
    )?;
    check(
        restore(&output, &source, &cancel).is_err(),
        "existing directory refused",
    )?;
    check(
        create(
            &source,
            &source.join("nested.store"),
            Options::default(),
            &cancel,
        )
        .is_err(),
        "recursive store refused",
    )?;
    let outside = temp.path().join("outside");
    fs::write(&outside, b"untouched").ctx("outside")?;
    symlink(&outside, temp.path().join("alias")).ctx("destination symlink")?;
    check(
        restore(&output, &temp.path().join("alias"), &cancel).is_err(),
        "symlink destination refused",
    )?;
    symlink("../outside", source.join("escape")).ctx("source escape")?;
    check(
        create(
            &source,
            &temp.path().join("escape.store"),
            Options::default(),
            &cancel,
        )
        .is_err(),
        "escaping source symlink refused",
    )?;
    check_eq(
        fs::read(outside).ctx("outside after")?,
        b"untouched".to_vec(),
        "outside data untouched",
    )
}

#[test]
fn authenticated_indexes_still_require_safe_paths_and_bounded_chunks() -> TestResult {
    use format::{Chunk, Codec, Index};
    let root = Entry {
        path: Path::new("").into(),
        mode: 0o755,
        modified_secs: 0,
        modified_nanos: 0,
        xattrs: Vec::new(),
        hardlink_to: None,
        kind: Kind::Directory,
    };
    let file = Entry {
        path: Path::new("../escape").into(),
        kind: Kind::File {
            size: 1,
            chunks: vec![0],
        },
        ..root.clone()
    };
    let mut index = Index {
        entries: vec![root, file],
        chunks: vec![Chunk {
            offset: 64,
            stored: 1,
            raw: 1,
            codec: Codec::Raw,
            hash: [0; 32],
        }],
    };
    check(
        index.validate(65, 1).is_err(),
        "parent escape rejected even with correct index checksum",
    )?;
    index.entries.get_mut(1).ctx("entry")?.path = Path::new("file").into();
    index.validate(65, 1).ctx("valid control")?;
    index.chunks.first_mut().ctx("chunk")?.raw = u32::MAX;
    check(index.validate(65, 1).is_err(), "decoded allocation bounded")?;
    index.chunks.first_mut().ctx("chunk")?.raw = 1;
    index.chunks.first_mut().ctx("chunk")?.offset = u64::MAX;
    check(index.validate(65, 1).is_err(), "payload offset bounded")
}

proptest::proptest! {
    #[test]
    fn arbitrary_store_bytes_are_rejected_without_panicking(bytes in proptest::collection::vec(proptest::num::u8::ANY,0..1024)) {
        let result = (|| -> TestResult {
            let mut file = tempfile::tempfile().ctx("fixture")?;
            file.write_all(&bytes).ctx("input")?;
            check(Reader::from_file(file).is_err(),"random bytes cannot authenticate a store")
        })();
        result.map_err(proptest::test_runner::TestCaseError::fail)?;
    }
}
