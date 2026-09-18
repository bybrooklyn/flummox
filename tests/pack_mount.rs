//! Kernel-backed checks for the experimental read-only store view.
#![cfg(feature = "pack-mount")]

use flummox::{
    pack,
    testutil::{Ctx, TestResult, check, check_eq},
};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
    sync::atomic::AtomicBool,
};

#[test]
fn mounted_store_supports_reads_execution_and_refuses_writes() -> TestResult {
    if !Path::new("/dev/fuse").exists() {
        check(
            std::env::var_os("FLUMMOX_REQUIRE_FUSE").is_none(),
            "FUSE is required for this test run",
        )?;
        eprintln!("skipped: pack mount requires /dev/fuse");
        return Ok(());
    }
    let temp = tempfile::tempdir().ctx("fixture")?;
    let source = temp.path().join("source");
    let target = temp.path().join("view");
    fs::create_dir(&source).ctx("source")?;
    fs::create_dir(&target).ctx("mount target")?;
    let mut bytes = vec![b'a'; pack::CHUNK_BYTES];
    bytes.extend_from_slice(b"boundary");
    fs::write(source.join("data"), &bytes).ctx("payload")?;
    fs::hard_link(source.join("data"), source.join("data-copy")).ctx("base hard link")?;
    xattr::set(source.join("data"), "user.flummox-test", b"base attribute").ctx("base xattr")?;
    fs::write(
        source.join("play"),
        b"#!/bin/sh\nprintf 'fixture-game-ok\\n'\n",
    )
    .ctx("game fixture")?;
    fs::set_permissions(source.join("play"), fs::Permissions::from_mode(0o755))
        .ctx("executable")?;
    symlink("data", source.join("link")).ctx("symlink")?;
    let store = temp.path().join("game.flumpack");
    pack::create_shared(
        &source,
        &store,
        &temp.path().join("pool"),
        pack::Options::default(),
        &AtomicBool::new(false),
    )
    .ctx("store")?;
    let session = pack::mount::mount(&store, &target, None).ctx("mount")?;
    let names: Result<Vec<_>, _> = fs::read_dir(&target)
        .ctx("directory")?
        .map(|e| e.map(|e| e.file_name()))
        .collect();
    check_eq(
        names.ctx("directory names")?.len(),
        4,
        "directory enumeration",
    )?;
    check_eq(
        fs::read(target.join("data")).ctx("mounted read")?,
        bytes.clone(),
        "mounted bytes",
    )?;
    check_eq(
        fs::metadata(target.join("data"))
            .ctx("mounted hard-link source")?
            .ino(),
        fs::metadata(target.join("data-copy"))
            .ctx("mounted hard-link alias")?
            .ino(),
        "base hard-link identity reaches the mount",
    )?;
    check_eq(
        xattr::get(target.join("data-copy"), "user.flummox-test").ctx("mounted xattr")?,
        Some(b"base attribute".to_vec()),
        "base extended attributes reach the mount",
    )?;
    check_eq(
        fs::read(target.join("link")).ctx("symlink read")?,
        bytes,
        "mounted link",
    )?;
    let mut file = fs::File::open(target.join("data")).ctx("random read")?;
    file.seek(SeekFrom::Start(pack::CHUNK_BYTES as u64 - 1))
        .ctx("seek")?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ctx("read boundary")?;
    check_eq(tail, b"aboundary".to_vec(), "cross-frame kernel read")?;
    let child = std::process::Command::new(target.join("play"))
        .output()
        .ctx("run fixture from store")?;
    check(
        child.status.success(),
        "fixture executes from compressed storage",
    )?;
    check_eq(
        child.stdout,
        b"fixture-game-ok\n".to_vec(),
        "fixture output",
    )?;
    check(
        fs::write(target.join("data"), b"changed").is_err(),
        "writes refused",
    )?;
    check(
        fs::write(target.join("new"), b"new").is_err(),
        "creation refused",
    )?;
    drop(file);
    session.umount_and_join().ctx("unmount")?;
    check(
        fs::read_dir(&target)
            .ctx("unmounted directory")?
            .next()
            .is_none(),
        "mountpoint restored to empty",
    )
}

