//! Flummox compresses installed games and keeps them playable.
//!
//! The filesystem does the decompression, so a compressed game is still an
//! ordinary directory of ordinary files and the launcher never knows.
//!
//! Two binaries sit on this library: `flummox` drives it from a terminal, and
//! `flummox-gui` is a window that runs jobs as child processes. Everything
//! here is synchronous and free of UI concerns.

pub mod backend;
pub mod busy;
pub mod cli;
pub mod db;
pub mod estimate;
pub mod fsprobe;
#[cfg(feature = "gui")]
pub mod gui;
pub mod inventory;
pub mod launchers;
pub mod model;
pub mod safeio;
pub mod sandbox;
pub mod testutil;
