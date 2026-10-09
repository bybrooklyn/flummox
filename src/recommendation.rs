//! Turns measured estimates into one storage decision.

use crate::estimate::Estimate;
use serde::{Deserialize, Serialize};

/// The storage action shown to users and consumed by automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageMode {
    /// The predicted return does not justify touching the install.
    Skip,
    /// Use the operating system or filesystem compression backend.
    Native,
    /// Use Flummox's verified, writable pack store.
    MaximumSpace,
}

impl StorageMode {
    /// Short product-facing name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Skip => "No change recommended",
            Self::Native => "Standard",
            Self::MaximumSpace => "Maximum",
        }
    }
}

/// How much source evidence supports a prediction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    /// User-facing confidence name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Early estimate",
            Self::Medium => "Good estimate",
            Self::High => "Strong estimate",
        }
    }
}

/// Defaults used by the one-click optimizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Smallest predicted saving, in bytes, that justifies any change.
    pub minimum_saving: u64,
    /// Smallest predicted saving as basis points of current disk usage.
    pub minimum_ratio_bps: u16,
    /// How much more Maximum Space must save than native compression, in
    /// basis points of current disk usage, before it is chosen over native.
    pub maximum_advantage_bps: u16,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            minimum_saving: 16 * 1024 * 1024,
            minimum_ratio_bps: 500,
            maximum_advantage_bps: 500,
        }
    }
}

/// A complete, explainable storage choice for one game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recommendation {
    pub mode: StorageMode,
    /// Predicted bytes saved by `mode`. For `Skip`, the larger of the two
    /// projections.
    pub predicted_saving: u64,
    /// Predicted bytes saved by native compression.
    pub native_saving: u64,
    /// Predicted bytes saved by Maximum Space, when the estimate has one.
    pub maximum_saving: Option<u64>,
    pub confidence: Confidence,
    /// Sentences for the user explaining the choice.
    pub reasons: Vec<String>,
    /// What the chosen mode needs from the system. Empty unless Maximum Space.
    pub requirements: Vec<String>,
}

/// Whether a saving meets both the absolute and the relative minimum.
pub fn clears_threshold(saving: u64, current: u64, policy: Policy) -> bool {
    saving >= policy.minimum_saving
        && current > 0
        && saving.saturating_mul(10_000)
            >= current.saturating_mul(u64::from(policy.minimum_ratio_bps))
}

/// Sampled bytes per ten thousand install bytes, or `None` when the install
/// size is unknown.
fn coverage_bps(estimate: &Estimate) -> Option<u64> {
    (estimate.install_bytes > 0).then(|| {
        let bps = u128::from(estimate.sampled) * 10_000 / u128::from(estimate.install_bytes);
        u64::try_from(bps).unwrap_or(u64::MAX)
    })
}

/// Low with no sampled file, under 4 MiB sampled, or under 0.05% of the
/// install's bytes sampled. Medium when fewer than a quarter of the eligible
/// files were sampled or under 0.5% of the bytes were. High otherwise.
fn confidence(estimate: &Estimate) -> Confidence {
    let considered = estimate
        .inspected_files
        .saturating_add(estimate.unsampled_files);
    let coverage = coverage_bps(estimate);
    if estimate.inspected_files == 0
        || estimate.sampled < 4 * 1024 * 1024
        || coverage.is_some_and(|bps| bps < 5)
    {
        Confidence::Low
    } else if (considered > 0 && estimate.inspected_files.saturating_mul(4) < considered)
        || coverage.is_some_and(|bps| bps < 50)
    {
        Confidence::Medium
    } else {
        Confidence::High
    }
}

