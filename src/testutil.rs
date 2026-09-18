//! Assertion helpers that report failures instead of panicking.
//!
//! The workspace denies `unwrap`, `expect`, `panic` and `indexing_slicing`
//! everywhere, tests included, so `assert!` and `.unwrap()` are not available
//! in test bodies either. A test here returns [`TestResult`] and uses `?`:
//!
//! ```
//! use flummox::testutil::{Ctx, TestResult, check_eq};
//!
//! // In a real test module this carries #[test].
//! fn two_plus_two() -> TestResult {
//!     let text = std::str::from_utf8(b"4").ctx("utf8")?;
//!     check_eq(2 + 2, text.parse::<i32>().ctx("parse")?, "arithmetic")
//! }
//! two_plus_two()?;
//! # Ok::<(), String>(())
//! ```

use std::fmt::{Debug, Display};

/// What a test returns. The `String` is the failure message.
pub type TestResult = Result<(), String>;

/// Fails unless `cond` holds.
pub fn check(cond: bool, msg: impl Display) -> TestResult {
    if cond { Ok(()) } else { Err(msg.to_string()) }
}

/// Fails unless the two values are equal, showing both when they are not.
pub fn check_eq<T: PartialEq + Debug>(actual: T, expected: T, msg: impl Display) -> TestResult {
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "{msg}\n  actual:   {actual:?}\n  expected: {expected:?}"
        ))
    }
}

/// Fails if the two values are equal.
pub fn check_ne<T: PartialEq + Debug>(a: T, b: T, msg: impl Display) -> TestResult {
    if a == b {
        Err(format!("{msg}\n  both were: {a:?}"))
    } else {
        Ok(())
    }
}

/// Turns an error or a `None` into a test failure message.
///
/// This replaces `.unwrap()` and `.expect(..)` in test bodies.
pub trait Ctx<T> {
    /// Adds context and converts the failure to a `String`.
    fn ctx(self, what: impl Display) -> Result<T, String>;
}

impl<T, E: Display> Ctx<T> for Result<T, E> {
    fn ctx(self, what: impl Display) -> Result<T, String> {
        self.map_err(|e| format!("{what}: {e}"))
    }
}

impl<T> Ctx<T> for Option<T> {
    fn ctx(self, what: impl Display) -> Result<T, String> {
        self.ok_or_else(|| format!("{what}: nothing there"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_reports_only_failures() -> TestResult {
        check(true, "fine")?;
        check_eq(1 + 1, 2, "arithmetic")?;
        check_ne(1, 2, "different")?;
        let failure = check_eq(1, 2, "mismatch");
        check(failure.is_err(), "a mismatch must fail")?;
        let msg = failure.err().unwrap_or_default();
        check(
            msg.contains("expected: 2"),
            "the message should show both values",
        )
    }

    #[test]
    fn ctx_converts_results_and_options() -> TestResult {
        let ok: Result<i32, String> = Ok(3);
        check_eq(ok.ctx("value")?, 3, "Ok passes through")?;
        let err: Result<i32, String> = Err("boom".to_owned());
        check_eq(
            err.ctx("value").err().unwrap_or_default(),
            "value: boom".to_owned(),
            "errors are labelled",
        )?;
        check(Option::<i32>::None.ctx("missing").is_err(), "None fails")
    }
}
