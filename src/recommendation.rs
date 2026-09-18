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
            Self::Skip => "Already efficient",
            Self::Native => "Transparent compression",
            Self::MaximumSpace => "Maximum Space",
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
    pub minimum_saving: u64,
    pub minimum_ratio_bps: u16,
    pub maximum_advantage_bps: u16,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            minimum_saving: 256 * 1024 * 1024,
            minimum_ratio_bps: 500,
            maximum_advantage_bps: 500,
        }
    }
}

/// A complete, explainable storage choice for one game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recommendation {
    pub mode: StorageMode,
    pub predicted_saving: u64,
    pub native_saving: u64,
    pub maximum_saving: Option<u64>,
    pub confidence: Confidence,
    pub reasons: Vec<String>,
    pub requirements: Vec<String>,
}

fn clears_threshold(saving: u64, current: u64, policy: Policy) -> bool {
    saving >= policy.minimum_saving
        && current > 0
        && saving.saturating_mul(10_000)
            >= current.saturating_mul(u64::from(policy.minimum_ratio_bps))
}

fn confidence(estimate: &Estimate) -> Confidence {
    let considered = estimate
        .inspected_files
        .saturating_add(estimate.unsampled_files);
    if estimate.inspected_files == 0 || estimate.sampled < 4 * 1024 * 1024 {
        Confidence::Low
    } else if considered > 0 && estimate.inspected_files.saturating_mul(4) < considered {
        Confidence::Medium
    } else {
        Confidence::High
    }
}

/// Selects one mode from native and pack projections.
pub fn choose(
    estimate: &Estimate,
    native_available: bool,
    maximum_compatible: bool,
    policy: Policy,
) -> Recommendation {
    let native = estimate.saving();
    let maximum = estimate.maximum_saving();
    let native_worthwhile = clears_threshold(native, estimate.disk_now, policy);
    let maximum_worthwhile = maximum.is_some_and(|saving| {
        clears_threshold(saving, estimate.disk_now, policy)
            && saving.saturating_sub(native).saturating_mul(10_000)
                >= estimate
                    .disk_now
                    .saturating_mul(u64::from(policy.maximum_advantage_bps))
    });
    let maximum_is_only_mode = maximum.is_some_and(|saving| {
        !native_available && clears_threshold(saving, estimate.disk_now, policy)
    });
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
        StorageMode::Skip if maximum_worthwhile && !maximum_compatible => reasons.push(
            "Maximum Space could save more, but automatic activation waits for a game-specific compatibility result. Its advanced controls remain available for local testing."
                .into(),
        ),
        StorageMode::Skip => reasons.push(
            "The measured saving is below the automatic optimization threshold.".into(),
        ),
        StorageMode::Native => {
            reasons.push(
                "Native compression provides a worthwhile saving with ordinary game files."
                    .into(),
            );
            if maximum_worthwhile && !maximum_compatible {
                reasons.push(
                    "Maximum Space needs a completed compatibility check for this game.".into(),
                );
            }
        }
        StorageMode::MaximumSpace => reasons.push(
            "The verified pack projection saves at least five percentage points beyond native compression."
                .into(),
        ),
    }
    if estimate.unsampled_files > 0 {
        reasons.push(format!(
            "{} eligible files remain outside the sample.",
            estimate.unsampled_files
        ));
    }
    let requirements = if mode == StorageMode::MaximumSpace {
        vec![
            "Verified writable mount support".into(),
            "Rollback space during activation".into(),
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
    use crate::testutil::{TestResult, check_eq};

    fn estimate(native_after: u64, maximum_after: Option<u64>) -> Estimate {
        Estimate {
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
    fn unavailable_native_backend_is_never_recommended() -> TestResult {
        let gib = 1024 * 1024 * 1024;
        let measured = estimate(8 * gib, Some(7 * gib));
        check_eq(
            choose(&measured, false, false, Policy::default()).mode,
            StorageMode::Skip,
            "a filesystem without a native backend cannot select one",
        )
    }
}