/// Selects one mode from native and pack projections.
///
/// Maximum Space needs `maximum_compatible` and either the required advantage
/// over native or no native backend at all. Native needs `native_available`
/// and a saving over the thresholds. Anything else is `Skip`.
pub fn choose(
    estimate: &Estimate,
    native_available: bool,
    maximum_compatible: bool,
    policy: Policy,
) -> Recommendation {
    let native = estimate.saving();
    let maximum = estimate.maximum_saving();
    let current = estimate.current_bytes();
    let native_worthwhile = clears_threshold(native, current, policy);
    let maximum_worthwhile = maximum.is_some_and(|saving| {
        clears_threshold(saving, current, policy)
            && saving.saturating_sub(native).saturating_mul(10_000)
                >= current.saturating_mul(u64::from(policy.maximum_advantage_bps))
    });
    // Without a native backend there is nothing to beat, so the pack only
    // has to clear the ordinary thresholds.
    let maximum_is_only_mode = maximum
        .is_some_and(|saving| !native_available && clears_threshold(saving, current, policy));
    let (mode, predicted_saving) =
        if maximum_compatible && (maximum_worthwhile || maximum_is_only_mode) {
            (StorageMode::MaximumSpace, maximum.unwrap_or(native))
        } else if native_available && native_worthwhile {
            (StorageMode::Native, native)
        } else {
            (StorageMode::Skip, native.max(maximum.unwrap_or(0)))
        };

    let mut reasons = Vec::new();
    match mode {
        StorageMode::Skip if maximum_is_only_mode && !maximum_compatible => reasons.push(
            "This drive has no Standard compression. Maximum could save space here, but it is not applied automatically until a compatibility report for this game exists."
                .into(),
        ),
        StorageMode::Skip if maximum_worthwhile && !maximum_compatible => reasons.push(
            "Maximum could save more, but it is not applied automatically until a compatibility report for this game exists. You can still try it yourself under Advanced."
                .into(),
        ),
        StorageMode::Skip => reasons.push(
            "The measured saving is too small to compress automatically.".into(),
        ),
        StorageMode::Native => {
            reasons.push(
                "Standard compression saves a useful amount and keeps ordinary game files."
                    .into(),
            );
            if maximum_worthwhile && !maximum_compatible {
                reasons.push(
                    "Maximum needs a compatibility report for this game.".into(),
                );
            }
        }
        StorageMode::MaximumSpace if maximum_worthwhile => reasons.push(
            "Maximum is estimated to save at least five percentage points more than Standard."
                .into(),
        ),
        // Chosen because this drive has no native compression to compare with.
        StorageMode::MaximumSpace => reasons.push(
            "This drive has no Standard compression, and Maximum is estimated to save a useful amount."
                .into(),
        ),
    }
    if estimate.unsampled_files > 0 {
        reasons.push(format!(
            "{} outside the sample.",
            crate::text::count(
                estimate.unsampled_files,
                "eligible file is",
                "eligible files are"
            )
        ));
    }
    let requirements = if mode == StorageMode::MaximumSpace {
        vec![
            "Support for Maximum on this system".into(),
            "Room to keep the original while switching".into(),
        ]
    } else {
        Vec::new()
    };
    Recommendation {
        mode,
        predicted_saving,
        native_saving: native,
        maximum_saving: maximum,
        confidence: confidence(estimate),
        reasons,
        requirements,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestResult, check, check_eq};

    fn estimate(native_after: u64, maximum_after: Option<u64>) -> Estimate {
        Estimate {
            install_bytes: 10 * 1024 * 1024 * 1024,
            disk_now: 10 * 1024 * 1024 * 1024,
            disk_after: native_after,
            maximum_after,
            sampled: 32 * 1024 * 1024,
            inspected_files: 20,
            ..Estimate::default()
        }
    }

    #[test]
    fn pack_requires_extra_saving_and_compatibility() -> TestResult {
        let gib = 1024 * 1024 * 1024;
        let measured = estimate(8 * gib, Some(7 * gib));
        check_eq(
            choose(&measured, true, true, Policy::default()).mode,
            StorageMode::MaximumSpace,
            "a proven additional gigabyte selects the pack",
        )?;
        check_eq(
            choose(&measured, true, false, Policy::default()).mode,
            StorageMode::Native,
            "an unproven pack falls back to native compression",
        )
    }

    #[test]
    fn small_returns_are_left_alone() -> TestResult {
        let measured = estimate(10 * 1024 * 1024 * 1024 - 128 * 1024 * 1024, None);
        check_eq(
            choose(&measured, true, false, Policy::default()).mode,
            StorageMode::Skip,
            "automatic work needs an absolute and relative return",
        )
    }

    #[test]
    fn smaller_games_can_still_receive_a_useful_recommendation() -> TestResult {
        let mut measured = estimate(100 * 1024 * 1024, None);
        measured.disk_now = 200 * 1024 * 1024;
        measured.install_bytes = 200 * 1024 * 1024;
        check_eq(
            choose(&measured, true, false, Policy::default()).mode,
            StorageMode::Native,
            "a small game with a large relative saving is worth compressing",
        )?;
        measured.disk_after = measured.disk_now - 8 * 1024 * 1024;
        check_eq(
            choose(&measured, true, false, Policy::default()).mode,
            StorageMode::Skip,
            "a small absolute saving still avoids a rewrite",
        )
    }

    #[test]
    fn unavailable_native_backend_is_never_recommended() -> TestResult {
        let gib = 1024 * 1024 * 1024;
        let measured = estimate(8 * gib, Some(7 * gib));
        check_eq(
            choose(&measured, false, false, Policy::default()).mode,
            StorageMode::Skip,
            "a filesystem without a native backend cannot select one",
        )
    }

    #[test]
    fn the_threshold_is_measured_against_the_whole_install() -> TestResult {
        let mib = 1024 * 1024;
        let gib = 1024 * mib;
        // A 50 GiB game: 49 GiB of incompressible video, 1 GiB of DLLs that
        // shrink by 30 percent. Only the DLLs reach `disk_now`.
        let realistic = Estimate {
            install_bytes: 50 * gib,
            disk_now: gib,
            disk_after: gib - 300 * mib,
            sampled: 32 * mib,
            inspected_files: 20,
            ..Estimate::default()
        };
        check_eq(
            choose(&realistic, true, false, Policy::default()).mode,
            StorageMode::Skip,
            "300 MiB is 0.6 percent of the game",
        )?;
        let mut compressible = realistic;
        compressible.install_bytes = gib;
        check_eq(
            choose(&compressible, true, false, Policy::default()).mode,
            StorageMode::Native,
            "control: the same saving on a 1 GiB game is worth taking",
        )
    }

    #[test]
    fn confidence_falls_with_the_share_of_bytes_sampled() -> TestResult {
        let gib = 1024 * 1024 * 1024;
        let mib = 1024 * 1024;
        let thin = Estimate {
            install_bytes: 90 * gib,
            disk_now: 90 * gib,
            disk_after: 45 * gib,
            sampled: 30 * mib,
            inspected_files: 30,
            ..Estimate::default()
        };
        check_eq(
            choose(&thin, true, false, Policy::default()).confidence,
            Confidence::Low,
            "30 files of 3 GB with 30 MiB read is not a strong estimate",
        )?;
        let thorough = Estimate {
            install_bytes: gib,
            sampled: 32 * mib,
            ..thin
        };
        check_eq(
            choose(&thorough, true, false, Policy::default()).confidence,
            Confidence::High,
            "control: the same sample of a 1 GiB game is strong",
        )
    }

    #[test]
    fn a_drive_without_native_support_names_the_pack_gate() -> TestResult {
        let gib = 1024 * 1024 * 1024;
        let measured = estimate(8 * gib, Some(7 * gib));
        let skipped = choose(&measured, false, false, Policy::default());
        check_eq(
            skipped.mode,
            StorageMode::Skip,
            "the pack is not yet approved",
        )?;
        check(
            skipped
                .reasons
                .iter()
                .any(|reason| reason.contains("compatibility")),
            format!(
                "the reason names the compatibility gate: {:?}",
                skipped.reasons
            ),
        )
    }
}
