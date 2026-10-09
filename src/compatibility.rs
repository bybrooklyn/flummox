//! Versioned, path-free evidence for enabling game-specific storage modes.

use crate::model::{Game, GameId, Launcher};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Report schema version. [`Report::validate`] rejects any other.
pub const VERSION: u32 = 1;

/// A game build without its title or installation path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameBuild {
    pub launcher: Launcher,
    pub key: String,
    pub build: String,
}

/// Longest key a report may carry.
const MAX_KEY_LEN: usize = 256;
/// Longest build or tool version a report may carry.
const MAX_TEXT_LEN: usize = 128;

impl GameBuild {
    /// The build record for `id` at `build`, with the key made path-free.
    pub fn new(id: &GameId, build: &str) -> Self {
        Self {
            launcher: id.launcher,
            key: Self::key_for(id),
            build: build.trim().into(),
        }
    }

    /// The key a report stores for `id`.
    ///
    /// A manual game is keyed by its install path, and a path names the
    /// user's folders, so manual keys, and any key holding a path separator,
    /// are stored as a BLAKE3 hash of the key. Other keys are stored as given.
    pub fn key_for(id: &GameId) -> String {
        if id.launcher == Launcher::Manual || id.key.contains(['/', '\\']) {
            let hex = blake3::hash(id.key.as_bytes()).to_hex();
            format!("hash-{}", hex.chars().take(32).collect::<String>())
        } else {
            id.key.clone()
        }
    }

    /// Rewrites the key of a custom-folder report to its hashed form and says
    /// whether it changed. A key already shaped `hash-` and 32 hex digits stays.
    fn hash_manual_key(&mut self) -> bool {
        let hashed = self.key.strip_prefix("hash-").is_some_and(|rest| {
            rest.len() == 32 && rest.bytes().all(|b| b.is_ascii_hexdigit())
        });
        if self.launcher != Launcher::Manual || hashed {
            return false;
        }
        self.key = Self::key_for(&GameId::new(Launcher::Manual, self.key.as_str()));
        true
    }

    /// Whether `game` is this launcher entry at this build. A game whose
    /// build is unknown never matches.
    pub fn matches(&self, game: &Game) -> bool {
        self.launcher == game.id.launcher
            && self.key == Self::key_for(&game.id)
            && game.build.as_deref() == Some(self.build.as_str())
    }

    /// The game's id, which carries no build.
    pub fn id(&self) -> GameId {
        GameId::new(self.launcher, self.key.clone())
    }
}

/// Reads a string and folds it to lower case, so a hand-edited report with an
/// upper-case hash compares equal to the walk's own output.
fn lowercase<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    String::deserialize(deserializer).map(|text| text.to_ascii_lowercase())
}

/// Stable source identity produced by a verified full-corpus walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    /// Hex SHA-256 over every file in path order: path length, path bytes,
    /// file size, then content. Read back in lower case.
    #[serde(deserialize_with = "lowercase")]
    pub sha256: String,
    /// Regular files hashed.
    pub files: u64,
    /// Total content bytes hashed.
    pub bytes: u64,
}

/// Storage implementation exercised by a qualification run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageMode {
    Native,
    MaximumSpace,
}

impl std::fmt::Display for StorageMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Native => "Standard",
            Self::MaximumSpace => "Maximum",
        })
    }
}

/// Platform family without host names, mount paths, or device identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Linux,
    Windows,
    Macos,
    Other,
}

impl Platform {
    /// The platform this binary was compiled for.
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Other
        }
    }
}

/// Outcomes that must all hold before automatic Maximum Space activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checks {
    pub bytes_verified: bool,
    pub metadata_verified: bool,
    pub writable_update_verified: bool,
    pub rollback_verified: bool,
    pub launched: bool,
    /// A problem was observed. Both issue flags must be false to qualify.
    pub anti_cheat_issue: bool,
    pub gameplay_issue: bool,
    /// Load time of the ordinary install, in milliseconds.
    pub baseline_load_ms: u64,
    /// Load time under the tested storage mode, in milliseconds.
    pub candidate_load_ms: u64,
}

