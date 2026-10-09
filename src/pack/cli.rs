//! Explicit commands for verified Maximum Space stores.

use anyhow::{Context, Result};
use clap::Subcommand;
use std::{path::PathBuf, sync::atomic::AtomicBool, time::Instant};

// The `///` lines in this enum are the help text clap prints.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create and check a new store; every source file stays.
    Create {
        folder: PathBuf,
        store: PathBuf,
        /// Share chunk allocation with other directory stores in this pool.
        #[arg(long)]
        pool: Option<PathBuf>,
        #[arg(long, default_value_t = 9)]
        level: i32,
        /// Compare levels 9, 15, 19 and 22 per unique chunk.
        #[arg(long, conflicts_with = "level")]
        maximum: bool,
    },
    /// Inspect index sizes; payload verification is a separate command.
    Info { store: PathBuf },
    /// Verify every unique chunk, including data not read during play.
    Verify { store: PathBuf },
    /// Decompress a store to a new folder, refusing to replace existing data.
    Restore { store: PathBuf, folder: PathBuf },
    /// Fold a stopped update layer into a new checked store.
    Commit {
        store: PathBuf,
        writes: PathBuf,
        new_store: PathBuf,
        #[arg(long)]
        scratch_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 9)]
        level: i32,
        #[arg(long, conflicts_with = "level")]
        maximum: bool,
    },
    /// Measure a fully verified temporary store, including all metadata.
    Benchmark {
        folder: PathBuf,
        /// Parent directory for the temporary store; defaults to system temp.
        #[arg(long)]
        scratch_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 9)]
        level: i32,
        #[arg(long, conflicts_with = "level")]
        maximum: bool,
        /// Verified 32 KiB wimlib LZX helper for a same-corpus WOF proxy.
        #[arg(long)]
        wof_lzx_helper: Option<PathBuf>,
    },
    /// Mount read-only, or add --writes for persistent copy-on-write updates.
    Mount {
        store: PathBuf,
        folder: PathBuf,
        /// Persistent copy-on-write data for game saves and launcher updates.
        #[arg(long)]
        writes: Option<PathBuf>,
    },
    /// Switch an installed game to Maximum, keeping the original.
    Activate {
        store: PathBuf,
        folder: PathBuf,
        /// Update layer; defaults beside the store with a .writes suffix.
        #[arg(long)]
        writes: Option<PathBuf>,
    },
    /// Stop using a store and decompress to ordinary files.
    Rollback { folder: PathBuf },
    /// Delete the original after testing the game.
    Reclaim { folder: PathBuf },
    /// Fold in launcher updates by building a new store while the game stays in use.
    Compact { folder: PathBuf },
    /// Delete the previous version kept after folding in updates.
    Prune { folder: PathBuf },
    /// Delete pooled chunks that no game store still references.
    PoolPrune { pool: PathBuf },
    /// List the games that use Maximum.
    Installs,
}

/// Resolves a path against this process's working directory.
///
/// The coordinator keeps the working directory of whichever client started
/// it, so a relative path sent as typed would name a different folder there.
fn absolute(path: PathBuf) -> Result<PathBuf> {
    std::path::absolute(&path).with_context(|| format!("resolving {}", path.display()))
}

// `--maximum` replaces `--level`. clap rejects the two together.
fn options(level: i32, maximum: bool) -> super::Options {
    if maximum {
        super::Options::maximum()
    } else {
        super::Options {
            level,
            compare_level: None,
        }
    }
}

