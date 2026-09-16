//! The state database: what we compressed, and what the files looked like
//! when we did it.
//!
//! Compressing a 60 GB install is expensive, so it must not happen twice for
//! the same bytes. After a Steam update only a handful of files have actually
//! been rewritten, and the only way to know which is to have recorded a
//! fingerprint of each file at the time of the last pass. That is what
//! [`Db::changed_since`] answers, and it is the reason this module exists.
//!
//! The second reason is [`Db::level_applied`]. The estimator samples a file
//! and predicts what compressing it would save, but it cannot see the
//! difference between "nothing has tried to compress this" and "the kernel
//! tried at level 15, the result did not free a whole sector, and so it stored
//! the block uncompressed". Both look identical on disk. Measured against real
//! installs the gap is large: Celeste was still being advertised as having
//! ~45 MB to gain immediately after a max pass, and Balatro was predicted to
//! free 717 kB where it actually freed 369 kB. Recording the level last
//! applied to each file gives the estimator the missing fact. Teaching it to
//! use that fact is a separate change; this module only stores it.
//!
//! Everything here is synchronous. The database is small — a few thousand rows
//! per game — and every caller is already doing filesystem work, so the cost
//! of a connection per process is not worth avoiding.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, named_params, params};
use thiserror::Error;

use crate::backend::{CompressOpts, Preset};
use crate::fsprobe::BackendKind;
use crate::inventory::{FileEntry, Inventory};
use crate::model::{GameId, Launcher};

/// Anything that can go wrong talking to the state database.
#[derive(Debug, Error)]
pub enum DbError {
    /// SQLite itself refused.
    #[error("state database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database file or its directory could not be created.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path being created or opened.
        path: PathBuf,
        /// What the operating system said.
        #[source]
        source: std::io::Error,
    },
    /// A value is too large for SQLite's 64-bit integer columns.
    ///
    /// Only nanosecond timestamps can reach this: they are held as `i128`
    /// in an [`FileEntry`], and a clock would have to be roughly 292 years
    /// from the epoch to overflow.
    #[error("{what} does not fit a 64-bit column: {value}")]
    OutOfRange {
        /// Which field overflowed.
        what: &'static str,
        /// The value that would not fit.
        value: i128,
    },
    /// A stored value is not one this version of the code understands.
    ///
    /// Reached only by a hand-edited or corrupted database, or by opening a
    /// file written by a newer release.
    #[error("stored {what} is not recognised: {value:?}")]
    Unreadable {
        /// Which column held it.
        what: &'static str,
        /// What was in it.
        value: String,
    },
}

/// The result type every operation on the database returns.
pub type Result<T> = std::result::Result<T, DbError>;

/// The schema version this build writes, held in SQLite's `user_version`.
///
/// Bumping this and adding another arm to [`migrate`] is the whole upgrade
/// procedure. Migrations are additive: a new release adds tables or nullable
/// columns, so an older build can still open the file and read what it knows.
const SCHEMA_VERSION: i32 = 1;

/// The `level_applied` value meaning no compression was ever attempted.
///
/// Zero is not a real zstd level, so it cannot collide with one, and it
/// compares correctly: "already attempted at or above the target level" is
/// false for any target a job would actually ask for.
pub const NOT_ATTEMPTED: i32 = 0;

