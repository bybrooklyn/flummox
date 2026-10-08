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

/// The worker's stdout, shared by its threads.
struct Output(Mutex<std::io::Stdout>);
impl Output {
    /// Writes one event as a flushed JSON line. Write errors are discarded.
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
/// Presents the coordinator's pause flag to the backend as a busy check.
struct Pause(Arc<AtomicBool>);
impl BusyCheck for Pause {
    // Reading the flag costs nothing, so it is checked on every call.
    fn check_interval(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
    fn in_use_by(&self) -> Option<String> {
        self.0
            .load(Ordering::Relaxed)
            .then(|| "Paused. Work continues when resumed or gaming ends.".into())
    }
}

/// Makes pack sampling and corpus hashing wait while paused and stop on cancel.
struct AnalysisObserver<'a> {
    output: &'a Output,
    ctx: &'a JobCtx<'a>,
    gate: &'a Mutex<std::time::Instant>,
}
impl crate::pack::Observer for AnalysisObserver<'_> {
    fn checkpoint(&self) -> Result<()> {
        ensure!(
            self.ctx.wait_while_busy(self.gate),
            "Compatibility verification stopped"
        );
        Ok(())
    }
}

/// Runs one job through to its `Done` event. `run` reports an `Err` as `Failed`.
/// The saving to record after a pass: this pass's estimate plus the share of
/// the earlier figure that belongs to bytes this pass did not rewrite.
fn carried_saving(earlier: u64, install_bytes: u64, rewritten: u64, this_pass: u64) -> u64 {
    let untouched = install_bytes.saturating_sub(rewritten);
    let kept = if install_bytes == 0 {
        0
    } else {
        (u128::from(earlier) * u128::from(untouched) / u128::from(install_bytes)) as u64
    };
    kept.saturating_add(this_pass)
}

