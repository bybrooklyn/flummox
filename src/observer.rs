//! Progress and stop points for long operations, on every platform.

/// Checkpoints and measured progress for long storage preparation phases.
pub trait Observer {
    /// Called between units of work. An error stops the operation there.
    fn checkpoint(&self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Announces a new stage with its expected totals.
    fn started(&self, _files: u64, _bytes: u64, _stage: &str) {}
    /// Reports cumulative counts within the current stage.
    fn progress(&self, _files: u64, _bytes: u64, _stage: &str) {}
}

/// The observer for callers that want no progress and no extra stop points.
pub struct NoObserver;
impl Observer for NoObserver {}
