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
mod restore;

pub use create::{Options, PoolPruneSummary, create, create_shared, prune_shared_pool};
pub use format::{CHUNK_BYTES, Entry, Kind, Reader, Summary};
pub use install::{Install, InstallPhase};
pub use restore::restore;

#[cfg(feature = "pack-mount")]
pub(crate) use install::{
    MountedInstall, activate, begin_prune, finish_prune, finish_reclaim, prepare, reclaim, recover,
    rollback,
};

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