/// Runs one `pack` subcommand. Create, info, verify, restore, commit,
/// benchmark, mount and pool-prune work in this process. Activate, rollback,
/// reclaim, compact, prune and installs are requests to the coordinator,
/// which owns the mounts and the install records.
pub fn run(command: Command, json: bool, cancel: &AtomicBool) -> Result<()> {
    // Arms that produce a store summary fall through to the shared printing
    // at the end. Every other arm prints its own output and returns.
    let summary = match command {
        Command::Create {
            folder,
            store,
            pool,
            level,
            maximum,
        } => {
            let result = if let Some(pool) = pool {
                super::create_shared(&folder, &store, &pool, options(level, maximum), cancel)?
            } else {
                super::create(&folder, &store, options(level, maximum), cancel)?
            };
            eprintln!(
                "Store checked. The source folder stays: {}",
                folder.display()
            );
            result
        }
        Command::Info { store } => {
            eprintln!("Index checked. Run pack verify to check every chunk.");
            super::Reader::open(&store)?.summary().clone()
        }
        Command::Verify { store } => {
            let reader = super::Reader::open(&store)?;
            reader.verify(cancel)?;
            eprintln!("All chunks checked.");
            reader.summary().clone()
        }
        Command::Restore { store, folder } => {
            let result = super::restore(&store, &folder, cancel)?;
            eprintln!("Decompressed to {}.", folder.display());
            result
        }
        Command::Commit {
            store,
            writes,
            new_store,
            scratch_dir,
            level,
            maximum,
        } => {
            #[cfg(feature = "pack-mount")]
            {
                let result = super::overlay::commit(
                    &store,
                    &writes,
                    &new_store,
                    scratch_dir.as_deref(),
                    options(level, maximum),
                    cancel,
                )?;
                eprintln!(
                    "New store checked at {}. The old store and update layer are kept.",
                    new_store.display()
                );
                result
            }
            #[cfg(not(feature = "pack-mount"))]
            {
                let _paths = (store, writes, new_store, scratch_dir, level, maximum);
                anyhow::bail!(
                    "This build of Flummox cannot fold in updates. Use a build with the pack-mount feature."
                );
            }
        }
        Command::Benchmark {
            folder,
            scratch_dir,
            level,
            maximum,
            wof_lzx_helper,
        } => {
            let temp = match scratch_dir {
                Some(path) => tempfile::tempdir_in(path)?,
                None => tempfile::tempdir()?,
            };
            let store = temp.path().join("benchmark.flumpack");
            let start = Instant::now();
            let summary = super::create(&folder, &store, options(level, maximum), cancel)?;
            let build_and_verify_ns = start.elapsed().as_nanos();
            let reader = super::Reader::open(&store)?;
            // Read probes: 64 KiB at the head, middle and tail of the first
            // 256 files in index order. The reader is new, so its chunk cache
            // starts empty.
            let start = Instant::now();
            let mut random_read_bytes = 0u64;
            for entry in reader
                .entries()
                .iter()
                .filter(|e| {
                    matches!(
                        e.kind,
                        super::Kind::File { .. } | super::Kind::SlicedFile { .. }
                    )
                })
                .take(256)
            {
                anyhow::ensure!(
                    !cancel.load(std::sync::atomic::Ordering::Relaxed),
                    "Benchmark cancelled"
                );
                if let super::Kind::File { size, .. } | super::Kind::SlicedFile { size, .. } =
                    entry.kind
                {
                    for offset in [0, size / 2, size.saturating_sub(65536)] {
                        random_read_bytes += reader.read(&entry.path, offset, 65536)?.len() as u64;
                    }
                }
            }
            let random_read_ns = start.elapsed().as_nanos();
            // The store is one file, so its allocation rounds up once.
            let archive_allocated_bytes_4k = summary.archive_bytes.div_ceil(4096) * 4096;
            let wof_lzx = wof_lzx_helper
                .as_deref()
                .map(|helper| {
                    crate::benchmark::wof_lzx_proxy(
                        &folder,
                        helper,
                        summary.files,
                        summary.logical_bytes,
                        cancel,
                    )
                })
                .transpose()?;
            #[derive(serde::Serialize)]
            struct Report {
                #[serde(flatten)]
                summary: super::Summary,
                build_and_verify_ns: u128,
                random_read_bytes: u64,
                random_read_ns: u128,
                archive_allocated_bytes_4k: u64,
                wof_lzx: Option<crate::benchmark::WofLzxProxy>,
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&Report {
                        summary,
                        build_and_verify_ns,
                        random_read_bytes,
                        random_read_ns,
                        archive_allocated_bytes_4k,
                        wof_lzx
                    })?
                );
            } else {
                print(&summary);
                println!(
                    "Build and verify: {:.2} s. Read probes: {} bytes in {:.2} ms.",
                    build_and_verify_ns as f64 / 1e9,
                    random_read_bytes,
                    random_read_ns as f64 / 1e6
                );
                if let Some(wof) = &wof_lzx {
                    println!(
                        "WOF/LZX proxy: {} stream bytes, {} allocated at 4 KiB; {} of {} files compressed.",
                        wof.stored_stream_bytes,
                        wof.allocated_bytes_4k,
                        wof.compressed_files,
                        wof.files
                    );
                    println!(
                        "Flummox allocation at 4 KiB: {} bytes; difference: {} bytes.",
                        archive_allocated_bytes_4k,
                        i128::from(wof.allocated_bytes_4k) - i128::from(archive_allocated_bytes_4k)
                    );
                    println!("Same-corpus SHA-256: {}", wof.corpus_sha256);
                }
                println!("Full source corpus; store metadata included. Source files retained.");
            }
            return Ok(());
        }
        Command::Mount {
            store,
            folder,
            writes,
        } => {
            #[cfg(feature = "pack-mount")]
            {
                let session = super::mount::mount(&store, &folder, writes.as_deref())?;
                eprintln!(
                    "{} view at {}. Keep this process running; Ctrl-C unmounts it.",
                    if writes.is_some() {
                        "Writable copy-on-write"
                    } else {
                        "Read-only"
                    },
                    folder.display()
                );
                while !cancel.load(std::sync::atomic::Ordering::Relaxed) && !session.is_finished() {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                session.umount_and_join()?;
                return Ok(());
            }
            #[cfg(not(feature = "pack-mount"))]
            {
                let _paths = (store, folder, writes);
                anyhow::bail!(
                    "This build of Flummox cannot mount a store. Use a build with the pack-mount feature."
                );
            }
        }
        Command::Activate {
            store,
            folder,
            writes,
        } => {
            // The default layer is a sibling of the store: its path plus `.writes`.
            let (store, folder) = (absolute(store)?, absolute(folder)?);
            let writes = writes.map_or_else(
                || {
                    let mut name = store.as_os_str().to_os_string();
                    name.push(".writes");
                    Ok(PathBuf::from(name))
                },
                absolute,
            )?;
            let snapshot = crate::jobs::request(crate::jobs::Command::PackActivate {
                game_path: folder.clone(),
                store_path: store,
                writes_path: writes,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot.packs)?);
            } else if let Some(install) = snapshot
                .packs
                .iter()
                .find(|install| install.game_path == folder || install.game_path.ends_with(&folder))
            {
                println!("{}: {}", install.phase.label(), install.game_path.display());
                println!("{}", install.message);
                if let Some(backup) = &install.backup_path {
                    println!(
                        "Test the game, then run `flummox pack reclaim {}` to delete the original.",
                        install.game_path.display()
                    );
                    println!("Original kept at: {}", backup.display());
                }
            }
            return Ok(());
        }
        Command::Rollback { folder } => {
            let folder = absolute(folder)?;
            let snapshot = crate::jobs::request(crate::jobs::Command::PackRollback {
                game_path: folder.clone(),
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot.packs)?);
            } else {
                println!("Decompressed to ordinary files at {}.", folder.display());
            }
            return Ok(());
        }
        Command::Reclaim { folder } => {
            let folder = absolute(folder)?;
            let snapshot = crate::jobs::request(crate::jobs::Command::PackReclaim {
                game_path: folder.clone(),
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot.packs)?);
            } else {
                println!("Deleted the original for {}.", folder.display());
                println!(
                    "Decompressing will rebuild ordinary files from the store and its updates."
                );
            }
            return Ok(());
        }
        Command::Compact { folder } => {
            let folder = absolute(folder)?;
            let snapshot = crate::jobs::request(crate::jobs::Command::PackCompact {
                game_path: folder.clone(),
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot.packs)?);
            } else if let Some(install) = snapshot
                .packs
                .iter()
                .find(|install| install.game_path == folder || install.game_path.ends_with(&folder))
            {
                println!("Folded in updates for {}.", install.game_path.display());
                if let Some(previous) = &install.previous_store_path {
                    println!("Previous version kept at {}.", previous.display());
                    println!(
                        "After testing the game, run `flummox pack prune {}` to delete it.",
                        install.game_path.display()
                    );
                }
            }
            return Ok(());
        }
        Command::Prune { folder } => {
            let folder = absolute(folder)?;
            let snapshot = crate::jobs::request(crate::jobs::Command::PackPrune {
                game_path: folder.clone(),
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot.packs)?);
            } else {
                println!("Deleted the previous version for {}.", folder.display());
            }
            return Ok(());
        }
        Command::PoolPrune { pool } => {
            let summary = super::prune_shared_pool(&pool)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                println!(
                    "Removed {} unused pool objects and freed {}.",
                    summary.objects,
                    humansize::format_size(summary.reclaimed_bytes, humansize::DECIMAL)
                );
            }
            return Ok(());
        }
        Command::Installs => {
            let snapshot = crate::jobs::request(crate::jobs::Command::Snapshot)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot.packs)?);
            } else if snapshot.packs.is_empty() {
                println!("No games use Maximum.");
            } else {
                for install in &snapshot.packs {
                    println!(
                        "{}  {}  {}",
                        install.phase.label(),
                        install.game_path.display(),
                        install.message
                    );
                }
            }
            return Ok(());
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print(&summary);
    }
    Ok(())
}

