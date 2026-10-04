//! Guided, locally saved evidence for game compatibility qualification.
use crate::{
    compatibility::{
        self, Checks, Corpus, GameBuild, Platform, Report, StorageMode, StorageResult,
    },
    model::Game,
};
use anyhow::{Context, Result, ensure};

#[derive(Debug, Clone, Copy)]
pub enum Field {
    Build,
    BaselineLoad,
    CandidateLoad,
    AllocationBefore,
    AllocationAfter,
}
#[derive(Debug, Clone, Copy)]
pub enum Check {
    Bytes,
    Metadata,
    Update,
    Restore,
    Launch,
    AntiCheat,
    Gameplay,
}

#[derive(Debug, Clone)]
pub struct Wizard {
    pub game: Game,
    pub corpus: Corpus,
    pub mode: StorageMode,
    pub build: String,
    pub baseline_load: String,
    pub candidate_load: String,
    pub allocated_before: String,
    pub allocated_after: String,
    pub checks: Checks,
}
impl Wizard {
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
    pub fn field(&mut self, field: Field, text: String) {
        match field {
            Field::Build => self.build = text,
            Field::BaselineLoad => self.baseline_load = text,
            Field::CandidateLoad => self.candidate_load = text,
            Field::AllocationBefore => self.allocated_before = text,
            Field::AllocationAfter => self.allocated_after = text,
        }
    }
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
            game: GameBuild {
                launcher: self.game.id.launcher,
                key: self.game.id.key.clone(),
                build: self.build.trim().into(),
            },
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

pub fn baseline(game: &Game) -> Result<Corpus> {
    #[cfg(target_os = "linux")]
    return compatibility::corpus(
        &game.install_dir,
        &std::sync::atomic::AtomicBool::new(false),
        &crate::pack::NoObserver,
    );
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
        files.sort();
        let mut hash = Sha256::new();
        let mut total = 0u64;
        for path in &files {
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

#[cfg(feature = "gui")]
pub fn view<'a, Message: Clone + 'a>(
    wizard: &'a Wizard,
    field: impl Fn(Field, String) -> Message + Clone + 'a,
    check: impl Fn(Check, bool) -> Message + Clone + 'a,
    mode: impl Fn(StorageMode) -> Message + 'a,
    save: Message,
    cancel: Message,
) -> iced::Element<'a, Message> {
    use iced::widget::{button, checkbox, column, pick_list, row, text, text_input};
    let mut form = column![text(format!("Qualify {}", wizard.game.title)).size(20), text("Use a disposable game copy. Measure the ordinary install, then compression, launch, gameplay, update, verification, restart, and restoration.").size(13), text(format!("Baseline: {} files · {} logical bytes", wizard.corpus.files, wizard.corpus.bytes)).size(12), pick_list([StorageMode::Native, StorageMode::MaximumSpace], Some(wizard.mode), mode)].spacing(8);
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
        form = form.push(
            text_input(label, value)
                .on_input(move |value| field(kind, value))
                .padding(8),
        );
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
    form = form.push(text("Record actual allocated bytes; whole-drive free-space changes include other processes. Reports retain failed checks and do not claim untested games are compatible.").size(12));
    if let Err(error) = wizard.report() {
        form = form.push(text(error.to_string()).size(12));
    }
    form.push(
        row![
            button("Save local report").on_press_maybe(wizard.report().is_ok().then_some(save)),
            button("Close").on_press(cancel)
        ]
        .spacing(8),
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
}
