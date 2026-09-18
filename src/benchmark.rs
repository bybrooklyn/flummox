//! Read-only codec experiments on identical, bounded samples.
//! Frame results exclude the pack store's index and allocation overhead.

use crate::{
    estimate::{BtrfsModel, UnitModel},
    inventory,
    safeio::Anchor,
};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    os::unix::ffi::OsStrExt,
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

const FRAME: usize = 4 * 1024 * 1024;
const WOF_CHUNK: usize = 32 * 1024;
const ALLOCATION_UNIT: u64 = 4096;

/// Full-corpus estimate produced by a verified 32 KiB LZX chunk helper.
#[derive(Debug, serde::Serialize)]
pub struct WofLzxProxy {
    pub method: &'static str,
    pub corpus_sha256: String,
    pub files: u64,
    pub compressed_files: u64,
    pub input_bytes: u64,
    pub stored_stream_bytes: u64,
    pub allocated_bytes_4k: u64,
    pub compressed_chunks: u64,
    pub raw_chunks: u64,
    pub benchmark_ns: u128,
}

fn write_helper(input: &mut impl Write, bytes: &[u8]) -> Result<()> {
    input.write_all(&u32::try_from(bytes.len())?.to_le_bytes())?;
    input.write_all(bytes)?;
    input.flush()?;
    Ok(())
}

fn read_helper(output: &mut impl Read) -> Result<u32> {
    let mut bytes = [0u8; 4];
    output.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn allocated(bytes: u64) -> u64 {
    bytes.div_ceil(ALLOCATION_UNIT) * ALLOCATION_UNIT
}

fn path_hash(hasher: &mut Sha256, path: &Path) -> Result<()> {
    let bytes = path.as_os_str().as_bytes();
    hasher.update(u64::try_from(bytes.len())?.to_le_bytes());
    hasher.update(bytes);
    Ok(())
}

/// Models a Windows WOF/LZX stream using an external 32 KiB codec helper.
///
/// Each compressed chunk is decoded by the helper before its size is accepted.
/// Source counts from the Flummox run must match this second corpus walk.
pub fn wof_lzx_proxy(
    root: &Path,
    helper: &Path,
    expected_files: u64,
    expected_bytes: u64,
    cancel: &AtomicBool,
) -> Result<WofLzxProxy> {
    let root = crate::jobs::validate_folder(root)?;
    let anchor = Anchor::open(&root)?;
    ensure!(
        anchor.fully_resolved(),
        "Safe path resolution is unavailable on this kernel"
    );
    let mut inv =
        inventory::walk_cancellable(&root, &inventory::WalkOpts { min_size: 0 }, Some(cancel))?;
    ensure!(
        inv.warnings.is_empty(),
        "The LZX corpus walk could not read every source path"
    );
    inv.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    ensure!(
        u64::try_from(inv.files.len())? == expected_files && inv.total_bytes() == expected_bytes,
        "The Flummox and LZX corpus counts differ"
    );

    let mut child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "LZX helper stdin is missing",
        )
    })?;
    let mut output = child.stdout.take().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "LZX helper stdout is missing",
        )
    })?;
    let started = Instant::now();
    let measured = (|| -> Result<WofLzxProxy> {
        let mut report = WofLzxProxy {
            method: "wimlib LZX:50 WOF proxy",
            corpus_sha256: String::new(),
            files: expected_files,
            compressed_files: 0,
            input_bytes: expected_bytes,
            stored_stream_bytes: 0,
            allocated_bytes_4k: 0,
            compressed_chunks: 0,
            raw_chunks: 0,
            benchmark_ns: 0,
        };
        let mut corpus = Sha256::new();
        let mut buffer = vec![0u8; WOF_CHUNK];
        for entry in &inv.files {
            ensure!(!cancel.load(Ordering::Relaxed), "LZX benchmark cancelled");
            path_hash(&mut corpus, &entry.rel)?;
            corpus.update(entry.size.to_le_bytes());
            let mut file = anchor.open_file(&entry.rel)?;
            ensure!(
                entry.matches_file(&file)?,
                "{} changed before the LZX benchmark",
                entry.rel.display()
            );
            let mut encoded = 0u64;
            let mut chunks = 0u64;
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                let bytes = buffer.get(..count).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid LZX chunk")
                })?;
                corpus.update(bytes);
                write_helper(&mut input, bytes)?;
                let stored = u64::from(read_helper(&mut output)?);
                ensure!(
                    stored <= count as u64,
                    "LZX helper returned an invalid size"
                );
                encoded = encoded.checked_add(stored).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "LZX size overflow")
                })?;
                if stored < count as u64 {
                    report.compressed_chunks = report.compressed_chunks.saturating_add(1);
                } else {
                    report.raw_chunks = report.raw_chunks.saturating_add(1);
                }
                chunks = chunks.saturating_add(1);
            }
            ensure!(
                entry.matches_file(&file)?,
                "{} changed during the LZX benchmark",
                entry.rel.display()
            );
            let table_width = if entry.size > u64::from(u32::MAX) {
                8
            } else {
                4
            };
            let table = chunks.saturating_sub(1).saturating_mul(table_width);
            let stream = encoded.saturating_add(table);
            let raw_allocation = allocated(entry.size);
            let wof_allocation = allocated(stream);
            if wof_allocation < raw_allocation {
                report.compressed_files = report.compressed_files.saturating_add(1);
                report.stored_stream_bytes = report.stored_stream_bytes.saturating_add(stream);
                report.allocated_bytes_4k =
                    report.allocated_bytes_4k.saturating_add(wof_allocation);
            } else {
                report.stored_stream_bytes = report.stored_stream_bytes.saturating_add(entry.size);
                report.allocated_bytes_4k =
                    report.allocated_bytes_4k.saturating_add(raw_allocation);
            }
        }
        report.corpus_sha256 = format!("{:x}", corpus.finalize());
        report.benchmark_ns = started.elapsed().as_nanos();
        Ok(report)
    })();
    let _closed = input.write_all(&0u32.to_le_bytes());
    drop(input);
    drop(output);
    let status = child.wait()?;
    let report = measured?;
    ensure!(status.success(), "LZX helper failed with {status}");
    Ok(report)
}