// Human-readable form of a store summary. All figures are serialized sizes.
fn print(summary: &super::Summary) {
    println!(
        "{} files: {} source bytes, {} store bytes ({} metadata bytes).",
        summary.files, summary.logical_bytes, summary.archive_bytes, summary.metadata_bytes
    );
    println!(
        "{} unique chunks; {} duplicate source bytes shared.",
        summary.unique_chunks, summary.duplicate_bytes
    );
    if summary.shared_bytes > 0 {
        println!(
            "{} store bytes share allocation with another pooled game.",
            summary.shared_bytes
        );
        println!(
            "Marginal serialized bytes for this store: {}.",
            summary.archive_bytes.saturating_sub(summary.shared_bytes)
        );
    }
    println!(
        "Payload: {} raw bytes; {} compressed to {}; {} zero bytes need no payload.",
        summary.raw_bytes,
        summary.compressed_input_bytes,
        summary.compressed_bytes,
        summary.zero_bytes
    );
    if summary.logical_bytes > 0 {
        println!(
            "{:.2}% retained as serialized store bytes. This is not a drive free-space measurement.",
            summary.archive_bytes as f64 / summary.logical_bytes as f64 * 100.
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    #[test]
    fn coordinator_requests_carry_absolute_paths() -> TestResult {
        let relative = absolute(PathBuf::from("games/Portal")).ctx("relative path")?;
        check(
            relative.is_absolute(),
            "a relative path gains its directory",
        )?;
        check(relative.ends_with("games/Portal"), "and keeps its tail")?;
        check_eq(
            absolute(PathBuf::from("/mnt/games/Portal")).ctx("absolute path")?,
            PathBuf::from("/mnt/games/Portal"),
            "an absolute path is unchanged",
        )
    }
}
