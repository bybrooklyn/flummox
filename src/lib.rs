//! Flummox compresses installed games and keeps them playable.
//!
//! The filesystem does the decompression, so a compressed game is still an
//! ordinary directory of ordinary files and the launcher never knows.
//!
//! Two binaries sit on this library: `flummox` drives it from a terminal, and
//! `flummox-gui` is a window that runs jobs as child processes. Everything
//! here is synchronous and free of UI concerns.

// Three modules need `unsafe`: the btrfs ioctls, the anchored opens, and the
// Landlock call. Each opts out by name, so a reviewer knows where to look and
// new `unsafe` cannot appear anywhere else. `deny` rather than `forbid`,
// because `forbid` cannot be opted out of at all.
#![deny(unsafe_code)]

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
pub mod watch;
