//! One bounded job per process. Filesystem rights are dropped before work.

use super::*;
use crate::{
    backend::{self, BusyCheck, Event, EventSink, JobCtx},
    db::{Db, GameRecord},
    estimate::{self, Estimate, EstimateOpts, PackModel, PreviewEstimate},
    inventory::{self, Inventory},
    safeio::Anchor,
};
use anyhow::{Context, Result, ensure};
use std::io::{BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

struct Output(Mutex<std::io::Stdout>);
impl Output {
    fn send(&self, event: WorkerEvent) {
        if let Ok(mut output) = self.0.lock() {
            let _sent = serde_json::to_writer(&mut *output, &event)
                .map_err(std::io::Error::other)
                .and_then(|_| output.write_all(b"\n"))
                .and_then(|_| output.flush());
        }
    }
}
impl EventSink for Output {
    fn event(&self, event: Event) {
        self.send(WorkerEvent::Progress(event));
    }
}
struct Pause(Arc<AtomicBool>);
impl BusyCheck for Pause {
    fn check_interval(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
    fn in_use_by(&self) -> Option<String> {
        self.0
            .load(Ordering::Relaxed)
            .then(|| "Paused. Work continues when resumed or gaming ends.".into())
    }
}

fn execute(work: Work, input: BufReader<std::io::Stdin>, output: &Output) -> Result<()> {
    ensure!(work.version == VERSION, "Worker version mismatch");
    let job = work.job;
    let _operation = super::operation_lock()?;
    let path = validate_folder(&job.game.install_dir)?;
    let fs = crate::fsprobe::probe(&path)?;
    let kind = crate::fsprobe::tier_for(&fs)
        .backend()
        .context("Compression is not supported on this drive yet.")?;
    let backend =
        backend::for_kind(kind).context("Compression is not supported on this drive yet.")?;
    let mut db = Db::open(&Db::default_path().context("Cannot locate state database")?)?;
    for id in job.game.ids() {
        ensure!(
            !db.is_excluded(id)?,
            "This game is excluded. Restore it before processing."
        );
    }
    let mut completed = std::collections::HashMap::new();
    let receipts = rusqlite::Connection::open_with_flags(
        super::state_dir()?.join("queue.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut stmt = receipts.prepare("SELECT entry FROM receipts WHERE game=?1 AND policy=?2")?;
    let policy = service::policy(&job)?;
    for row in stmt.query_map(
        rusqlite::params![path.as_os_str().as_bytes(), policy],
        |r| r.get::<_, String>(0),
    )? {
        let (entry, level): (inventory::FileEntry, i32) = serde_json::from_str(&row?)?;
        if job.operation == Operation::Decompress || level >= job.options.level_plan().floor() {
            completed.insert(entry.rel.clone(), (entry, level));
        }
    }
    drop(stmt);
    drop(receipts);
    if job.operation != Operation::Analyze {
        service::invalidate_receipts(
            &super::state_dir()?.join("queue.sqlite"),
            &path,
            Some(&policy),
        )?;
        if job.operation == Operation::Decompress {
            db.invalidate_compression(&path)?;
        }
    }
    let sandbox = crate::sandbox::restrict(&crate::sandbox::SandboxPlan::for_job(
        &path,
        Db::default_path()
            .as_deref()
            .and_then(std::path::Path::parent),
    ));
    if !sandbox.is_active() {
        output.event(Event::Warning(sandbox.describe()));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let control_cancel = cancel.clone();
    let control_pause = paused.clone();
    std::thread::spawn(move || {
        let mut input = input;
        loop {
            match service::read_message(&mut input) {
                Ok(Control::Cancel) => {
                    control_cancel.store(true, Ordering::Relaxed);
                }
                Ok(Control::Pause(value)) => control_pause.store(value, Ordering::Relaxed),
                Err(_) => {
                    control_cancel.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });
    let pause = Pause(paused);
    let ctx = JobCtx {
        events: output,
        cancel: &cancel,
        busy: Some(&pause),
    };
    let gate = Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(5));
    let full = match inventory::walk_cancellable(&path, &backend.walk_opts(), Some(&cancel)) {
        Ok(inv) => inv,
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
            output.send(WorkerEvent::Done {
                cancelled: true,
                errors: vec![],
                drive_change: None,
            });
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let mut inv = Inventory {
        files: full
            .files
            .iter()
            .filter(|entry| {
                completed
                    .get(&entry.rel)
                    .is_none_or(|(old, _)| old.changed_since(entry))
            })
            .cloned()
            .collect(),
        warnings: full.warnings.clone(),
    };
    // Spend the bounded preview on the files that dominate install size.
    inv.files
        .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.rel.cmp(&b.rel)));
    let anchor = Anchor::open(&path)?;
    ensure!(
        anchor.fully_resolved(),
        "This kernel cannot safely resolve game paths. Update Linux before using GUI jobs."
    );
    let mut summary = Estimate {
        install_bytes: full.total_bytes(),
        rewrite_files: inv.to_compress().count() as u64,
        unsampled_files: inv.to_compress().count() as u64,
        already_compressed_mount: fs.mount_compression().is_some(),
        ..Estimate::default()
    };
    let mut failures: Vec<_> = inv.warnings.iter().take(20).cloned().collect();
    if job.operation != Operation::Decompress {
        summary.maximum_after = Some(0);
        output.event(Event::Started {
            files: inv.files.len() as u64,
            bytes: inv.total_bytes(),
        });
        let model = backend.model(&job.options);
        let opts = EstimateOpts::new(job.options.btrfs_level(), &fs);
        let probe = backend.disk_probe();
        const ANALYSIS_BUDGET: u64 = 32 * 1024 * 1024;
        const FILE_SAMPLE_CAP: u64 = 1024 * 1024;
        let mut budget = ANALYSIS_BUDGET;
        let mut candidates = Vec::new();
        let mut inspected_bytes = 0u64;
        for (index, entry) in inv.files.iter().enumerate() {
            if !ctx.wait_while_busy(&gate) {
                break;
            }
            if !entry.action.is_compress() {
                summary.skipped_files += 1;
                continue;
            }
            if budget == 0 {
                candidates.push(entry.clone());
                continue;
            }
            let sampled = (|| -> Result<_> {
                let file = anchor.open_file(&entry.rel)?;
                ensure!(entry.matches_file(&file)?, "File changed during analysis");
                let measured = probe
                    .measure(&path.join(&entry.rel))
                    .and_then(|(c, n)| (n > 0).then_some(c as f64 / n as f64));
                let sample_cap = budget.min(FILE_SAMPLE_CAP);
                let (native, maximum) = estimate::estimate_open_file_pair(
                    &file,
                    entry.size,
                    PreviewEstimate {
                        native: model.as_ref(),
                        native_opts: &opts,
                        measured,
                        maximum: &PackModel { level: 19 },
                        maximum_opts: &EstimateOpts {
                            level: 19,
                            mount_level: None,
                        },
                        byte_cap: sample_cap,
                    },
                )?;
                ensure!(entry.matches_file(&file)?, "File changed during analysis");
                Ok((native, maximum))
            })();
            match sampled {
                Ok((native, maximum)) => {
                    // Both projections score the same bytes, so charge the
                    // analysis budget once.
                    let sampled = native.sampled;
                    budget = budget.saturating_sub(sampled);
                    summary.sampled = summary.sampled.saturating_add(sampled);
                    summary.inspected_files += 1;
                    summary.unsampled_files = summary.unsampled_files.saturating_sub(1);
                    summary.format_evidence.observe(native.inspection);
                    // Small absolute wins still matter across many small files.
                    if native.worthwhile() || maximum.worthwhile() {
                        summary.disk_now = summary.disk_now.saturating_add(native.disk_now);
                        summary.disk_after =
                            summary.disk_after.saturating_add(if native.worthwhile() {
                                native.disk_after
                            } else {
                                native.disk_now
                            });
                        summary.maximum_after = summary.maximum_after.map(|after| {
                            after.saturating_add(if maximum.worthwhile() {
                                maximum.disk_after
                            } else {
                                native.disk_now
                            })
                        });
                        summary.bytes += entry.size;
                        summary.files += 1;
                        if native.worthwhile() {
                            candidates.push(entry.clone());
                        }
                    } else {
                        summary.skipped_files += 1;
                    }
                }
                Err(e) => {
                    if failures.len() < 20 {
                        failures.push(format!("{}: {e}", entry.rel.display()));
                    }
                }
            }
            output.send(WorkerEvent::Estimate(summary));
            inspected_bytes = inspected_bytes.saturating_add(entry.size);
            output.event(Event::Progress {
                files_done: index as u64 + 1,
                bytes_done: inspected_bytes,
                current: entry.rel.display().to_string(),
            });
        }
        summary.rewrite_files = candidates.len() as u64;
        inv.files = candidates;
        output.send(WorkerEvent::Estimate(summary));
    }
    if job.operation == Operation::Analyze || cancel.load(Ordering::Relaxed) {
        output.send(WorkerEvent::Done {
            cancelled: cancel.load(Ordering::Relaxed),
            errors: failures,
            drive_change: None,
        });
        return Ok(());
    }
    if !ctx.wait_while_busy(&gate) {
        output.send(WorkerEvent::Done {
            cancelled: true,
            errors: vec![],
            drive_change: None,
        });
        return Ok(());
    }
    let outcome = if job.operation == Operation::Decompress {
        backend.decompress(&path, &inv, &ctx)?
    } else {
        backend.compress(&path, &inv, &job.options, &ctx)?
    };
    failures.extend(outcome.errors.clone());
    if job.operation == Operation::Compress
        && outcome
            .effective_level
            .is_some_and(|level| level < job.options.level_plan().floor())
    {
        failures.push("The kernel used its default compression level. A newer kernel is needed to apply the requested preset.".into());
    }
    if job.operation == Operation::Compress {
        let mut record = GameRecord::new(
            job.game.id.clone(),
            job.game.title.clone(),
            path,
            kind,
            &job.options,
        );
        record.build = job.game.build.clone();
        record.install_bytes = full.total_bytes();
        record.level = outcome.effective_level.unwrap_or(0);
        // The number describes this pass's analysis, never a measured receipt.
        record.est_saving = i64::try_from(summary.saving()).unwrap_or(i64::MAX);
        let mut confirmed = outcome.completed.clone();
        confirmed.extend(full.files.iter().filter_map(|entry| {
            completed
                .get(&entry.rel)
                .filter(|(old, _)| !old.changed_since(entry))
                .map(|(_, level)| (entry.clone(), *level))
        }));
        db.record_outcome(&record, &full, &confirmed)?;
    } else if !outcome.cancelled && outcome.errors.is_empty() {
        db.forget(&job.game.id)?;
    }
    output.send(WorkerEvent::Done {
        cancelled: outcome.cancelled,
        errors: failures,
        drive_change: outcome.freed(),
    });
    Ok(())
}

pub(super) fn run() -> Result<()> {
    let output = Output(Mutex::new(std::io::stdout()));
    let mut input = BufReader::new(std::io::stdin());
    let result = service::read_message(&mut input).and_then(|work| execute(work, input, &output));
    if let Err(error) = result {
        output.send(WorkerEvent::Failed(format!("{error:#}")));
    }
    Ok(())
}
