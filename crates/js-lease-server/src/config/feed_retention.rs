//! `--job-feed-retention` and `server.job_feed_retention`: how many of the newest changes
//! the job store's change feed keeps. Older rows are trimmed on each tick
//! ([`crate::job_store::FeedTrim`]); the count is of feed changes, never a time.

use std::num::NonZeroU64;

/// The default of `--job-feed-retention` and `server.job_feed_retention`.
pub const DEFAULT_JOB_FEED_RETENTION: &str = "100000";

/// What a refused count must be. It never repeats the value: a file's refusal does not.
pub(crate) const JOB_FEED_RETENTION_MUST: &str =
    "must be a whole number of feed changes to keep, at least 1, such as 100000";

/// How many of the newest feed changes of the job store are kept: at least one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedRetention(NonZeroU64);

impl FeedRetention {
    /// The default, 100 000 changes.
    pub const DEFAULT: FeedRetention = FeedRetention(NonZeroU64::MIN.saturating_add(99_999));

    /// A count of `keep` changes; `None` for zero.
    #[must_use]
    pub fn new(keep: u64) -> Option<FeedRetention> {
        NonZeroU64::new(keep).map(FeedRetention)
    }

    /// How many of the newest changes are kept.
    #[must_use]
    pub fn keep(self) -> u64 {
        self.0.get()
    }
}

/// A count of feed changes to keep: decimal digits only (no sign, no fraction, no unit),
/// at least 1, and small enough for a `u64`.
///
/// # Errors
/// [`JOB_FEED_RETENTION_MUST`] as text when `text` is not such a count.
pub fn parse_job_feed_retention(text: &str) -> Result<FeedRetention, String> {
    Some(text)
        .filter(|text| text.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|text| text.parse::<u64>().ok())
        .and_then(FeedRetention::new)
        .ok_or_else(|| JOB_FEED_RETENTION_MUST.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_JOB_FEED_RETENTION, FeedRetention, JOB_FEED_RETENTION_MUST,
        parse_job_feed_retention,
    };
    use crate::config::Cli;

    fn keep(text: &str) -> Result<u64, String> {
        parse_job_feed_retention(text).map(FeedRetention::keep)
    }

    // Catches a default other than 100 000 changes, in the parser, the type or the flag.
    #[test]
    fn job_feed_retention_defaults_to_one_hundred_thousand_changes() {
        assert_eq!(keep(DEFAULT_JOB_FEED_RETENTION), Ok(100_000));
        assert_eq!(FeedRetention::DEFAULT.keep(), 100_000);
        let cli = Cli::try_parse_from(["js-lease-server", "--config", "js.toml"])
            .expect("the command line parses");
        assert_eq!(cli.job_feed_retention, FeedRetention::DEFAULT);
    }

    // Catches a zero, signed, fractional, unit-carrying, overflowing or empty count
    // accepted, and a refusal that does not name the flag.
    #[test]
    fn job_feed_retention_of_zero_or_a_non_number_is_refused_naming_the_flag() {
        for bad in [
            "0",
            "000",
            "+5",
            "-5",
            "1.5",
            "5d",
            "5 ",
            "many",
            "",
            "18446744073709551616",
        ] {
            assert_eq!(
                keep(bad),
                Err(JOB_FEED_RETENTION_MUST.to_owned()),
                "{bad:?} was accepted"
            );
            let flag = format!("--job-feed-retention={bad}");
            let refused =
                Cli::try_parse_from(["js-lease-server", "--config", "js.toml", flag.as_str()])
                    .expect_err("the command line is refused");
            let said = refused.to_string();
            assert!(said.contains("--job-feed-retention"), "{bad:?}: {said}");
            assert!(said.contains(JOB_FEED_RETENTION_MUST), "{bad:?}: {said}");
            assert_eq!(refused.exit_code(), 2, "{bad:?}");
        }
        for (good, count) in [
            ("1", 1),
            ("007", 7),
            ("100000", 100_000),
            ("18446744073709551615", u64::MAX),
        ] {
            assert_eq!(keep(good), Ok(count), "{good:?}");
        }
        assert_eq!(FeedRetention::new(0), None);
    }
}
