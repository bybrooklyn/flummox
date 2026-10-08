//! Flummox compresses installed games and keeps them playable.
//!
//! The filesystem does the decompression, so a compressed game is still an
//! ordinary directory of ordinary files and the launcher never knows.
//!
//! Two binaries sit on this library: `flummox` drives it from a terminal, and
//! `flummox-gui` is a window that runs jobs as child processes. Everything
//! here is synchronous and free of UI concerns.

// Eleven files carry `#![allow(unsafe_code)]`: `backend/btrfs.rs`, `busy.rs`,
// `fsprobe.rs`, `macos.rs`, `safeio.rs`, `storage.rs` and five under
// `windows/`. Each opts out by name, so new `unsafe` cannot appear anywhere
// else unnoticed. `deny` rather than `forbid`, because `forbid` cannot be
// opted out of at all.
#![deny(unsafe_code)]

pub mod allocation;
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
pub mod desktop;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod desktop_discovery;
pub mod desktop_jobs;
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
pub mod libraries;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod model;
pub mod observer;
#[cfg(any(windows, target_os = "macos"))]
pub mod native;
#[cfg(target_os = "macos")]
#[path = "launchers/vdf.rs"]
mod native_vdf;
#[cfg(target_os = "linux")]
pub mod pack;
mod path_serde;
pub mod qualification;
#[cfg(target_os = "linux")]
pub mod recommendation;
#[cfg(target_os = "linux")]
pub mod safeio;
#[cfg(target_os = "linux")]
pub mod sandbox;
pub mod storage;
pub mod testutil;
#[cfg(target_os = "linux")]
pub mod watch;
#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
#[path = "launchers/vdf.rs"]
mod windows_vdf;