/// Cost and timing over the same source bytes, with every frame decoded back.
#[derive(Debug, serde::Serialize)]
pub struct Candidate {
    pub name: String,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub compression_ns: u128,
    pub decompression_ns: u128,
}

/// A sample comparison, never a measurement of disk space recovered.
#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub install_bytes: u64,
    pub sampled_files: u64,
    pub eligible_files: u64,
    pub sampled_bytes: u64,
    pub cancelled: bool,
    pub warnings: Vec<String>,
    pub candidates: Vec<Candidate>,
}

fn measure(input: &[u8], level: i32, native: bool, row: &mut Candidate) -> Result<()> {
    let block = if native {
        BtrfsModel::BLOCK as usize
    } else {
        FRAME
    };
    for chunk in input.chunks(block) {
        let started = Instant::now();
        let compressed = zstd::bulk::compress(chunk, level)?;
        row.compression_ns += started.elapsed().as_nanos();
        let started = Instant::now();
        let decoded = zstd::bulk::decompress(&compressed, chunk.len())?;
        row.decompression_ns += started.elapsed().as_nanos();
        ensure!(decoded == chunk, "Codec round-trip failed");
        row.input_bytes += chunk.len() as u64;
        row.output_bytes += if native {
            BtrfsModel { level }.disk_cost(chunk.len() as u32, compressed.len() as u32)
        } else {
            compressed.len().min(chunk.len()) as u64
        };
    }
    Ok(())
}

