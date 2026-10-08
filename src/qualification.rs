//! Guided, locally saved evidence for game compatibility qualification.
use crate::{
    compatibility::{
        self, Checks, Corpus, GameBuild, Platform, Report, StorageMode, StorageResult,
    },
    model::Game,
};
use anyhow::{Context, Result, ensure};

pub use crate::observer::{NoObserver, Observer};

/// A text input of the wizard form.
#[derive(Debug, Clone, Copy)]
pub enum Field {
    Build,
    BaselineLoad,
    CandidateLoad,
    AllocationBefore,
    AllocationAfter,
}
/// A checkbox of the wizard form. Each maps to one flag in [`Checks`].
#[derive(Debug, Clone, Copy)]
pub enum Check {
    Bytes,
    Metadata,
    /// The launcher updated and verified the game under the tested mode.
    Update,
    /// Restoring ordinary files and restarting worked.
    Restore,
    Launch,
    /// Ticked when a problem was seen. A ticked issue box disqualifies.
    AntiCheat,
    Gameplay,
}

/// Form state for one qualification run. Numeric inputs stay as typed text
/// until [`Wizard::report`] parses them.
#[derive(Debug, Clone)]
pub struct Wizard {
    pub game: Game,
    /// Hash of the game's files taken when the wizard started.
    pub corpus: Corpus,
    pub mode: StorageMode,
    pub build: String,
    /// Load times in milliseconds and allocations in bytes, as typed.
    pub baseline_load: String,
    pub candidate_load: String,
    pub allocated_before: String,
    pub allocated_after: String,
    /// What the last allocated-byte measurement covered, or why it failed.
    pub measurement: Option<String>,
    pub checks: Checks,
}
impl Wizard {
    /// An empty form for `game`, with the build taken from the game record.
    pub fn new(game: Game, corpus: Corpus) -> Self {
        Self {
            build: game.build.clone().unwrap_or_default(),
            game,
            corpus,
            mode: StorageMode::Native,
            baseline_load: String::new(),
            candidate_load: String::new(),
            allocated_before: String::new(),
            allocated_after: String::new(),
            measurement: None,
            checks: Checks {
                bytes_verified: false,
                metadata_verified: false,
                writable_update_verified: false,
                rollback_verified: false,
                launched: false,
                anti_cheat_issue: false,
                gameplay_issue: false,
                baseline_load_ms: 0,
                candidate_load_ms: 0,
            },
        }
    }
    /// Stores the text typed into one input.
    pub fn field(&mut self, field: Field, text: String) {
        match field {
            Field::Build => self.build = text,
            Field::BaselineLoad => self.baseline_load = text,
            Field::CandidateLoad => self.candidate_load = text,
            Field::AllocationBefore => self.allocated_before = text,
            Field::AllocationAfter => self.allocated_after = text,
        }
    }
    /// Hashes the game and measures its folder as the uncompressed baseline.
    pub fn start(game: Game) -> Result<Self> {
        Self::start_cancellable(
            game,
            &std::sync::atomic::AtomicBool::new(false),
            &NoObserver,
        )
    }