/// Measured storage result. Values are allocated bytes, not apparent sizes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageResult {
    /// Sum of file sizes. Must equal the corpus's `bytes`.
    pub logical_bytes: u64,
    /// Bytes allocated by the ordinary install.
    pub allocated_before: u64,
    /// Bytes allocated under the tested storage mode.
    pub allocated_after: u64,
    /// 95th-percentile random read latency in nanoseconds, when measured.
    pub random_read_p95_ns: Option<u64>,
}

/// A local or community qualification record. Its schema cannot contain paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub version: u32,
    pub game: GameBuild,
    pub corpus: Corpus,
    pub platform: Platform,
    pub mode: StorageMode,
    pub checks: Checks,
    pub storage: StorageResult,
    /// Version of the Flummox build that wrote the report.
    pub flummox_version: String,
}

/// Thresholds a valid report must meet before it qualifies a game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Maximum accepted load-time increase in basis points.
    pub maximum_load_regression_bps: u16,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            maximum_load_regression_bps: 1_000,
        }
    }
}

impl Report {
    /// BLAKE3 of the report's JSON. An estimate stores it to name the report
    /// that matched the analyzed game.
    pub fn identity(&self) -> Result<[u8; 32]> {
        Ok(*blake3::hash(&serde_json::to_vec(self)?).as_bytes())
    }

    /// Checks that the report is complete and self-consistent. Passing says
    /// nothing about whether the recorded results are good enough.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "Unsupported compatibility report version"
        );
        ensure!(
            !self.game.key.is_empty() && !self.game.build.is_empty(),
            "Compatibility report game identity is incomplete"
        );
        let path_free =
            |text: &str, limit: usize| text.len() <= limit && !text.contains(['/', '\\']);
        ensure!(
            path_free(&self.game.key, MAX_KEY_LEN)
                && path_free(&self.game.build, MAX_TEXT_LEN)
                && path_free(&self.flummox_version, MAX_TEXT_LEN),
            "Compatibility report identity holds a path separator or is too long"
        );
        ensure!(
            self.corpus.sha256.len() == 64
                && self
                    .corpus
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "Compatibility report corpus hash is invalid"
        );
        ensure!(
            self.corpus.files > 0 && self.corpus.bytes > 0,
            "Compatibility report corpus is empty"
        );
        ensure!(
            self.storage.logical_bytes == self.corpus.bytes,
            "Compatibility report corpus sizes disagree"
        );
        ensure!(
            self.checks.baseline_load_ms > 0 && self.checks.candidate_load_ms > 0,
            "Compatibility report load measurements are missing"
        );
        ensure!(
            !self.flummox_version.trim().is_empty(),
            "Compatibility report tool version is missing"
        );
        Ok(())
    }

    /// Whether this report permits automatic Maximum Space for `game` here:
    /// a valid Maximum Space report from this platform, for this build and
    /// corpus hash, with every check passed, no issue seen, and the load time
    /// within the policy's allowance over the baseline.
    pub fn qualifies(&self, game: &Game, corpus_sha256: &str, policy: Policy) -> bool {
        self.validate().is_ok()
            && self.mode == StorageMode::MaximumSpace
            && self.platform == Platform::current()
            && self.game.matches(game)
            && self.corpus.sha256.eq_ignore_ascii_case(corpus_sha256)
            && self.checks.bytes_verified
            && self.checks.metadata_verified
            && self.checks.writable_update_verified
            && self.checks.rollback_verified
            && self.checks.launched
            && !self.checks.anti_cheat_issue
            && !self.checks.gameplay_issue
            && u128::from(self.checks.candidate_load_ms) * 10_000
                <= u128::from(self.checks.baseline_load_ms)
                    * (10_000 + u128::from(policy.maximum_load_regression_bps))
    }

    /// The content-derived file name a valid report is stored under.
    fn filename(&self) -> Result<String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        Ok(format!("{}.json", blake3::hash(&bytes).to_hex()))
    }
}

