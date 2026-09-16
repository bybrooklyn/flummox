//! Core of gamecompressor: the shared model, filesystem probing, busy
//! detection, estimation and the compression backends.
//!
//! Everything here is synchronous and free of UI concerns. The CLI, the
//! daemon and the GUI are clients of this crate.

pub mod backend;
pub mod busy;
pub mod db;
pub mod estimate;
pub mod fsprobe;
pub mod inventory;
pub mod model;
pub mod safeio;
pub mod sandbox;
