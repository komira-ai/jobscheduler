//! `--job-retention` and `server.job_retention`: how long an ended job's record and result
//! are kept after the job ends (design section 3.2 and question 14).

use std::time::Duration;

use js_jobs::Retention;

use super::parse_duration;

/// The default of `--job-retention` and `server.job_retention`: seven days.
pub const DEFAULT_JOB_RETENTION: &str = "7d";

/// The default of `--job-allocation-retention` and `server.job_allocation_retention`: fourteen
/// days, [`Retention::ALLOCATION_DEFAULT`].
pub const DEFAULT_JOB_ALLOCATION_RETENTION: &str = "14d";

/// What a refused window must be. It never repeats the value: a file's refusal does not.
pub(crate) const JOB_RETENTION_MUST: &str =
    "must be a duration of at least one minute, such as 7d, 36h or 90m";

/// Seconds in a day.
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// A retention window: a whole count of days (`7d`), or a duration as [`parse_duration`]
/// reads it (`36h`, `90m`, `3600s`, or a bare count of seconds), of at least one minute
/// ([`Retention::MIN`]). Zero, a negative or fractional count, a count too large to hold,
/// a sign, a window under a minute and anything else are refused.
///
/// # Errors
/// [`JOB_RETENTION_MUST`] as text when `text` is not such a window.
pub fn parse_job_retention(text: &str) -> Result<Retention, String> {
    days_or_duration(text)
        .and_then(|window| Retention::new(window).ok())
        .ok_or_else(|| JOB_RETENTION_MUST.to_owned())
}

/// A whole count of days (`7d`), or a duration as [`parse_duration`] reads it; `None` for
/// anything else, zero included.
pub(super) fn days_or_duration(text: &str) -> Option<Duration> {
    match text.strip_suffix('d') {
        // Digits only: `u64`'s parser would also take a leading `+`.
        Some(days) => Some(days)
            .filter(|days| days.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|days| days.parse::<u64>().ok())
            .and_then(|days| days.checked_mul(SECS_PER_DAY))
            .map(Duration::from_secs),
        None => parse_duration(text).ok(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use js_jobs::Retention;

    use super::{DEFAULT_JOB_RETENTION, JOB_RETENTION_MUST, parse_job_retention};
    use crate::config::Cli;

    fn window(text: &str) -> Result<Duration, String> {
        parse_job_retention(text).map(Retention::window)
    }

    // Catches a default other than seven days, in the parser or in the flag.
    #[test]
    fn job_retention_defaults_to_seven_days() {
        assert_eq!(
            window(DEFAULT_JOB_RETENTION),
            Ok(Duration::from_secs(604_800))
        );
        let cli = Cli::try_parse_from(["js-lease-server", "--config", "js.toml"])
            .expect("the command line parses");
        assert_eq!(cli.job_retention, Retention::DEFAULT);
        assert_eq!(Retention::DEFAULT.window(), Duration::from_secs(604_800));
    }

    // Catches a zero, negative, fractional, overflowing, short or unparsable window accepted,
    // a refusal that does not name the flag, and a unit read wrongly.
    #[test]
    fn job_retention_of_zero_is_refused_naming_the_flag() {
        for bad in [
            "0",
            "0d",
            "0s",
            "+7d",
            "-1d",
            "-7d",
            "1.5d",
            "d",
            "59s",
            "59999ms",
            "soon",
            "7 d",
            "7D",
            "18446744073709551615d",
        ] {
            assert_eq!(
                window(bad),
                Err(JOB_RETENTION_MUST.to_owned()),
                "{bad:?} was accepted"
            );
            // With `=`, so that a value that starts with a dash is the flag's value.
            let flag = format!("--job-retention={bad}");
            let refused =
                Cli::try_parse_from(["js-lease-server", "--config", "js.toml", flag.as_str()])
                    .expect_err("the command line is refused");
            let said = refused.to_string();
            assert!(said.contains("--job-retention"), "{bad:?}: {said}");
            assert!(said.contains(JOB_RETENTION_MUST), "{bad:?}: {said}");
            assert_eq!(refused.exit_code(), 2, "{bad:?}");
        }
        assert_eq!(window(""), Err(JOB_RETENTION_MUST.to_owned()));
        for (good, secs) in [
            ("1m", 60),
            ("60", 60),
            ("60000ms", 60),
            ("90m", 5400),
            ("36h", 129_600),
            ("1d", 86_400),
            ("30d", 2_592_000),
        ] {
            assert_eq!(window(good), Ok(Duration::from_secs(secs)), "{good:?}");
        }
    }
}