/// Owner-local immutable compatibility reports.
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens the store at `root`, creating the folder owner-only.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        create_private_dir(&root)?;
        Ok(Self { root })
    }

    /// The store beside this user's state database.
    #[cfg(target_os = "linux")]
    pub fn local() -> Result<Self> {
        let database = crate::db::Db::default_path().context("Cannot locate state folder")?;
        let parent = database.parent().context("Invalid state folder")?;
        Self::open(parent.join("compatibility"))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn local() -> Result<Self> {
        Self::open(crate::libraries::data_dir()?.join("compatibility"))
    }

    /// Writes a valid report under its content-derived name and returns the
    /// path. A report already stored is left untouched.
    pub fn save(&self, report: &Report) -> Result<PathBuf> {
        let path = self.root.join(report.filename()?);
        if path.exists() {
            return Ok(path);
        }
        let bytes = serde_json::to_vec_pretty(report)?;
        write_private_new(&path, &bytes)?;
        Ok(path)
    }

    /// Every valid report in the store.
    ///
    /// A file that is oversized, unreadable or malformed is skipped. Failing
    /// here made one bad file stop every analysis job, which loads reports.
    pub fn load(&self) -> Result<Vec<Report>> {
        let mut reports = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            match Self::read(&path) {
                Ok((report, migrated)) => {
                    if migrated && let Err(error) = self.rewrite(&path, &report) {
                        tracing::warn!(path = %path.display(), %error, "could not rewrite a migrated compatibility report");
                    }
                    reports.push(report);
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "skipped a compatibility report");
                }
            }
        }
        Ok(reports)
    }

    /// Replaces an old report with its migrated form. The new file is staged
    /// beside the store, synced and renamed to its content name, and only then
    /// is the old file removed, so a crash leaves a complete report either way.
    fn rewrite(&self, old: &Path, report: &Report) -> Result<()> {
        use std::io::Write;
        let target = self.root.join(report.filename()?);
        let mut staged = tempfile::NamedTempFile::new_in(&self.root)?;
        staged.write_all(&serde_json::to_vec_pretty(report)?)?;
        staged.as_file().sync_all()?;
        staged.persist(&target)?;
        #[cfg(unix)]
        std::fs::File::open(&self.root)?.sync_all()?;
        if target != old {
            std::fs::remove_file(old)?;
        }
        Ok(())
    }

    /// Reads and validates one report file of at most 1 MiB. The flag says the
    /// key of a custom-folder report was rewritten to its hashed form, which
    /// reports saved before the key was hashed need. An imported report is
    /// never migrated.
    fn read(path: &Path) -> Result<(Report, bool)> {
        ensure!(
            std::fs::metadata(path)?.len() <= 1024 * 1024,
            "Compatibility report exceeds 1 MiB"
        );
        let bytes = std::fs::read(path).context("Reading the report")?;
        let mut report: Report = serde_json::from_slice(&bytes).context("Parsing the report")?;
        let migrated = report.game.hash_manual_key();
        report.validate()?;
        Ok((report, migrated))
    }
}

/// Creates `path` and its parents, and sets `path` to mode 0700.
#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

