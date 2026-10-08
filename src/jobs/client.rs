//! The client side of coordinator IPC: locating state, connecting, and
//! starting or replacing the coordinator.

use super::*;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::Connection;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::os::unix::{
    fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    net::UnixStream,
};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Largest IPC message in bytes, counting its terminating newline.
pub(super) const LIMIT: u64 = 8 * 1024 * 1024;

/// The owner-only folder holding the socket, queue and locks, created on
/// first use. Fails unless it is a real directory owned by this user.
pub fn state_dir() -> Result<PathBuf> {
    let path = crate::db::Db::default_path().context("Cannot locate Flummox's state folder")?;
    let dir = path
        .parent()
        .context("Invalid state folder")?
        .join("desktop");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let meta = std::fs::symlink_metadata(&dir)?;
    ensure!(
        meta.is_dir() && meta.uid() == nix::unistd::geteuid().as_raw(),
        "Unsafe Flummox state folder"
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// The executable to run as coordinator or worker: `flummox` beside the
/// current executable when that file exists, otherwise the current executable.
pub(super) fn binary() -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    let sibling = current.with_file_name("flummox");
    if sibling.is_file() {
        Ok(sibling)
    } else {
        ensure!(
            current.is_file(),
            "The Flummox worker executable is missing"
        );
        Ok(current)
    }
}

/// Connects to the same user's coordinator, starting it when necessary.
/// The socket is reachable only through an owner-only directory.
///
/// An idle coordinator left running by an older version is replaced first.
pub fn request(command: Command) -> Result<Snapshot> {
    exchange(command, true)
}

/// Sends one command and returns the coordinator's state after it.
/// `may_replace` permits one restart of a coordinator from an older version.
pub(super) fn exchange(command: Command, may_replace: bool) -> Result<Snapshot> {
    let restarting = matches!(&command, Command::Restart);
    let dir = state_dir()?;
    let socket = dir.join("control.sock");
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        // Nothing is listening. Start a coordinator in its own process group,
        // so it outlives this client, and wait up to 5 seconds for its socket.
        Err(_) => {
            std::process::Command::new(binary()?)
                .arg("__coordinator")
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(dir.join("service.log"))?,
                )
                .spawn()?;
            let started = Instant::now();
            loop {
                if let Ok(stream) = UnixStream::connect(&socket) {
                    break stream;
                }
                ensure!(
                    started.elapsed() < Duration::from_secs(5),
                    "The background worker did not start. See service.log in Flummox's state folder."
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    // The coordinator runs these inside the request and replies when the
    // transaction ends, so they get a two-hour read timeout.
    let filesystem_transaction = matches!(
        &command,
        Command::PackActivate { .. }
            | Command::PackRollback { .. }
            | Command::PackReclaim { .. }
            | Command::PackCompact { .. }
            | Command::PackPrune { .. }
    );
    stream.set_read_timeout(Some(Duration::from_secs(if filesystem_transaction {
        7_200
    } else {
        8
    })))?;
    stream.set_write_timeout(Some(Duration::from_secs(8)))?;
    serde_json::to_writer(
        &mut stream,
        &Request {
            version: VERSION,
            command: command.clone(),
        },
    )?;
    stream.write_all(b"\n")?;
    // Check the envelope before decoding a snapshot from a different schema.
    let response: serde_json::Value = read_message(&mut BufReader::new(stream))?;
    let running = response.get("version").and_then(serde_json::Value::as_u64);
    if !restarting && running != Some(u64::from(VERSION)) {
        // Only an older coordinator is replaced. If each version replaced the
        // other, two installed copies would restart the coordinator in turn.
        ensure!(
            running.is_some_and(|version| version < u64::from(VERSION)),
            "The background worker belongs to a newer Flummox. Update this copy, or close the newer one and run flummox jobs restart."
        );
        ensure!(
            may_replace,
            "The background worker still uses an older protocol after restarting. Log out and back in."
        );
        exchange(Command::Restart, false).context(
            "The background worker belongs to an older Flummox and could not be replaced",
        )?;
        return exchange(command, false);
    }
    if let Some(error) = response.get("error").and_then(serde_json::Value::as_str) {
        bail!("{error}");
    }
    // The old coordinator replies before it exits. Wait until it releases
    // owner.lock, then send a request that starts the installed executable.
    if restarting {
        let owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("owner.lock"))?;
        let started = Instant::now();
        loop {
            if owner.try_lock().is_ok() {
                drop(owner);
                return request(Command::Snapshot);
            }
            ensure!(
                started.elapsed() < Duration::from_secs(5),
                "The worker has not finished restarting. Try again shortly."
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    serde_json::from_value(
        response
            .get("snapshot")
            .context("The worker returned no state")?
            .clone(),
    )
    .context("Invalid worker state")
}

/// Reads one newline-terminated JSON message of at most [`LIMIT`] bytes.
/// A line without its newline is rejected as truncated.
pub(super) fn read_message<T: serde::de::DeserializeOwned>(reader: &mut impl BufRead) -> Result<T> {
    let mut line = String::new();
    reader.take(LIMIT + 1).read_line(&mut line)?;
    ensure!(
        line.len() as u64 <= LIMIT && line.ends_with('\n'),
        "Incomplete or oversized worker message"
    );
    Ok(serde_json::from_str(&line)?)
}

/// Fails reads after a deadline. The socket timeout bounds one read, so a
/// client sending a byte at a time could otherwise hold the accept loop.
pub(super) struct Within<R> {
    pub(super) inner: R,
    pub(super) until: Instant,
}

impl<R: Read> Read for Within<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if Instant::now() >= self.until {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "The request took too long to arrive",
            ));
        }
        self.inner.read(buffer)
    }
}

/// Reads custom library configuration without starting the coordinator.
pub fn configured_libraries() -> Result<Vec<Library>> {
    let Some(path) = crate::db::Db::default_path()
        .and_then(|p| p.parent().map(|p| p.join("desktop/queue.sqlite")))
    else {
        return Ok(vec![]);
    };
    if !path.exists() {
        return Ok(vec![]);
    }
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = db.prepare("SELECT data FROM settings WHERE id=1")?;
    let Some(row) = stmt.query_map([], |r| r.get::<_, String>(0))?.next() else {
        return Ok(vec![]);
    };
    let (libraries, _): (Vec<Library>, Vec<String>) = serde_json::from_str(&row?)?;
    Ok(libraries)
}
