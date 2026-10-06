//! Cancellable discovery messages consumed without blocking the coordinator.
use super::{Env, Scan};
use crate::model::Game;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

pub enum Event {
    Source(&'static str),
    Batch(Vec<Game>),
    Finished(Option<Scan>),
}
pub struct Worker {
    receiver: mpsc::Receiver<Event>,
    cancel: Arc<AtomicBool>,
}
impl Worker {
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
        Self { receiver, cancel }
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
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