/// Creates `path` with mode 0600, writes it and syncs it. Fails if it exists.
#[cfg(unix)]
fn write_private_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Hashes every regular file using the benchmark's path, size and payload records.
#[cfg(target_os = "linux")]
pub fn corpus(
    root: &Path,
    cancel: &std::sync::atomic::AtomicBool,
    observer: &dyn crate::pack::Observer,
) -> Result<Corpus> {
    use sha2::{Digest, Sha256};
    use std::{io::Read, os::unix::ffi::OsStrExt, sync::atomic::Ordering};
    let root = crate::jobs::validate_folder(root)?;
    let anchor = crate::safeio::Anchor::open(&root)?;
    ensure!(
        anchor.fully_resolved(),
        "Safe path resolution is unavailable"
    );
    let mut inventory = crate::inventory::walk_cancellable(
        &root,
        &crate::inventory::WalkOpts { min_size: 0 },
        Some(cancel),
    )?;
    ensure!(
        inventory.warnings.is_empty(),
        "Cannot verify every file in this game"
    );
    // Hash in path order, so the result does not depend on walk order.
    inventory.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    for (number, entry) in inventory.files.iter().enumerate() {
        observer.checkpoint()?;
        ensure!(
            !cancel.load(Ordering::Relaxed),
            "Compatibility verification stopped"
        );
        let mut file = anchor.open_file(&entry.rel)?;
        ensure!(
            entry.matches_file(&file)?,
            "File changed before compatibility verification"
        );
        let path = entry.rel.as_os_str().as_bytes();
        hash.update(u64::try_from(path.len())?.to_le_bytes());
        hash.update(path);
        hash.update(entry.size.to_le_bytes());
        loop {
            observer.checkpoint()?;
            ensure!(
                !cancel.load(Ordering::Relaxed),
                "Compatibility verification stopped"
            );
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(buffer.get(..count).context("Invalid read length")?);
            bytes = bytes.saturating_add(count as u64);
            observer.progress(
                number as u64,
                bytes,
                "Checking compatibility against installed files",
            );
        }
        ensure!(
            entry.matches_file(&file)?,
            "File changed during compatibility verification"
        );
    }
    // Walk again and compare every fingerprint. A file added, removed or
    // changed while hashing ran means the hash describes no real state.
    let after = crate::inventory::walk_cancellable(
        &root,
        &crate::inventory::WalkOpts { min_size: 0 },
        Some(cancel),
    )?;
    ensure!(
        after.warnings.is_empty() && after.files.len() == inventory.files.len(),
        "Game changed during compatibility verification"
    );
    for entry in &inventory.files {
        ensure!(
            entry.matches_file(&anchor.open_file(&entry.rel)?)?,
            "Game changed during compatibility verification"
        );
    }
    ensure!(
        bytes == inventory.total_bytes(),
        "Game size changed during compatibility verification"
    );
    Ok(Corpus {
        sha256: hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        files: inventory.files.len() as u64,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{InstallState, Launcher},
        testutil::{Ctx, TestResult, check, check_eq},
    };

    fn game() -> Game {
        Game {
            id: GameId::new(Launcher::Steam, "42"),
            also: vec![],
            title: "Private title".into(),
            install_dir: "/home/person/Games/private".into(),
            build: Some("7".into()),
            size_hint: Some(4096),
            state: InstallState::Idle,
            is_tool: false,
        }
    }

    fn report() -> Report {
        Report {
            version: VERSION,
            game: GameBuild {
                launcher: Launcher::Steam,
                key: "42".into(),
                build: "7".into(),
            },
            corpus: Corpus {
                sha256: "a".repeat(64),
                files: 2,
                bytes: 4096,
            },
            platform: Platform::current(),
            mode: StorageMode::MaximumSpace,
            checks: Checks {
                bytes_verified: true,
                metadata_verified: true,
                writable_update_verified: true,
                rollback_verified: true,
                launched: true,
                anti_cheat_issue: false,
                gameplay_issue: false,
                baseline_load_ms: 1000,
                candidate_load_ms: 1100,
            },
            storage: StorageResult {
                logical_bytes: 4096,
                allocated_before: 4096,
                allocated_after: 2048,
                random_read_p95_ns: Some(50_000),
            },
            flummox_version: "0.1.0".into(),
        }
    }

    #[test]
    fn qualification_is_exact_to_build_corpus_and_threshold() -> TestResult {
        let mut installed = game();
        let report = report();
        check(
            report.qualifies(&installed, &"a".repeat(64), Policy::default()),
            "the verified matching report qualifies",
        )?;
        let mut wrong_platform = report.clone();
        wrong_platform.platform = match Platform::current() {
            Platform::Linux => Platform::Windows,
            Platform::Windows => Platform::Linux,
            Platform::Other | Platform::Macos => Platform::Linux,
        };
        check(
            !wrong_platform.qualifies(&installed, &"a".repeat(64), Policy::default()),
            "evidence from another platform cannot unlock this backend",
        )?;
        installed.build = Some("8".into());
        check(
            !report.qualifies(&installed, &"a".repeat(64), Policy::default()),
            "a changed build is not approved",
        )?;
        check(
            !report.qualifies(&game(), &"b".repeat(64), Policy::default()),
            "a changed corpus is not approved",
        )?;
        let mut slow = report;
        slow.checks.candidate_load_ms = 1101;
        check(
            !slow.qualifies(&game(), &"a".repeat(64), Policy::default()),
            "a load regression above ten percent is refused",
        )
    }

    fn manual_game() -> Game {
        Game {
            id: GameId::new(Launcher::Manual, "/home/person/Games/private"),
            also: vec![],
            title: "Private title".into(),
            install_dir: "/home/person/Games/private".into(),
            build: Some("7".into()),
            size_hint: Some(4096),
            state: InstallState::Idle,
            is_tool: false,
        }
    }

    /// A report for `game` made by the form's own builder.
    fn built_report(game: &Game) -> Result<Report, anyhow::Error> {
        let mut wizard = crate::qualification::Wizard::new(
            game.clone(),
            Corpus {
                sha256: "a".repeat(64),
                files: 2,
                bytes: 4096,
            },
        );
        wizard.mode = StorageMode::MaximumSpace;
        wizard.baseline_load = "1000".into();
        wizard.candidate_load = "1050".into();
        wizard.allocated_before = "4096".into();
        wizard.allocated_after = "2048".into();
        for check in [
            crate::qualification::Check::Bytes,
            crate::qualification::Check::Metadata,
            crate::qualification::Check::Update,
            crate::qualification::Check::Restore,
            crate::qualification::Check::Launch,
        ] {
            wizard.check(check, true);
        }
        wizard.report()
    }

    #[test]
    fn stored_reports_have_no_titles_or_paths() -> TestResult {
        let game = manual_game();
        let built = built_report(&game).ctx("build a report for a manual game")?;
        let dir = tempfile::tempdir().ctx("temporary report folder")?;
        let store = Store::open(dir.path().join("compatibility")).ctx("open report store")?;
        let path = store.save(&built).ctx("save report")?;
        let json = std::fs::read_to_string(path).ctx("read report")?;
        check(!json.contains("Private title"), "title is absent")?;
        check(!json.contains("/home/person"), "install path is absent")?;
        check(!json.contains("person"), "user name is absent")?;
        check(
            built.qualifies(&game, &"a".repeat(64), Policy::default()),
            "the hashed key still matches the game it came from",
        )?;
        let mut other = manual_game();
        other.id = GameId::new(Launcher::Manual, "/home/person/Games/other");
        check(
            !built.qualifies(&other, &"a".repeat(64), Policy::default()),
            "a different folder does not match",
        )?;
        let loaded = store.load().ctx("load reports")?;
        check_eq(loaded, vec![built.clone()], "stored report round trip")?;
        std::fs::write(
            dir.path().join("compatibility/broken.json"),
            b"{ not a report",
        )
        .ctx("malformed file")?;
        let loaded = store.load().ctx("load beside a malformed file")?;
        check_eq(loaded, vec![built], "a malformed file hides nothing")
    }

    #[test]
    fn a_report_that_carries_a_path_is_rejected_on_import_and_migrated_on_load() -> TestResult {
        let mut old = report();
        old.game.launcher = Launcher::Manual;
        old.game.key = "/home/person/Games/private".into();
        check(old.validate().is_err(), "a path key fails validation")?;
        let dir = tempfile::tempdir().ctx("temporary report folder")?;
        let root = dir.path().join("compatibility");
        let store = Store::open(&root).ctx("open report store")?;
        std::fs::write(
            root.join("old.json"),
            serde_json::to_vec(&old).ctx("serialise an old report")?,
        )
        .ctx("plant an old report")?;
        let loaded = store.load().ctx("load an old report")?;
        let migrated = loaded.first().ctx("the migrated report")?;
        check(
            migrated.qualifies(&manual_game(), &"a".repeat(64), Policy::default()),
            "the migrated report matches the game it was made for",
        )?;
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&root).ctx("list the store")? {
            let path = entry.ctx("entry")?.path();
            let text = std::fs::read_to_string(&path).ctx("read a stored file")?;
            check(
                !text.contains("/home/person"),
                "no stored file keeps the path",
            )?;
            files.push((path, text));
        }
        check_eq(files.len(), 1, "one file remains and no temp file is left")?;
        let again = store.load().ctx("load again")?;
        check_eq(again, loaded, "a second load returns the same report")?;
        let mut after = Vec::new();
        for entry in std::fs::read_dir(&root).ctx("list again")? {
            let path = entry.ctx("entry")?.path();
            let text = std::fs::read_to_string(&path).ctx("read again")?;
            after.push((path, text));
        }
        check_eq(after, files, "a second load writes nothing")?;
        let mut long = report();
        long.game.build = "7".repeat(MAX_TEXT_LEN + 1);
        check(long.validate().is_err(), "an over-long build is rejected")?;
        let mut windows = report();
        windows.game.build = "C:\\Games".into();
        check(windows.validate().is_err(), "a backslash is rejected")?;
        check(
            report().validate().is_ok(),
            "control: the plain report is valid",
        )
    }

    #[test]
    fn the_load_check_cannot_be_passed_by_overflowing_values() -> TestResult {
        let mut huge = report();
        huge.checks.baseline_load_ms = 2_000_000_000_000_000;
        huge.checks.candidate_load_ms = u64::MAX;
        check(
            !huge.qualifies(&game(), &"a".repeat(64), Policy::default()),
            "a huge baseline does not excuse a huge candidate",
        )?;
        let mut slow = report();
        slow.checks.candidate_load_ms = 1100;
        check(
            slow.qualifies(&game(), &"a".repeat(64), Policy::default()),
            "control: exactly ten percent slower still qualifies",
        )
    }

    #[test]
    fn upper_case_corpus_hashes_are_read_as_lower_case() -> TestResult {
        let mut upper = report();
        upper.corpus.sha256 = "A".repeat(64);
        let json = serde_json::to_string(&upper).ctx("serialise")?;
        let read: Report = serde_json::from_str(&json).ctx("parse")?;
        check_eq(read.corpus.sha256, "a".repeat(64), "folded on read")
    }
}

