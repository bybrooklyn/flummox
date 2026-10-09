//! Wording helpers shared by messages the window and the command line print.

/// `"1 file"` or `"3 files"`: the count, a space, and `one` when `count` is 1,
/// otherwise `many`.
pub fn count(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// A whole number of seconds as `"45 s"`, `"3 min 5 s"` or `"1 h 2 min"`.
/// Seconds are dropped once the duration reaches an hour.
pub fn duration(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    if hours > 0 {
        format!("{hours} h {minutes} min")
    } else if minutes > 0 {
        format!("{minutes} min {seconds} s")
    } else {
        format!("{seconds} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestResult, check_eq};

    #[test]
    fn one_is_singular_and_everything_else_is_plural() -> TestResult {
        check_eq(count(1, "file", "files"), "1 file".to_owned(), "one")?;
        check_eq(count(0, "file", "files"), "0 files".to_owned(), "zero")?;
        check_eq(count(2, "game", "games"), "2 games".to_owned(), "two")
    }

    #[test]
    fn durations_use_the_two_largest_units() -> TestResult {
        check_eq(duration(0), "0 s".to_owned(), "zero")?;
        check_eq(duration(45), "45 s".to_owned(), "seconds")?;
        check_eq(duration(185), "3 min 5 s".to_owned(), "minutes")?;
        check_eq(duration(3725), "1 h 2 min".to_owned(), "hours")?;
        check_eq(duration(7200), "2 h 0 min".to_owned(), "whole hours")
    }
}
