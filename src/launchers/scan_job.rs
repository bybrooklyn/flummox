//! Cancellable discovery messages consumed without blocking the coordinator.
use super::{Env, Scan};
use crate::model::Game;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

/// Messages from a running discovery scan, in the order they happen.
pub enum Event {
    /// The source about to be read.
    Source(&'static str),
    /// Every game found so far, earlier batches included, before merging.
    Batch(Vec<Game>),
    /// The merged scan. `None` when it was cancelled or its thread died.
    Finished(Option<Scan>),
}
/// A discovery scan on its own thread. Dropping it cancels the scan.
pub struct Worker {
    receiver: mpsc::Receiver<Event>,
    cancel: Arc<AtomicBool>,
    started: std::time::Instant,
}

/// How long a scan may run before the coordinator should abandon it.
pub const SCAN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
impl Worker {
    /// Starts scanning `env` in the background.
    pub fn start(env: Env) -> Self {
        let (send, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let token = cancel.clone();
        std::thread::spawn(move || {
            let scan = super::scan_with(&env, &token, |event| {
                let _sent = send.send(event);
            });
            let _sent = send.send(Event::Finished(scan));
        });
        Self {
            receiver,
            cancel,
            started: std::time::Instant::now(),
        }
    }
    /// Asks the scan to stop. It checks between sources, so the one being
    /// read finishes first.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    /// Whether the scan has run longer than `limit`. A scan stuck on a dead
    /// network mount never checks its cancel flag, so the caller drops the
    /// worker, raises a warning and treats game states as unknown.
    pub fn overdue(&self, limit: std::time::Duration) -> bool {
        self.started.elapsed() > limit
    }
    /// Whether `cancel` was called.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    /// Takes the events that have arrived, without blocking. `Finished` is
    /// always last. A thread that ended without sending it yields
    /// `Finished(None)`.
    pub fn events(&self) -> Vec<Event> {
        let mut events = vec![];
        loop {
            match self.receiver.try_recv() {
                Ok(event) => {
                    let finished = matches!(event, Event::Finished(_));
                    events.push(event);
                    if finished {
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    events.push(Event::Finished(None));
                    break;
                }
            }
        }
        events
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check};
    #[test]
    fn a_worker_reports_when_it_has_run_too_long() -> TestResult {
        let fixture = tempfile::tempdir().ctx("discovery fixture")?;
        let worker = Worker::start(Env::from_home(fixture.path()));
        check(
            !worker.overdue(std::time::Duration::from_secs(3600)),
            "a new worker is not overdue",
        )?;
        std::thread::sleep(std::time::Duration::from_millis(5));
        check(
            worker.overdue(std::time::Duration::ZERO),
            "any worker is overdue against a zero limit",
        )
    }
    #[test]
    fn cancellation_stops_later_providers_and_worker_finishes() -> TestResult {
        let fixture = tempfile::tempdir().ctx("discovery fixture")?;
        let env = Env::from_home(fixture.path());
        let token = AtomicBool::new(false);
        let mut sources = vec![];
        let result = super::super::scan_with(&env, &token, |event| match event {
            Event::Source(source) => sources.push(source),
            Event::Batch(_) => token.store(true, Ordering::Relaxed),
            Event::Finished(_) => {}
        });
        check(result.is_none(), "cancelled scan is not published")?;
        check(sources == vec!["Steam"], "later providers are skipped")?;
        let worker = Worker::start(env);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if worker
                .events()
                .into_iter()
                .any(|event| matches!(event, Event::Finished(Some(_))))
            {
                return Ok(());
            }
            check(
                std::time::Instant::now() < deadline,
                "fixture worker completes",
            )?;
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}