/// The first schema.
///
/// `relpath` and `path` are BLOBs because a Linux path is a sequence of bytes,
/// not a string. Most game files are ASCII, but some installer or mod will
/// eventually write a name that is not valid UTF-8, and storing it as TEXT
/// would either fail or silently mangle it — and a mangled path means the file
/// is fingerprinted under a name that never matches again, so it is
/// recompressed on every single pass.
const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS games(
    id                TEXT PRIMARY KEY,
    launcher          TEXT NOT NULL,
    title             TEXT NOT NULL,
    path              BLOB NOT NULL,
    backend           TEXT NOT NULL,
    level             INTEGER NOT NULL,
    preset            TEXT NOT NULL,
    build_at_compress TEXT,
    compressed_at     INTEGER NOT NULL,
    install_bytes     INTEGER NOT NULL,
    disk_before       INTEGER NOT NULL,
    disk_after        INTEGER NOT NULL,
    est_saving        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS files(
    game_id       TEXT NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    relpath       BLOB NOT NULL,
    size          INTEGER NOT NULL,
    ino           INTEGER NOT NULL,
    mtime_ns      INTEGER NOT NULL,
    ctime_ns      INTEGER NOT NULL,
    level_applied INTEGER NOT NULL,
    PRIMARY KEY(game_id, relpath)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS jobs(
    id       INTEGER PRIMARY KEY,
    game_id  TEXT NOT NULL,
    kind     TEXT NOT NULL,
    state    TEXT NOT NULL,
    created  INTEGER NOT NULL,
    started  INTEGER,
    finished INTEGER,
    cursor   BLOB,
    error    TEXT
);
CREATE INDEX IF NOT EXISTS jobs_by_state ON jobs(state, created);

CREATE TABLE IF NOT EXISTS activity(
    id          INTEGER PRIMARY KEY,
    ts          INTEGER NOT NULL,
    level       TEXT NOT NULL,
    game_id     TEXT,
    kind        TEXT NOT NULL,
    message     TEXT NOT NULL,
    bytes_delta INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS activity_by_time ON activity(ts DESC, id DESC);
";

/// What one file looked like when it was last compressed.
///
/// The four identity fields are the same ones [`FileEntry::changed_since`]
/// compares, so a fingerprint and a fresh walk entry can be checked against
/// each other directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFingerprint {
    /// Size in bytes.
    pub size: u64,
    /// Inode number.
    pub ino: u64,
    /// Modification time in nanoseconds.
    pub mtime_ns: i128,
    /// Inode change time in nanoseconds.
    pub ctime_ns: i128,
    /// The zstd level last applied, or [`NOT_ATTEMPTED`].
    pub level_applied: i32,
}

impl FileFingerprint {
    /// Whether a freshly walked file is still the file this fingerprint
    /// describes.
    pub fn matches(&self, entry: &FileEntry) -> bool {
        self.size == entry.size
            && self.ino == entry.ino
            && self.mtime_ns == entry.mtime_ns
            && self.ctime_ns == entry.ctime_ns
    }
}

/// One compressed game, as the database holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameRecord {
    /// Which game.
    pub id: GameId,
    /// Display name at the time of the pass.
    pub title: String,
    /// Absolute path to the install directory.
    pub install_dir: PathBuf,
    /// The backend that did the work.
    pub backend: BackendKind,
    /// The zstd level the job used.
    pub level: i32,
    /// The preset the level came from, kept separately because an explicit
    /// `--level` can override a preset and the UI still wants to say which
    /// preset was chosen.
    pub preset: Preset,
    /// The launcher's build id when the pass ran, so a later run can tell
    /// that the game has been updated even before walking it.
    pub build: Option<String>,
    /// When the pass finished, in seconds since the Unix epoch.
    pub compressed_at: i64,
    /// Total size of the files in the install directory.
    pub install_bytes: u64,
    /// Disk usage before the pass.
    pub disk_before: u64,
    /// Disk usage after it.
    pub disk_after: u64,
    /// What the estimator predicted it would save.
    ///
    /// Kept so a later run can compare the prediction against
    /// `disk_before - disk_after` and show how far off it was.
    pub est_saving: i64,
}

impl GameRecord {
    /// Builds a record for a pass that is finishing now.
    ///
    /// The byte figures start at zero; the caller fills them in from its
    /// [`crate::backend::Outcome`] and its estimate before storing.
    pub fn new(
        id: GameId,
        title: impl Into<String>,
        install_dir: impl Into<PathBuf>,
        backend: BackendKind,
        opts: &CompressOpts,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            install_dir: install_dir.into(),
            backend,
            level: opts.btrfs_level(),
            preset: opts.preset,
            build: None,
            compressed_at: now_secs(),
            install_bytes: 0,
            disk_before: 0,
            disk_after: 0,
            est_saving: 0,
        }
    }
}

/// How much an activity entry matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityLevel {
    /// Something happened, as intended.
    Info,
    /// Something was skipped or degraded but the job carried on.
    Warn,
    /// Something failed.
    Error,
}

impl ActivityLevel {
    /// The name stored in the database and shown in the UI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    /// Reads a level back, or `None` if it is not one we know.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "info" => Some(Self::Info),
            "warn" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// One line of the activity log the UI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// Row id. Zero on an entry that has not been stored yet.
    pub id: i64,
    /// When it happened, in seconds since the Unix epoch.
    pub ts: i64,
    /// How much it matters.
    pub level: ActivityLevel,
    /// The game it concerns, where it concerns one.
    pub game_id: Option<GameId>,
    /// A short stable event name, such as `compress` or `decompress`.
    pub kind: String,
    /// The sentence shown to the user.
    pub message: String,
    /// The change in disk usage, negative when the event freed space.
    pub bytes_delta: i64,
}