    /// [`Wizard::start`] that stops with an error once `cancel` is set, and
    /// reports hashing progress to `observer`.
    pub fn start_cancellable(
        game: Game,
        cancel: &std::sync::atomic::AtomicBool,
        observer: &dyn Observer,
    ) -> Result<Self> {
        let corpus = baseline_cancellable(&game, cancel, observer)?;
        let before = crate::allocation::measure(std::slice::from_ref(&game.install_dir));
        let mut wizard = Self::new(game, corpus);
        match before {
            Ok(found) => {
                wizard.allocated_before = found.allocated_bytes.to_string();
                wizard.measurement = Some(format!(
                    "Measured the game folder as the original: {} files, {} allocated bytes.",
                    found.files, found.allocated_bytes
                ));
            }
            Err(error) => {
                wizard.measurement = Some(format!("Original not measured: {error:#}."));
            }
        }
        Ok(wizard)
    }
    /// Records a measurement of the compressed copy, or why there is none.
    pub fn measured(&mut self, result: Result<crate::allocation::Allocation, String>) {
        self.measurement = Some(match result {
            Ok(found) => {
                self.allocated_after = found.allocated_bytes.to_string();
                format!(
                    "Measured the compressed copy: {} files, {} allocated bytes.",
                    found.files, found.allocated_bytes
                )
            }
            Err(error) => format!("Compressed copy not measured: {error}."),
        });
    }
    /// Sets one checkbox.
    pub fn check(&mut self, check: Check, value: bool) {
        match check {
            Check::Bytes => self.checks.bytes_verified = value,
            Check::Metadata => self.checks.metadata_verified = value,
            Check::Update => self.checks.writable_update_verified = value,
            Check::Restore => self.checks.rollback_verified = value,
            Check::Launch => self.checks.launched = value,
            Check::AntiCheat => self.checks.anti_cheat_issue = value,
            Check::Gameplay => self.checks.gameplay_issue = value,
        }
    }
    /// Builds the report the form describes. Fails when a number is missing
    /// or not a whole number, an allocation is zero, or validation fails.
    pub fn report(&self) -> Result<Report> {
        let integer = |text: &str, label: &str| -> Result<u64> {
            text.trim()
                .parse::<u64>()
                .with_context(|| format!("Enter {label} as whole-number measurements"))
        };
        let mut checks = self.checks.clone();
        checks.baseline_load_ms =
            integer(&self.baseline_load, "baseline load time in milliseconds")?;
        checks.candidate_load_ms =
            integer(&self.candidate_load, "compressed load time in milliseconds")?;
        let report = Report {
            version: compatibility::VERSION,
            game: GameBuild::new(&self.game.id, &self.build),
            corpus: self.corpus.clone(),
            platform: Platform::current(),
            mode: self.mode,
            checks,
            storage: StorageResult {
                logical_bytes: self.corpus.bytes,
                allocated_before: integer(&self.allocated_before, "original allocated bytes")?,
                allocated_after: integer(&self.allocated_after, "compressed allocated bytes")?,
                random_read_p95_ns: None,
            },
            flummox_version: env!("CARGO_PKG_VERSION").into(),
        };
        ensure!(
            report.storage.allocated_before > 0 && report.storage.allocated_after > 0,
            "Allocated-byte measurements must be positive"
        );
        report.validate()?;
        Ok(report)
    }
}

/// Hashes every file of the game into a [`Corpus`]. Linux uses
/// `compatibility::corpus`. Other platforms walk the folder here and fail
/// if a file changes size or modification time while it is read.
pub fn baseline(game: &Game) -> Result<Corpus> {
    baseline_cancellable(
        game,
        &std::sync::atomic::AtomicBool::new(false),
        &NoObserver,
    )
}

/// [`baseline`] that stops with an error once `cancel` is set, and reports
/// progress to `observer`.
pub fn baseline_cancellable(
    game: &Game,
    cancel: &std::sync::atomic::AtomicBool,
    observer: &dyn Observer,
) -> Result<Corpus> {
    #[cfg(target_os = "linux")]
    return compatibility::corpus(&game.install_dir, cancel, observer);
    #[cfg(not(target_os = "linux"))]
    {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let root = game.install_dir.canonicalize()?;
        let mut files = vec![];
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_file() {
                files.push(entry.into_path());
            }
        }
        // Per file, in sorted path order: path length, path, size, content.
        files.sort();
        let mut hash = Sha256::new();
        let mut total = 0u64;
        for path in &files {
            observer.checkpoint()?;
            ensure!(
                !cancel.load(std::sync::atomic::Ordering::Relaxed),
                "Compatibility verification stopped"
            );
            ensure!(
                path.canonicalize()?.starts_with(&root),
                "File escaped the selected game"
            );
            let metadata = std::fs::symlink_metadata(path)?;
            ensure!(metadata.is_file(), "Game changed during qualification");
            let relative = path.strip_prefix(&root)?.as_os_str().as_encoded_bytes();
            hash.update((relative.len() as u64).to_le_bytes());
            hash.update(relative);
            hash.update(metadata.len().to_le_bytes());
            let mut file = std::fs::File::open(path)?;
            let mut buffer = vec![0u8; 1024 * 1024];
            loop {
                let length = file.read(&mut buffer)?;
                if length == 0 {
                    break;
                }
                ensure!(
                    !cancel.load(std::sync::atomic::Ordering::Relaxed),
                    "Compatibility verification stopped"
                );
                hash.update(buffer.get(..length).context("Invalid read length")?);
            }
            let after = file.metadata()?;
            ensure!(
                metadata.len() == after.len() && metadata.modified()? == after.modified()?,
                "Game changed during qualification"
            );
            total = total
                .checked_add(metadata.len())
                .context("Corpus size overflow")?;
        }
        Ok(Corpus {
            sha256: hash
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            files: files.len() as u64,
            bytes: total,
        })
    }
}

