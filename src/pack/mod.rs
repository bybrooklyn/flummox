//! Versioned, verified game stores with bounded random reads.
//! Creation and restoration publish new destinations without replacing sources.

pub(crate) mod cli;
mod create;
mod format;
mod install;
#[cfg(feature = "pack-mount")]
pub mod mount;
#[cfg(feature = "pack-mount")]
mod overlay;
#[cfg(feature = "pack-mount")]
mod recovery;
mod restore;

pub use create::{
    Options, PoolPruneSummary, SmallFileSample, create, create_observed, create_shared,
    create_shared_observed, prune_shared_pool, sample_small_files,
};
pub use format::{CHUNK_BYTES, Entry, Kind, Reader, Summary};
pub use install::{Install, InstallPhase};
#[cfg(feature = "pack-mount")]
pub use recovery::verify_restored;
pub use restore::restore;

#[cfg(feature = "pack-mount")]
pub(crate) use install::{
    MountedInstall, activate, begin_prune, compaction_prefix, finish_prune, finish_reclaim,
    prepare, prepare_observed, reclaim, recover, rollback,
};

/// Builds a new store at `output` from `store` with the stopped update layer
/// at `writes` applied. A mounted layer is refused. The old store and the
/// layer are left as they were.
#[cfg(feature = "pack-mount")]
pub fn commit_updates(
    store: &std::path::Path,
    writes: &std::path::Path,
    output: &std::path::Path,
    scratch: Option<&std::path::Path>,
    options: Options,
    cancel: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<Summary> {
    overlay::commit(store, writes, output, scratch, options, cancel)
}

#[cfg(test)]
mod tests;

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