/// Samples up to `budget_mib` (4..=256 MiB), spreading reads through each file.
/// Sources are anchored and checked for modification. No source bytes are written.
pub fn run(root: &Path, budget_mib: u64, cancel: &AtomicBool) -> Result<Report> {
    ensure!(
        (4..=256).contains(&budget_mib),
        "Choose a sampling budget from 4 to 256 MiB"
    );
    let root = crate::jobs::validate_folder(root)?;
    let anchor = Anchor::open(&root)?;
    ensure!(
        anchor.fully_resolved(),
        "Safe path resolution is unavailable on this kernel"
    );
    let inv = inventory::walk_cancellable(&root, &inventory::WalkOpts::native(), Some(cancel))?;
    let profiles = [
        (3, true),
        (9, true),
        (15, true),
        (9, false),
        (15, false),
        (19, false),
    ];
    let candidates = profiles
        .iter()
        .map(|(level, native)| Candidate {
            name: format!(
                "{} zstd:{level}",
                if *native {
                    "btrfs 128 KiB model"
                } else {
                    "experimental 4 MiB frames"
                }
            ),
            input_bytes: 0,
            output_bytes: 0,
            compression_ns: 0,
            decompression_ns: 0,
        })
        .collect();
    let mut report = Report {
        install_bytes: inv.total_bytes(),
        sampled_files: 0,
        eligible_files: inv.to_compress().count() as u64,
        sampled_bytes: 0,
        cancelled: false,
        warnings: inv.warnings.clone(),
        candidates,
    };
    let mut remaining = budget_mib * 1024 * 1024;
    let eligible = inv.to_compress().count().max(1) as u64;
    // Large-window comparisons need a complete frame when the file allows it.
    // Sampling 128 KiB from every file would hide all cross-block repetition.
    let per_file = (remaining / eligible).clamp(FRAME as u64, FRAME as u64 * 4);
    let mut entries: Vec<_> = inv.to_compress().collect();
    entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.rel.cmp(&b.rel)));
    for entry in entries {
        if remaining == 0 || cancel.load(Ordering::Relaxed) {
            break;
        }
        let file = anchor.open_file(&entry.rel);
        let mut file = match file {
            Ok(file) => file,
            Err(error) => {
                report
                    .warnings
                    .push(format!("{}: {error}", entry.rel.display()));
                continue;
            }
        };
        ensure!(
            entry.matches_file(&file)?,
            "{} changed before sampling",
            entry.rel.display()
        );
        let total = entry.size.min(per_file).min(remaining);
        let samples = total.div_ceil(FRAME as u64);
        let mut left = total;
        for index in 0..samples {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let length = left.min(FRAME as u64) as usize;
            let offset = if samples <= 1 {
                (entry.size.saturating_sub(length as u64)) / 2
            } else {
                entry.size.saturating_sub(length as u64) / (samples - 1) * index
            };
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = vec![0; length];
            file.read_exact(&mut bytes)?;
            for ((level, native), row) in profiles.iter().zip(&mut report.candidates) {
                measure(&bytes, *level, *native, row)?;
            }
            remaining -= length as u64;
            left -= length as u64;
            report.sampled_bytes += length as u64;
        }
        ensure!(
            entry.matches_file(&file)?,
            "{} changed during sampling; discard this comparison",
            entry.rel.display()
        );
        report.sampled_files += 1;
    }
    report.cancelled = cancel.load(Ordering::Relaxed);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    fn candidate() -> Candidate {
        Candidate {
            name: String::new(),
            input_bytes: 0,
            output_bytes: 0,
            compression_ns: 0,
            decompression_ns: 0,
        }
    }

    #[test]
    fn controls_distinguish_entropy_from_repetition_outside_native_blocks() -> TestResult {
        let mut seed = 42u64;
        let noise: Vec<u8> = (0..256 * 1024)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        let mut raw = candidate();
        measure(&noise, 9, true, &mut raw).ctx("noise control")?;
        check_eq(
            raw.output_bytes,
            raw.input_bytes,
            "random data gains nothing in native blocks",
        )?;
        let mut zeros = candidate();
        measure(&vec![0; 256 * 1024], 9, true, &mut zeros).ctx("zero control")?;
        check(
            zeros.output_bytes < zeros.input_bytes / 10,
            "zeros must shrink",
        )?;
        let repeated = noise.repeat(4);
        let mut native = candidate();
        let mut frames = candidate();
        measure(&repeated, 9, true, &mut native).ctx("native blocks")?;
        measure(&repeated, 9, false, &mut frames).ctx("larger frame")?;
        check_eq(
            native.input_bytes,
            frames.input_bytes,
            "identical source bytes",
        )?;
        check(
            frames.output_bytes < native.output_bytes / 2,
            "larger frames can reuse distant data",
        )
    }

    #[test]
    fn corpus_fingerprint_has_a_portable_record_layout() -> TestResult {
        let mut hash = Sha256::new();
        path_hash(&mut hash, Path::new("dir/file.bin")).ctx("path record")?;
        hash.update(3u64.to_le_bytes());
        hash.update(b"abc");
        check_eq(
            format!("{:x}", hash.finalize()),
            "02a59e4570844e8ab5f39860a50329a604937c943075d0bf736c7665df37b4b3".to_owned(),
            "PowerShell and Rust use the same path, size, and payload record",
        )
    }
}