impl Activity {
    /// A new entry, stamped with the current time.
    pub fn new(level: ActivityLevel, kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: 0,
            ts: now_secs(),
            level,
            game_id: None,
            kind: kind.into(),
            message: message.into(),
            bytes_delta: 0,
        }
    }

    /// Attaches the game this entry is about.
    pub fn for_game(mut self, id: &GameId) -> Self {
        self.game_id = Some(id.clone());
        self
    }

    /// Records how much disk usage changed, negative when space was freed.
    pub fn with_bytes(mut self, delta: i64) -> Self {
        self.bytes_delta = delta;
        self
    }
}

/// A connection to the state database.
#[derive(Debug)]
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens the database at `path`, creating it and its directory.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|source| DbError::Io { path: parent.to_path_buf(), source })?;
        }
        let conn = Connection::open(path)?;
        tracing::debug!(path = %path.display(), "opened the state database");
        Self::prepare(conn)
    }

    /// Opens a private database that never touches the disk, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::prepare(Connection::open_in_memory()?)
    }

    /// Where the database lives by default.
    ///
    /// `$XDG_STATE_HOME/flummox/state.sqlite`, falling back to
    /// `~/.local/state/...`. A relative `XDG_STATE_HOME` is ignored, as the
    /// XDG specification requires. `None` means neither variable is usable,
    /// which happens in a daemon started with an empty environment; the caller
    /// then has to be told a path explicitly.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
        Some(base.join("flummox").join("state.sqlite"))
    }

    /// Applies the connection settings, then brings the schema up to date.
    fn prepare(conn: Connection) -> Result<Self> {
        // WAL lets the UI read the log while a job is writing to it. An
        // in-memory database cannot do WAL and answers "memory" instead, so
        // the reply is recorded rather than checked.
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        tracing::debug!(journal_mode = %mode, "state database journal mode");
        // Off by default in SQLite, and the files table depends on it to keep
        // fingerprints from outliving the game row they belong to.
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Stores a finished pass: the game row and a fingerprint for every file
    /// the walk saw, in one transaction.
    ///
    /// Fingerprints are recorded for skipped files too, not just compressed
    /// ones. Without them a file below the size floor would have no stored
    /// fingerprint and [`Db::changed_since`] would report it as new on every
    /// run. Such files carry [`NOT_ATTEMPTED`] as their level.
    ///
    /// The file rows are replaced wholesale rather than merged, because the
    /// inventory is the complete truth about the install directory: a file the
    /// game update deleted must not keep a fingerprint.
    pub fn record_compression(&mut self, game: &GameRecord, inv: &Inventory) -> Result<()> {
        let id = game.id.to_string();
        let tx = self.conn.transaction()?;
        // An explicit upsert rather than INSERT OR REPLACE: REPLACE deletes
        // the old row first, and that would cascade through the foreign key
        // and silently take every fingerprint with it.
        tx.execute(
            "INSERT INTO games(
                 id, launcher, title, path, backend, level, preset,
                 build_at_compress, compressed_at, install_bytes,
                 disk_before, disk_after, est_saving)
             VALUES(:id, :launcher, :title, :path, :backend, :level, :preset,
                 :build, :compressed_at, :install_bytes,
                 :disk_before, :disk_after, :est_saving)
             ON CONFLICT(id) DO UPDATE SET
                 launcher = excluded.launcher,
                 title = excluded.title,
                 path = excluded.path,
                 backend = excluded.backend,
                 level = excluded.level,
                 preset = excluded.preset,
                 build_at_compress = excluded.build_at_compress,
                 compressed_at = excluded.compressed_at,
                 install_bytes = excluded.install_bytes,
                 disk_before = excluded.disk_before,
                 disk_after = excluded.disk_after,
                 est_saving = excluded.est_saving",
            named_params! {
                ":id": &id,
                ":launcher": game.id.launcher.slug(),
                ":title": &game.title,
                ":path": game.install_dir.as_os_str().as_bytes(),
                ":backend": game.backend.label(),
                ":level": game.level,
                ":preset": game.preset.label(),
                ":build": &game.build,
                ":compressed_at": game.compressed_at,
                ":install_bytes": as_i64(game.install_bytes),
                ":disk_before": as_i64(game.disk_before),
                ":disk_after": as_i64(game.disk_after),
                ":est_saving": game.est_saving,
            },
        )?;
        tx.execute("DELETE FROM files WHERE game_id = ?1", params![&id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO files(
                     game_id, relpath, size, ino, mtime_ns, ctime_ns, level_applied)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for file in &inv.files {
                let applied = if file.action.is_compress() { game.level } else { NOT_ATTEMPTED };
                stmt.execute(params![
                    &id,
                    file.rel.as_os_str().as_bytes(),
                    as_i64(file.size),
                    as_i64(file.ino),
                    ns_to_i64("mtime_ns", file.mtime_ns)?,
                    ns_to_i64("ctime_ns", file.ctime_ns)?,
                    applied,
                ])?;
            }
        }
        tx.commit()?;
        tracing::debug!(game = %game.id, files = inv.files.len(), "recorded a compression pass");
        Ok(())
    }

    /// Reads back a game, or `None` if it was never compressed.
    pub fn game(&self, id: &GameId) -> Result<Option<GameRecord>> {
        let raw = self
            .conn
            .query_row(
                "SELECT launcher, title, path, backend, level, preset,
                        build_at_compress, compressed_at, install_bytes,
                        disk_before, disk_after, est_saving
                 FROM games WHERE id = ?1",
                params![id.to_string()],
                |row| {
                    Ok(RawGame {
                        launcher: row.get(0)?,
                        title: row.get(1)?,
                        path: row.get(2)?,
                        backend: row.get(3)?,
                        level: row.get(4)?,
                        preset: row.get(5)?,
                        build: row.get(6)?,
                        compressed_at: row.get(7)?,
                        install_bytes: row.get(8)?,
                        disk_before: row.get(9)?,
                        disk_after: row.get(10)?,
                        est_saving: row.get(11)?,
                    })
                },
            )
            .optional()?;
        let Some(raw) = raw else { return Ok(None) };
        raw.into_record(id).map(Some)
    }

    /// Every stored fingerprint for a game, keyed by path relative to the
    /// install directory.
    pub fn fingerprints(&self, id: &GameId) -> Result<HashMap<PathBuf, FileFingerprint>> {
        let mut stmt = self.conn.prepare(
            "SELECT relpath, size, ino, mtime_ns, ctime_ns, level_applied
             FROM files WHERE game_id = ?1",
        )?;
        let rows = stmt.query_map(params![id.to_string()], |row| {
            let relpath: Vec<u8> = row.get(0)?;
            let size: i64 = row.get(1)?;
            let ino: i64 = row.get(2)?;
            Ok((
                blob_to_path(relpath),
                FileFingerprint {
                    size: as_u64(size),
                    ino: as_u64(ino),
                    mtime_ns: i128::from(row.get::<_, i64>(3)?),
                    ctime_ns: i128::from(row.get::<_, i64>(4)?),
                    level_applied: row.get(5)?,
                },
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (rel, fingerprint) = row?;
            out.insert(rel, fingerprint);
        }
        Ok(out)
    }

    /// The files in `inv` that a fresh pass would actually have to touch.
    ///
    /// A file is returned if its size, inode, mtime or ctime differ from the
    /// stored fingerprint, or if it has no fingerprint at all. This is the
    /// point of the whole table: after a Steam update most of a 60 GB install
    /// is byte-identical, and only what comes back here needs recompressing.
    ///
    /// A game that was never recorded has no fingerprints, so every file comes
    /// back — which is the right answer for a first pass.
    pub fn changed_since(&self, id: &GameId, inv: &Inventory) -> Result<Vec<FileEntry>> {
        let stored = self.fingerprints(id)?;
        Ok(inv
            .files
            .iter()
            .filter(|entry| stored.get(&entry.rel).is_none_or(|fp| !fp.matches(entry)))
            .cloned()
            .collect())
    }

    /// The zstd level last applied to one file, or `None` if the file has no
    /// fingerprint.
    ///
    /// [`NOT_ATTEMPTED`] means the file was walked but deliberately skipped,
    /// which is a different thing from never having been seen. The estimator
    /// needs the distinction: a file already attempted at or above the target
    /// level has nothing further to give, even where the kernel left its
    /// blocks stored uncompressed because compressing them did not free a
    /// whole sector.
    pub fn level_applied(&self, id: &GameId, rel: &Path) -> Result<Option<i32>> {
        let level = self
            .conn
            .query_row(
                "SELECT level_applied FROM files WHERE game_id = ?1 AND relpath = ?2",
                params![id.to_string(), rel.as_os_str().as_bytes()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(level)
    }

    /// Appends an entry to the activity log and returns its row id.
    pub fn log_activity(&self, entry: &Activity) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO activity(ts, level, game_id, kind, message, bytes_delta)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                entry.ts,
                entry.level.as_str(),
                entry.game_id.as_ref().map(GameId::to_string),
                &entry.kind,
                &entry.message,
                entry.bytes_delta,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The most recent entries, newest first.
    ///
    /// Ties on the timestamp are broken by row id, so two events logged in the
    /// same second still come back in the order they happened.
    pub fn recent_activity(&self, limit: u32) -> Result<Vec<Activity>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, level, game_id, kind, message, bytes_delta
             FROM activity ORDER BY ts DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![i64::from(limit)], |row| {
            let level: String = row.get(2)?;
            let game_id: Option<String> = row.get(3)?;
            Ok(Activity {
                id: row.get(0)?,
                ts: row.get(1)?,
                // The log is advisory, so an unreadable level or id degrades
                // to something displayable rather than failing the read.
                level: ActivityLevel::parse(&level).unwrap_or(ActivityLevel::Info),
                game_id: game_id.as_deref().and_then(parse_game_id),
                kind: row.get(4)?,
                message: row.get(5)?,
                bytes_delta: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Drops everything stored about a game, for a decompress.
    ///
    /// The activity log goes too, so the UI should log the decompress *after*
    /// calling this or the new entry will be removed along with the old ones.
    pub fn forget(&mut self, id: &GameId) -> Result<()> {
        let key = id.to_string();
        let tx = self.conn.transaction()?;
        // The foreign key would cascade the file rows away on its own, but
        // deleting them here means the result does not depend on a pragma
        // having been set.
        tx.execute("DELETE FROM files WHERE game_id = ?1", params![&key])?;
        tx.execute("DELETE FROM jobs WHERE game_id = ?1", params![&key])?;
        tx.execute("DELETE FROM activity WHERE game_id = ?1", params![&key])?;
        tx.execute("DELETE FROM games WHERE id = ?1", params![&key])?;
        tx.commit()?;
        tracing::debug!(game = %id, "forgot a game");
        Ok(())
    }
}

/// The `games` columns as SQLite hands them over, before they become types.
///
/// Kept separate because turning a launcher slug or a preset name back into an
/// enum can fail, and that failure is a [`DbError`] rather than anything
/// SQLite knows how to report from inside a row callback.
struct RawGame {
    launcher: String,
    title: String,
    path: Vec<u8>,
    backend: String,
    level: i32,
    preset: String,
    build: Option<String>,
    compressed_at: i64,
    install_bytes: i64,
    disk_before: i64,
    disk_after: i64,
    est_saving: i64,
}

impl RawGame {
    /// Rebuilds a record, given the id it was looked up by.
    fn into_record(self, id: &GameId) -> Result<GameRecord> {
        let launcher = launcher_from_slug(&self.launcher)
            .ok_or_else(|| DbError::Unreadable { what: "launcher", value: self.launcher.clone() })?;
        let backend = backend_from_label(&self.backend)
            .ok_or_else(|| DbError::Unreadable { what: "backend", value: self.backend.clone() })?;
        let preset = preset_from_label(&self.preset)
            .ok_or_else(|| DbError::Unreadable { what: "preset", value: self.preset.clone() })?;
        Ok(GameRecord {
            // The launcher comes from its own column rather than from the id
            // text, so a query can group by it without parsing every id.
            id: GameId::new(launcher, id.key.clone()),
            title: self.title,
            install_dir: blob_to_path(self.path),
            backend,
            level: self.level,
            preset,
            build: self.build,
            compressed_at: self.compressed_at,
            install_bytes: as_u64(self.install_bytes),
            disk_before: as_u64(self.disk_before),
            disk_after: as_u64(self.disk_after),
            est_saving: self.est_saving,
        })
    }
}

/// Brings the schema up to [`SCHEMA_VERSION`].
///
/// Safe to call on an already-current database: it reads `user_version` first
/// and does nothing when there is nothing to do. A file written by a newer
/// build is left alone rather than downgraded.
fn migrate(conn: &Connection) -> Result<()> {
    let current: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current >= SCHEMA_VERSION {
        return Ok(());
    }
    tracing::info!(from = current, to = SCHEMA_VERSION, "migrating the state database");
    if current < 1 {
        conn.execute_batch(SCHEMA_V1)?;
    }
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// The launcher a stored slug names.
fn launcher_from_slug(slug: &str) -> Option<Launcher> {
    [
        Launcher::Steam,
        Launcher::HeroicLegendary,
        Launcher::HeroicGog,
        Launcher::HeroicNile,
        Launcher::HeroicSideload,
        Launcher::Lutris,
        Launcher::Bottles,
        Launcher::Manual,
    ]
    .into_iter()
    .find(|l| l.slug() == slug)
}

/// The backend a stored label names.
fn backend_from_label(label: &str) -> Option<BackendKind> {
    [BackendKind::Btrfs, BackendKind::Bcachefs, BackendKind::Pack]
        .into_iter()
        .find(|k| k.label() == label)
}

/// The preset a stored label names.
fn preset_from_label(label: &str) -> Option<Preset> {
    [Preset::Fast, Preset::Balanced, Preset::Max].into_iter().find(|p| p.label() == label)
}

/// Parses the `launcher:key` text used as a game's primary key.
fn parse_game_id(text: &str) -> Option<GameId> {
    let (slug, key) = text.split_once(':')?;
    Some(GameId::new(launcher_from_slug(slug)?, key))
}

/// Rebuilds a path from the bytes stored in a BLOB column.
fn blob_to_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}

/// Reinterprets a `u64` as the `i64` SQLite stores.
///
/// SQLite has no unsigned integers. The two's-complement bit pattern is
/// preserved exactly, so [`as_u64`] gives the original value back; only a
/// direct `SELECT` of a value above `i64::MAX` would read as negative, and no
/// real size or inode reaches that.
fn as_i64(value: u64) -> i64 {
    value as i64
}

/// The inverse of [`as_i64`].
fn as_u64(value: i64) -> u64 {
    value as u64
}

/// Narrows a nanosecond timestamp to the width of a SQLite column.
fn ns_to_i64(what: &'static str, value: i128) -> Result<i64> {
    i64::try_from(value).map_err(|_| DbError::OutOfRange { what, value })
}

/// Seconds since the Unix epoch.
///
/// A clock set before 1970 reads as 0 rather than failing: a wrong timestamp
/// in the log is not worth refusing to record the pass over.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use crate::testutil::{Ctx, TestResult, check, check_eq};

    use super::*;
    use crate::inventory::Action;

    /// A walk entry with a fingerprint that is easy to vary.
    fn entry(rel: &str, size: u64) -> FileEntry {
        FileEntry {
            rel: PathBuf::from(rel),
            size,
            ino: 42,
            mtime_ns: 1_000_000_000,
            ctime_ns: 2_000_000_000,
            action: Action::Compress,
        }
    }

    /// An inventory holding exactly these files.
    fn inventory(files: Vec<FileEntry>) -> Inventory {
        Inventory { files, warnings: Vec::new() }
    }

    /// A game record with every field set to something distinguishable.
    fn record(id: &GameId) -> GameRecord {
        GameRecord {
            id: id.clone(),
            title: "Celeste".to_owned(),
            install_dir: PathBuf::from("/games/Celeste"),
            backend: BackendKind::Btrfs,
            level: 15,
            preset: Preset::Max,
            build: Some("9876".to_owned()),
            compressed_at: 1_700_000_000,
            install_bytes: 1_200_000_000,
            disk_before: 1_100_000_000,
            disk_after: 800_000_000,
            est_saving: 45_000_000,
        }
    }

    fn celeste() -> GameId {
        GameId::new(Launcher::Steam, "504230")
    }

    #[test]
    fn migrations_run_twice_without_complaint() -> TestResult {
        let db = Db::open_in_memory().ctx("open an in-memory database")?;
        // open_in_memory already migrated once; these are the second and third.
        migrate(&db.conn).ctx("migrate again")?;
        migrate(&db.conn).ctx("migrate a third time")?;

        let version: i32 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .ctx("read user_version")?;
        check_eq(version, SCHEMA_VERSION, "the schema version is recorded once")?;
        let foreign_keys: i32 = db
            .conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .ctx("read the foreign_keys pragma")?;
        check_eq(foreign_keys, 1, "foreign keys are enforced")
    }

    #[test]
    fn a_game_round_trips_and_a_second_pass_replaces_its_files() -> TestResult {
        let mut db = Db::open_in_memory().ctx("open")?;
        let id = celeste();
        let stored = record(&id);
        let inv = inventory(vec![entry("Content/atlas.dat", 100), entry("Celeste.dll", 200)]);
        db.record_compression(&stored, &inv).ctx("record the pass")?;

        let back = db.game(&id).ctx("read the game")?.ctx("the game should be stored")?;
        check_eq(back, stored.clone(), "every column comes back unchanged")?;

        let prints = db.fingerprints(&id).ctx("read the fingerprints")?;
        check_eq(prints.len(), 2, "one fingerprint per file")?;
        let atlas =
            prints.get(Path::new("Content/atlas.dat")).ctx("no fingerprint for the atlas")?;
        check_eq(atlas.size, 100, "the stored size")?;
        check_eq(atlas.ino, 42, "the stored inode")?;
        check_eq(atlas.mtime_ns, 1_000_000_000, "the stored mtime")?;
        check_eq(atlas.level_applied, 15, "the level the pass applied")?;

        // A game update deleted one file; its fingerprint must not linger.
        let second = inventory(vec![entry("Content/atlas.dat", 100)]);
        db.record_compression(&stored, &second).ctx("record a second pass")?;
        let prints = db.fingerprints(&id).ctx("re-read the fingerprints")?;
        check_eq(prints.len(), 1, "a file that went away loses its fingerprint")?;
        check(
            prints.contains_key(Path::new("Content/atlas.dat")),
            "the surviving file keeps its fingerprint",
        )
    }

    #[test]
    fn changed_since_finds_rewritten_and_new_files_only() -> TestResult {
        let mut db = Db::open_in_memory().ctx("open")?;
        let id = celeste();
        let original =
            inventory(vec![entry("a.dat", 100), entry("b.dat", 200), entry("c.dat", 300)]);
        db.record_compression(&record(&id), &original).ctx("record the pass")?;

        let unchanged = db.changed_since(&id, &original).ctx("compare an identical walk")?;
        check(unchanged.is_empty(), format!("an identical inventory changed nothing: {unchanged:?}"))?;

        // One file rewritten in place, one grown, one added, one untouched.
        let later = inventory(vec![
            FileEntry { mtime_ns: 9_000_000_000, ..entry("a.dat", 100) },
            entry("b.dat", 250),
            entry("c.dat", 300),
            entry("d.dat", 400),
        ]);
        let changed = db.changed_since(&id, &later).ctx("compare a later walk")?;
        let mut names: Vec<String> =
            changed.iter().map(|f| f.rel.display().to_string()).collect();
        names.sort();
        let expected: Vec<String> =
            ["a.dat", "b.dat", "d.dat"].iter().map(|s| (*s).to_owned()).collect();
        check_eq(names, expected, "a new mtime, a new size and a new file, but not c.dat")?;

        // Nothing recorded at all means everything is work to do.
        let fresh = GameId::new(Launcher::Steam, "220");
        let all = db.changed_since(&fresh, &original).ctx("compare an unrecorded game")?;
        check_eq(all.len(), 3, "an unknown game has every file to do")
    }

    #[test]
    fn a_non_utf8_relpath_round_trips_byte_for_byte() -> TestResult {
        let mut db = Db::open_in_memory().ctx("open")?;
        let id = celeste();
        let raw: &[u8] = b"bad\xff/name.dat";
        let rel = PathBuf::from(OsStr::from_bytes(raw));
        let inv = inventory(vec![FileEntry { rel: rel.clone(), ..entry("placeholder", 500) }]);
        db.record_compression(&record(&id), &inv).ctx("record a file SQLite cannot read as text")?;

        let prints = db.fingerprints(&id).ctx("read the fingerprints")?;
        let stored = prints.keys().next().ctx("nothing was stored")?;
        check_eq(stored.as_os_str().as_bytes(), raw, "the path comes back byte for byte")?;
        check_eq(prints.len(), 1, "exactly one fingerprint")?;

        // Looking it up by the same path must find it, not miss and report it
        // as a new file on the next pass.
        check_eq(
            db.level_applied(&id, &rel).ctx("look the level up")?,
            Some(15),
            "the level is found under the non-UTF-8 name",
        )?;
        let again = db.changed_since(&id, &inv).ctx("compare the same walk")?;
        check(again.is_empty(), format!("a non-UTF-8 path must not look changed: {again:?}"))
    }

    #[test]
    fn level_applied_separates_a_skipped_file_from_an_unknown_one() -> TestResult {
        let mut db = Db::open_in_memory().ctx("open")?;
        let id = celeste();
        let inv = inventory(vec![
            entry("big.dat", 1_000_000),
            FileEntry { action: Action::SkipTiny, ..entry("tiny.cfg", 10) },
            FileEntry { action: Action::SkipPrecompressed, ..entry("movie.bik", 900_000) },
        ]);
        db.record_compression(&record(&id), &inv).ctx("record the pass")?;

        check_eq(
            db.level_applied(&id, Path::new("big.dat")).ctx("look up a compressed file")?,
            Some(15),
            "a compressed file records the level the job used",
        )?;
        check_eq(
            db.level_applied(&id, Path::new("tiny.cfg")).ctx("look up a skipped file")?,
            Some(NOT_ATTEMPTED),
            "a skipped file records that nothing was attempted",
        )?;
        check_eq(
            db.level_applied(&id, Path::new("movie.bik")).ctx("look up a precompressed file")?,
            Some(NOT_ATTEMPTED),
            "an already-compressed file was never attempted either",
        )?;
        check_eq(
            db.level_applied(&id, Path::new("never-walked.dat")).ctx("look up an unknown file")?,
            None,
            "a file we have never seen is not the same as one we skipped",
        )
    }

    #[test]
    fn the_activity_log_returns_the_newest_first() -> TestResult {
        let db = Db::open_in_memory().ctx("open")?;
        let id = celeste();
        for (ts, message) in [(10_i64, "first"), (20, "second"), (30, "third")] {
            let entry = Activity {
                ts,
                ..Activity::new(ActivityLevel::Info, "compress", message).for_game(&id)
            };
            db.log_activity(&entry).ctx("log an entry")?;
        }
        db.log_activity(&Activity {
            ts: 25,
            ..Activity::new(ActivityLevel::Warn, "skip", "a warning").with_bytes(-512)
        })
        .ctx("log a warning")?;

        let rows = db.recent_activity(3).ctx("read the log")?;
        check_eq(rows.len(), 3, "the limit is honoured")?;
        let messages: Vec<String> = rows.iter().map(|r| r.message.clone()).collect();
        let expected: Vec<String> =
            ["third", "a warning", "second"].iter().map(|s| (*s).to_owned()).collect();
        check_eq(messages, expected, "newest first, by timestamp")?;

        let newest = rows.first().ctx("no rows came back")?;
        check_eq(newest.game_id.clone(), Some(id), "the game id survives the round trip")?;
        check_eq(newest.level, ActivityLevel::Info, "the level survives too")?;
        check(newest.id > 0, "a stored entry has a real row id")?;

        let warning = rows.get(1).ctx("no second row")?;
        check_eq(warning.level, ActivityLevel::Warn, "the warning kept its level")?;
        check_eq(warning.bytes_delta, -512, "freed bytes are negative")?;
        check_eq(warning.game_id.clone(), None, "an entry with no game has no id")
    }

    #[test]
    fn forget_removes_every_row_for_a_game() -> TestResult {
        let mut db = Db::open_in_memory().ctx("open")?;
        let id = celeste();
        let other = GameId::new(Launcher::Lutris, "balatro");
        let inv = inventory(vec![entry("a.dat", 100), entry("b.dat", 200)]);
        db.record_compression(&record(&id), &inv).ctx("record Celeste")?;
        db.record_compression(&record(&other), &inv).ctx("record the other game")?;
        db.log_activity(&Activity::new(ActivityLevel::Info, "compress", "done").for_game(&id))
            .ctx("log for Celeste")?;
        db.log_activity(&Activity::new(ActivityLevel::Info, "compress", "done").for_game(&other))
            .ctx("log for the other game")?;

        db.forget(&id).ctx("forget Celeste")?;

        check(db.game(&id).ctx("look Celeste up")?.is_none(), "the game row is gone")?;
        check(db.fingerprints(&id).ctx("read fingerprints")?.is_empty(), "the file rows are gone")?;
        let log = db.recent_activity(10).ctx("read the log")?;
        check_eq(log.len(), 1, "only the other game's log entry is left")?;
        let left = log.first().ctx("no rows came back")?;
        check_eq(left.game_id.clone(), Some(other.clone()), "and it belongs to the other game")?;
        check(db.game(&other).ctx("look the other game up")?.is_some(), "the other game stays")?;
        check_eq(
            db.fingerprints(&other).ctx("read the other game's fingerprints")?.len(),
            2,
            "so do its fingerprints",
        )
    }
}