#[test]
fn writable_layer_survives_remount_and_keeps_the_store_immutable() -> TestResult {
    if !Path::new("/dev/fuse").exists() {
        check(
            std::env::var_os("FLUMMOX_REQUIRE_FUSE").is_none(),
            "FUSE is required for this test run",
        )?;
        eprintln!("skipped: writable pack mount requires /dev/fuse");
        return Ok(());
    }
    let temp = tempfile::tempdir().ctx("fixture")?;
    let source = temp.path().join("source");
    let target = temp.path().join("view");
    let writes = temp.path().join("writes");
    fs::create_dir(&source).ctx("source")?;
    fs::create_dir(&target).ctx("target")?;
    fs::write(source.join("data"), b"original compressed bytes").ctx("base file")?;
    xattr::set(
        source.join("data"),
        "user.flummox-test",
        b"original attribute",
    )
    .ctx("base xattr")?;
    fs::write(source.join("obsolete"), b"remove me").ctx("deleted base file")?;
    fs::create_dir(source.join("assets")).ctx("base directory")?;
    fs::write(source.join("assets/kept"), b"base asset").ctx("base asset")?;
    fs::write(source.join("assets/deleted"), b"old asset").ctx("old asset")?;
    let store = temp.path().join("game.flumpack");
    pack::create(
        &source,
        &store,
        pack::Options::default(),
        &AtomicBool::new(false),
    )
    .ctx("store")?;
    {
        let session = pack::mount::mount(&store, &target, Some(&writes)).ctx("writable mount")?;
        check(
            nix::sys::statvfs::statvfs(&target)
                .ctx("mounted capacity")?
                .blocks_available()
                > 0,
            "launcher sees update-layer free space",
        )?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(target.join("data"))
            .ctx("copy-up open")?;
        file.seek(SeekFrom::Start(9)).ctx("copy-up seek")?;
        std::io::Write::write_all(&mut file, b"updated").ctx("copy-up write")?;
        file.sync_all().ctx("sync update")?;
        drop(file);
        xattr::set(
            target.join("data"),
            "user.flummox-test",
            b"updated attribute",
        )
        .ctx("update xattr")?;
        fs::write(target.join("download.tmp"), b"new launcher payload")
            .ctx("launcher temp file")?;
        fs::rename(target.join("download.tmp"), target.join("replacement"))
            .ctx("atomic launcher rename")?;
        fs::create_dir(target.join("runtime")).ctx("new directory")?;
        fs::write(target.join("runtime/state"), b"save data").ctx("new nested file")?;
        fs::hard_link(
            target.join("runtime/state"),
            target.join("runtime/state-copy"),
        )
        .ctx("launcher hard link")?;
        check_eq(
            fs::metadata(target.join("runtime/state"))
                .ctx("hard-link source")?
                .ino(),
            fs::metadata(target.join("runtime/state-copy"))
                .ctx("hard-link target")?
                .ino(),
            "write layer preserves hard-link identity",
        )?;
        fs::remove_file(target.join("obsolete")).ctx("base deletion")?;
        fs::write(target.join("assets/kept"), b"updated asset").ctx("update base child")?;
        fs::remove_file(target.join("assets/deleted")).ctx("delete base child")?;
        fs::write(target.join("assets/new"), b"new asset").ctx("new child")?;
        fs::create_dir(target.join("assets-v2")).ctx("empty rename destination")?;
        fs::rename(target.join("assets"), target.join("assets-v2"))
            .ctx("rename merged base directory")?;
        check(
            !target.join("assets").exists(),
            "renamed base directory is hidden",
        )?;
        check_eq(
            fs::read(target.join("assets-v2/kept")).ctx("renamed updated child")?,
            b"updated asset".to_vec(),
            "directory rename carries an updated base child",
        )?;
        check(
            pack::commit_updates(
                &store,
                &writes,
                &temp.path().join("while-mounted.flumpack"),
                Some(temp.path()),
                pack::Options::default(),
                &AtomicBool::new(false),
            )
            .is_err(),
            "a live update layer cannot be committed",
        )?;
        session.umount_and_join().ctx("first unmount")?;
    }
    check_eq(
        fs::read(source.join("data")).ctx("source bytes")?,
        b"original compressed bytes".to_vec(),
        "source fixture unchanged",
    )?;
    check_eq(
        pack::Reader::open(&store)
            .ctx("base store")?
            .read(Path::new("data"), 0, 64)
            .ctx("base read")?,
        b"original compressed bytes".to_vec(),
        "compressed store immutable",
    )?;
    {
        let session = pack::mount::mount(&store, &target, Some(&writes)).ctx("recovered mount")?;
        check_eq(
            fs::read(target.join("data")).ctx("updated data")?,
            b"original updatedsed bytes".to_vec(),
            "copy-up update recovered",
        )?;
        check_eq(
            xattr::get(target.join("data"), "user.flummox-test").ctx("recovered xattr")?,
            Some(b"updated attribute".to_vec()),
            "extended attribute update recovered",
        )?;
        check_eq(
            fs::read(target.join("replacement")).ctx("replacement")?,
            b"new launcher payload".to_vec(),
            "rename recovered",
        )?;
        check_eq(
            fs::read(target.join("runtime/state")).ctx("nested file")?,
            b"save data".to_vec(),
            "new tree recovered",
        )?;
        check_eq(
            fs::read(target.join("runtime/state-copy")).ctx("hard link")?,
            b"save data".to_vec(),
            "hard link recovered",
        )?;
        check(
            !target.join("obsolete").exists(),
            "deletion tombstone recovered",
        )?;
        check_eq(
            fs::read(target.join("assets-v2/kept")).ctx("remounted directory child")?,
            b"updated asset".to_vec(),
            "directory rename survives remount",
        )?;
        check_eq(
            fs::read(target.join("assets-v2/new")).ctx("remounted new child")?,
            b"new asset".to_vec(),
            "new child survives directory rename",
        )?;
        check(
            !target.join("assets-v2/deleted").exists(),
            "deleted child stays hidden after directory rename",
        )?;
        session.umount_and_join().ctx("second unmount")?;
    }
    check(
        writes.join("state.json").is_file(),
        "durable update journal",
    )?;
    check(
        writes.join("files/data").is_file(),
        "only modified data copied up",
    )?;
    check(
        !writes.join("files/obsolete").exists(),
        "deleted base data was never copied",
    )?;
    let replacement = temp.path().join("replacement.flumpack");
    pack::commit_updates(
        &store,
        &writes,
        &replacement,
        Some(temp.path()),
        pack::Options::default(),
        &AtomicBool::new(false),
    )
    .ctx("commit stopped layer")?;
    let committed = pack::Reader::open(&replacement).ctx("replacement store")?;
    check_eq(
        committed
            .read(Path::new("data"), 0, 64)
            .ctx("committed update")?,
        b"original updatedsed bytes".to_vec(),
        "updated file reaches replacement store",
    )?;
    check(
        committed.entry(Path::new("data")).is_some_and(|entry| {
            entry.xattrs.iter().any(|attribute| {
                attribute.name == b"user.flummox-test" && attribute.value == b"updated attribute"
            })
        }),
        "extended attribute reaches replacement store",
    )?;
    check(
        committed.entry(Path::new("obsolete")).is_none(),
        "deletion reaches replacement store",
    )?;
    check_eq(
        committed
            .read(Path::new("runtime/state"), 0, 64)
            .ctx("committed new file")?,
        b"save data".to_vec(),
        "new file reaches replacement store",
    )?;
    check_eq(
        committed
            .read(Path::new("assets-v2/kept"), 0, 64)
            .ctx("committed renamed child")?,
        b"updated asset".to_vec(),
        "renamed directory reaches replacement store",
    )?;
    check(
        committed.entry(Path::new("assets")).is_none()
            && committed.entry(Path::new("assets-v2/deleted")).is_none(),
        "renamed and deleted directory paths reach replacement store",
    )?;
    check(
        store.is_file() && writes.is_dir(),
        "commit retains rollback data",
    )
}