/// Renders the wizard form. `field`, `check` and `mode` turn an edit into the
/// caller's message. The save button is enabled only while
/// [`Wizard::report`] succeeds, and its error is shown otherwise.
#[cfg(all(
    feature = "gui",
    any(target_os = "linux", target_os = "macos", windows)
))]
pub fn view<'a, Message: Clone + 'a>(
    wizard: &'a Wizard,
    field: impl Fn(Field, String) -> Message + Clone + 'a,
    check: impl Fn(Check, bool) -> Message + Clone + 'a,
    mode: impl Fn(StorageMode) -> Message + 'a,
    measure: Message,
    save: Message,
    cancel: Message,
) -> iced::Element<'a, Message> {
    use crate::gui::theme;
    use iced::widget::{checkbox, column, pick_list, row, text_input};
    let mut form = column![
        theme::section_text(format!("Qualify {}", wizard.game.title)),
        theme::muted("Use a disposable game copy. Measure the ordinary install, then compression, launch, gameplay, update, verification, restart, and restoration."),
        theme::muted(format!(
            "Baseline: {} files · {}",
            wizard.corpus.files,
            humansize::format_size(wizard.corpus.bytes, humansize::DECIMAL)
        )),
        theme::field(
            "Storage mode",
            pick_list([StorageMode::Native, StorageMode::MaximumSpace], Some(wizard.mode), mode)
                .padding(theme::INPUT_PADDING)
                .style(theme::pick_list)
                .menu_style(theme::pick_menu)
        )
    ]
    .spacing(theme::PAGE_GAP / 2.0);
    // One text input per `Field`, then one checkbox per `Check`.
    for (label, value, kind) in [
        ("Game build", &wizard.build, Field::Build),
        (
            "Baseline load time (ms)",
            &wizard.baseline_load,
            Field::BaselineLoad,
        ),
        (
            "Compressed load time (ms)",
            &wizard.candidate_load,
            Field::CandidateLoad,
        ),
        (
            "Original allocated bytes",
            &wizard.allocated_before,
            Field::AllocationBefore,
        ),
        (
            "Compressed allocated bytes",
            &wizard.allocated_after,
            Field::AllocationAfter,
        ),
    ] {
        let field = field.clone();
        form = form.push(theme::field(
            label,
            text_input(label, value)
                .on_input(move |value| field(kind, value))
                .padding(theme::INPUT_PADDING)
                .style(theme::text_input),
        ));
    }
    for (label, value, kind) in [
        (
            "Ordinary reads preserve all file bytes",
            wizard.checks.bytes_verified,
            Check::Bytes,
        ),
        (
            "Permissions, metadata, and executable signatures preserved",
            wizard.checks.metadata_verified,
            Check::Metadata,
        ),
        (
            "Launcher update and verification succeed",
            wizard.checks.writable_update_verified,
            Check::Update,
        ),
        (
            "Restoration and restart succeed",
            wizard.checks.rollback_verified,
            Check::Restore,
        ),
        ("Launch succeeds", wizard.checks.launched, Check::Launch),
        (
            "Anti-cheat issue observed",
            wizard.checks.anti_cheat_issue,
            Check::AntiCheat,
        ),
        (
            "Gameplay issue observed",
            wizard.checks.gameplay_issue,
            Check::Gameplay,
        ),
    ] {
        let check = check.clone();
        form = form.push(
            checkbox(value)
                .label(label)
                .on_toggle(move |value| check(kind, value)),
        );
    }
    form = form.push(theme::secondary("Measure compressed copy", measure));
    if let Some(measurement) = &wizard.measurement {
        form = form.push(theme::muted(measurement));
    }
    form = form.push(theme::muted("Record actual allocated bytes; whole-drive free-space changes include other processes. Reports retain failed checks and do not claim untested games are compatible."));
    if let Err(error) = wizard.report() {
        form = form.push(theme::danger_text(error.to_string()));
    }
    form.push(
        row![
            theme::action_maybe("Save local report", wizard.report().is_ok().then_some(save)),
            theme::secondary("Close", cancel)
        ]
        .spacing(8)
        .wrap(),
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{GameId, InstallState, Launcher},
        testutil::{Ctx, TestResult, check, check_eq},
    };
    #[test]
    fn incomplete_measurements_cannot_be_saved_and_failed_checks_stay_failed() -> TestResult {
        let game = Game {
            id: GameId::new(Launcher::Steam, "fixture"),
            also: vec![],
            title: "Fixture".into(),
            install_dir: "/fixture".into(),
            build: Some("1".into()),
            size_hint: None,
            state: InstallState::Idle,
            is_tool: false,
        };
        let mut wizard = Wizard::new(
            game.clone(),
            Corpus {
                sha256: "a".repeat(64),
                files: 1,
                bytes: 4096,
            },
        );
        check(wizard.report().is_err(), "missing measurements must fail")?;
        wizard.measured(Err("refused".into()));
        check(
            wizard.allocated_after.is_empty(),
            "a failed measurement fills nothing in",
        )?;
        wizard.measured(Ok(crate::allocation::Allocation {
            files: 1,
            logical_bytes: 4096,
            allocated_bytes: 1024,
        }));
        check_eq(wizard.allocated_after.as_str(), "1024", "measured value")?;
        wizard.field(Field::BaselineLoad, "1000".into());
        wizard.field(Field::CandidateLoad, "1500".into());
        wizard.field(Field::AllocationBefore, "4096".into());
        wizard.field(Field::AllocationAfter, "2048".into());
        wizard.mode = StorageMode::MaximumSpace;
        wizard.check(Check::Gameplay, true);
        let report = wizard.report().ctx("measured report")?;
        check_eq(report.checks.baseline_load_ms, 1000, "baseline preserved")?;
        check(
            report.checks.gameplay_issue && !report.checks.launched,
            "failures are not replaced by optimistic defaults",
        )?;
        check(
            !report.qualifies(&game, &"a".repeat(64), compatibility::Policy::default()),
            "incomplete or regressed evidence must not qualify",
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_cancelled_wizard_start_stops_before_it_finishes() -> TestResult {
        let dir = tempfile::tempdir().ctx("install folder")?;
        std::fs::write(dir.path().join("data.bin"), vec![7u8; 4096]).ctx("write data.bin")?;
        let game = Game {
            id: GameId::new(Launcher::Manual, "fixture"),
            also: vec![],
            title: "Fixture".into(),
            install_dir: dir.path().to_path_buf(),
            build: Some("1".into()),
            size_hint: None,
            state: InstallState::Idle,
            is_tool: false,
        };
        let running = std::sync::atomic::AtomicBool::new(false);
        let wizard = Wizard::start_cancellable(game.clone(), &running, &NoObserver)
            .ctx("control: an uncancelled start hashes the folder")?;
        check_eq(wizard.corpus.files, 1, "control: the file was hashed")?;
        let stopped = std::sync::atomic::AtomicBool::new(true);
        check(
            Wizard::start_cancellable(game, &stopped, &NoObserver).is_err(),
            "a start cancelled up front returns an error",
        )
    }
}
