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

#[cfg(target_os = "linux")]
pub mod backend;
#[cfg(target_os = "linux")]
pub mod benchmark;
#[cfg(target_os = "linux")]
pub mod busy;
pub mod classify;
#[cfg(target_os = "linux")]
pub mod cli;
pub mod compatibility;
#[cfg(target_os = "linux")]
pub mod db;
#[cfg(target_os = "linux")]
pub mod estimate;
#[cfg(target_os = "linux")]
pub mod fsprobe;
#[cfg(feature = "gui")]
pub mod gui;
#[cfg(target_os = "linux")]
pub mod inventory;
#[cfg(target_os = "linux")]
pub mod jobs;
#[cfg(target_os = "linux")]
pub mod launchers;
pub mod model;
#[cfg(target_os = "linux")]
pub mod pack;
mod path_serde;
#[cfg(target_os = "linux")]
pub mod recommendation;
#[cfg(target_os = "linux")]
pub mod safeio;
#[cfg(target_os = "linux")]
pub mod sandbox;
pub mod testutil;
#[cfg(target_os = "linux")]
pub mod watch;
#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
#[path = "launchers/vdf.rs"]
mod windows_vdf;