fn execute(work: Work, input: BufReader<std::io::Stdin>, output: &Output) -> Result<()> {
    ensure!(work.version == VERSION, "Worker version mismatch");
    let job = work.job;
    // Checks that need the whole filesystem come first. The sandbox below
    // narrows this process to the game folder.
    let _operation = super::operation_lock()?;
    if let Some(plan) = &job.space_plan {
        plan.recheck()?;
    }
    let path = validate_folder(&job.game.install_dir)?;
    let fs = crate::fsprobe::probe(&path)?;
    // Analysis also runs on a drive with no native backend. It then samples
    // with the btrfs backend's model.
    let kind = crate::fsprobe::tier_for(&fs)
        .backend()
        .or_else(|| {
            (job.operation == Operation::Analyze).then_some(crate::fsprobe::BackendKind::Btrfs)
        })
        .context("Compression is not supported on this drive yet.")?;
    let backend =
        backend::for_kind(kind).context("Compression is not supported on this drive yet.")?;
    if job.operation != Operation::Analyze {
        let plan = crate::storage::native_plan(&path, job.operation == Operation::Decompress)?;
        output.send(WorkerEvent::SpacePlan(plan.clone()));
        plan.recheck()?;
    }
    let mut db = Db::open(&Db::default_path().context("Cannot locate state database")?)?;
    for id in job.game.ids() {
        ensure!(
            !db.is_excluded(id)?,
            "This game is excluded. Restore it before processing."
        );
    }
    // Receipts stored under this job's policy name files an earlier attempt
    // finished. A compression receipt below the requested level floor is
    // ignored, so that file is processed again.
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
    // Receipts under any other policy describe a state this job is about to
    // overwrite. They go before the first rewrite, so an interrupted job
    // cannot leave them behind.
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
    // Opening the report store creates its folder, which the sandbox will
    // only let this process read.
    let reports = crate::compatibility::Store::local()?;
    let sandbox = crate::sandbox::restrict(&crate::sandbox::SandboxPlan::for_worker(
        &path,
        &Db::default_path().context("Cannot locate state database")?,
    ));
    if !sandbox.is_active() {
        output.event(Event::Warning(sandbox.describe()));
    }
    // The control socket is reachable by path whatever Landlock allows, and
    // this process never needs a socket.
    if let Err(error) = crate::sandbox::deny_sockets() {
        output.event(Event::Warning(format!(
            "this job can still open sockets: {error}"
        )));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let control_cancel = cancel.clone();
    let control_pause = paused.clone();
    // Control thread: applies Pause and Cancel lines from stdin. A read
    // error, which includes the coordinator closing the pipe, cancels the job.
    std::thread::spawn(move || {
        let mut input = input;
        loop {
            match client::read_message(&mut input) {
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
    // `full` is every file. `inv` leaves out files whose receipt still matches
    // their fingerprint.
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
    // Analysis only: when a saved report covers this game build, hash the
    // whole install and record the report whose corpus matches.
    if job.operation == Operation::Analyze {
        let reports = reports.load()?;
        let candidates: Vec<_> = reports
            .iter()
            .filter(|report| {
                report.qualifies(
                    &job.game,
                    &report.corpus.sha256,
                    crate::compatibility::Policy::default(),
                )
            })
            .collect();
        if !candidates.is_empty() {
            let observer = AnalysisObserver {
                output,
                ctx: &ctx,
                gate: &gate,
            };
            observer.output.event(Event::Progress {
                files_done: 0,
                bytes_done: 0,
                current: "Checking Maximum Space compatibility".into(),
            });
            let corpus = crate::compatibility::corpus(&path, &cancel, &observer)?;
            if let Some(report) = candidates.iter().find(|report| report.corpus == corpus) {
                summary.maximum_qualified = true;
                summary.maximum_qualification = Some(report.identity()?);
            }
        }
    }

    // Sampling pass for Analyze and Compress. It fills the estimate and
    // narrows `inv` to the files a native rewrite is expected to shrink, plus
    // eligible files the sample budget did not reach.
    if job.operation != Operation::Decompress {
        summary.maximum_after = Some(0);
        // A storage job reports totals once, from the backend, when rewriting
        // starts. Totals here would fill the progress bar during sampling and
        // empty it again for the rewrite.
        let counted = job.operation == Operation::Analyze;
        if counted {
            output.event(Event::Started {
                files: inv.files.len() as u64,
                bytes: inv.total_bytes(),
            });
        }
        let model = backend.model(&job.options);
        let opts = EstimateOpts::new(job.options.btrfs_level(), &fs);
        let probe = backend.disk_probe();
        const ANALYSIS_BUDGET: u64 = 32 * 1024 * 1024;
        const FILE_SAMPLE_CAP: u64 = 1024 * 1024;
        let observer = AnalysisObserver {
            output,
            ctx: &ctx,
            gate: &gate,
        };
        output.event(Event::Progress {
            files_done: 0,
            bytes_done: 0,
            current: "Sampling small-file grouping".into(),
        });
        summary.small_files =
            crate::pack::sample_small_files(&path, &full, 4 * 1024 * 1024, &cancel, &observer)?;
        let mut budget = ANALYSIS_BUDGET.saturating_sub(summary.small_files.bytes);
        let mut candidates = Vec::new();
        let mut inspected_bytes = 0u64;
        // Sizes of the files sampled and of those the budget did not reach,
        // for scaling the totals once sampling ends.
        let mut sampled_size = 0u64;
        let mut unsampled_size = 0u64;
        for (index, entry) in inv.files.iter().enumerate() {
            if !ctx.wait_while_busy(&gate) {
                break;
            }
            if !entry.action.is_compress() {
                summary.skipped_files += 1;
                continue;
            }
            if budget == 0 {
                unsampled_size = unsampled_size.saturating_add(entry.size);
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
                    sampled_size = sampled_size.saturating_add(entry.size);
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
            output.event(if counted {
                Event::Progress {
                    files_done: index as u64 + 1,
                    bytes_done: inspected_bytes,
                    current: entry.rel.display().to_string(),
                }
            } else {
                Event::Progress {
                    files_done: 0,
                    bytes_done: 0,
                    current: format!("Analyzing {}", entry.rel.display()),
                }
            });
        }
        summary.rewrite_files = candidates.len() as u64;
        inv.files = candidates;
        // The totals cover the sampled files, which are the largest. Reporting
        // them alone showed a fraction of what a pass frees on a game with
        // thousands of files, so the rest is assumed to behave the same way.
        // A cancelled analysis stays as measured.
        if unsampled_size > 0 && sampled_size > 0 && !cancel.load(Ordering::Relaxed) {
            let scale = unsampled_size as f64 / sampled_size as f64;
            let grow = |value: u64| value.saturating_add((value as f64 * scale) as u64);
            summary.bytes = grow(summary.bytes);
            summary.disk_now = grow(summary.disk_now);
            summary.disk_after = grow(summary.disk_after);
            summary.maximum_after = summary.maximum_after.map(grow);
        }
        output.send(WorkerEvent::Estimate(summary));
    }
    // Analysis ends here, and so does a job cancelled during sampling.
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
    // History: a compress pass records the files it finished together with
    // earlier receipts that still match. A decompress with no errors forgets
    // the game.
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
        // The number describes analysis, never a measured receipt. A pass
        // after an update analyses only the files that changed, so the share
        // of the earlier figure that belongs to untouched files is kept.
        // Without it the recorded saving shrank to the latest pass alone.
        let earlier = db.game(&job.game.id)?.map_or(0, |previous| {
            u64::try_from(previous.est_saving).unwrap_or(0)
        });
        record.est_saving = i64::try_from(carried_saving(
            earlier,
            record.install_bytes,
            outcome.bytes,
            summary.saving(),
        ))
        .unwrap_or(i64::MAX);
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

/// Worker entry point. Reads one [`Work`] line from stdin and runs it. A
/// failure is sent as `WorkerEvent::Failed`, so this always returns `Ok`.
pub(super) fn run() -> Result<()> {
    let output = Output(Mutex::new(std::io::stdout()));
    let mut input = BufReader::new(std::io::stdin());
    let result = client::read_message(&mut input).and_then(|work| execute(work, input, &output));
    if let Err(error) = result {
        output.send(WorkerEvent::Failed(format!("{error:#}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::carried_saving;
    use crate::testutil::{TestResult, check_eq};

    #[test]
    fn a_later_pass_keeps_the_saving_on_files_it_did_not_touch() -> TestResult {
        check_eq(carried_saving(0, 1000, 1000, 300), 300, "a first pass")?;
        check_eq(
            carried_saving(300, 1000, 0, 0),
            300,
            "a pass with nothing to do changes nothing",
        )?;
        check_eq(
            carried_saving(300, 1000, 100, 40),
            310,
            "an update to a tenth of the game replaces a tenth of the figure",
        )?;
        check_eq(carried_saving(300, 0, 0, 0), 0, "an empty game")
    }
}