#[cfg(all(test, target_os = "linux"))]
mod corpus_tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq, check_ne};
    use std::sync::atomic::AtomicBool;
    #[test]
    fn corpus_uses_benchmark_records_and_detects_renames() -> TestResult {
        let temp = tempfile::tempdir().ctx("corpus fixture")?;
        std::fs::create_dir(temp.path().join("dir")).ctx("fixture directory")?;
        std::fs::write(temp.path().join("dir/file.bin"), b"abc").ctx("fixture bytes")?;
        let initial = corpus(
            temp.path(),
            &AtomicBool::new(false),
            &crate::pack::NoObserver,
        )
        .ctx("corpus")?;
        check_eq(
            initial.sha256.clone(),
            "02a59e4570844e8ab5f39860a50329a604937c943075d0bf736c7665df37b4b3".to_owned(),
            "same records as Windows comparison harness",
        )?;
        check_eq(initial.files, 1, "file count")?;
        std::fs::rename(
            temp.path().join("dir/file.bin"),
            temp.path().join("dir/renamed.bin"),
        )
        .ctx("rename")?;
        let changed = corpus(
            temp.path(),
            &AtomicBool::new(false),
            &crate::pack::NoObserver,
        )
        .ctx("renamed corpus")?;
        check_ne(
            initial.sha256,
            changed.sha256,
            "names are part of qualification",
        )?;
        check(
            corpus(
                temp.path(),
                &AtomicBool::new(true),
                &crate::pack::NoObserver,
            )
            .is_err(),
            "cancelled scan cannot qualify",
        )
    }
    #[test]
    fn writes_during_corpus_verification_are_rejected() -> TestResult {
        struct Mutate(PathBuf);
        impl crate::pack::Observer for Mutate {
            fn progress(&self, _files: u64, _bytes: u64, _stage: &str) {
                let _written = std::fs::write(&self.0, b"changed");
            }
        }
        let temp = tempfile::tempdir().ctx("changing corpus")?;
        let path = temp.path().join("asset");
        std::fs::write(&path, b"original").ctx("asset")?;
        check(
            corpus(temp.path(), &AtomicBool::new(false), &Mutate(path)).is_err(),
            "a concurrent write invalidates qualification",
        )
    }
}
