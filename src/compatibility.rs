//! Versioned, path-free evidence for enabling game-specific storage modes.

use crate::model::{Game, GameId, Launcher};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 1;

/// A game build without its title or installation path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameBuild {
    pub launcher: Launcher,
    pub key: String,
    pub build: String,
}

impl GameBuild {
    pub fn matches(&self, game: &Game) -> bool {
        self.launcher == game.id.launcher
            && self.key == game.id.key
            && game.build.as_deref() == Some(self.build.as_str())
    }

    pub fn id(&self) -> GameId {
        GameId::new(self.launcher, self.key.clone())
    }
}

/// Stable source identity produced by a verified full-corpus walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    pub sha256: String,
    pub files: u64,
    pub bytes: u64,
}

/// Storage implementation exercised by a qualification run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageMode {
    Native,
    MaximumSpace,
}

/// Platform family without host names, mount paths, or device identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Linux,
    Windows,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(windows) {
            Self::Windows
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
    pub anti_cheat_issue: bool,
    pub gameplay_issue: bool,
    pub baseline_load_ms: u64,
    pub candidate_load_ms: u64,
}

/// Measured storage result. Values are allocated bytes, not apparent sizes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageResult {
    pub logical_bytes: u64,
    pub allocated_before: u64,
    pub allocated_after: u64,
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
    pub flummox_version: String,
}

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
    pub fn identity(&self) -> Result<[u8; 32]> {
        Ok(*blake3::hash(&serde_json::to_vec(self)?).as_bytes())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "Unsupported compatibility report version"
        );
        ensure!(
            !self.game.key.is_empty() && !self.game.build.is_empty(),
            "Compatibility report game identity is incomplete"
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
            && self.checks.candidate_load_ms.saturating_mul(10_000)
                <= self.checks.baseline_load_ms.saturating_mul(
                    10_000u64.saturating_add(u64::from(policy.maximum_load_regression_bps)),
                )
    }

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
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        create_private_dir(&root)?;
        Ok(Self { root })
    }

    #[cfg(target_os = "linux")]
    pub fn local() -> Result<Self> {
        let database = crate::db::Db::default_path().context("Cannot locate state folder")?;
        let parent = database.parent().context("Invalid state folder")?;
        Self::open(parent.join("compatibility"))
    }

    pub fn save(&self, report: &Report) -> Result<PathBuf> {
        let path = self.root.join(report.filename()?);
        if path.exists() {
            return Ok(path);
        }
        let bytes = serde_json::to_vec_pretty(report)?;
        write_private_new(&path, &bytes)?;
        Ok(path)
    }

    pub fn load(&self) -> Result<Vec<Report>> {
        let mut reports = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            ensure!(
                std::fs::metadata(&path)?.len() <= 1024 * 1024,
                "Compatibility report exceeds 1 MiB"
            );
            let bytes =
                std::fs::read(&path).with_context(|| format!("Reading {}", path.display()))?;
            let report: Report = serde_json::from_slice(&bytes)
                .with_context(|| format!("Parsing {}", path.display()))?;
            report.validate()?;
            reports.push(report);
        }
        Ok(reports)
    }
}

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
            Platform::Other => Platform::Linux,
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

    #[test]
    fn stored_reports_have_no_titles_or_paths() -> TestResult {
        let dir = tempfile::tempdir().ctx("temporary report folder")?;
        let store = Store::open(dir.path().join("compatibility")).ctx("open report store")?;
        let path = store.save(&report()).ctx("save report")?;
        let json = std::fs::read_to_string(path).ctx("read report")?;
        check(!json.contains("Private title"), "title is absent")?;
        check(!json.contains("/home/person"), "install path is absent")?;
        let loaded = store.load().ctx("load reports")?;
        check_eq(loaded, vec![report()], "stored report round trip")
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
