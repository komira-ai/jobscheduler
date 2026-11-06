//! `--max-job-wall` and `server.max_job_wall`: the most a submitted job's wall limit may be
//! (design section 3.2, "What bounds a job"). A job asking for more is refused
//! `wall-time-above-ceiling`. It is read once at start; unset, it is the job service's own
//! default, [`DEFAULT_MAX_JOB_WALL`], and it never reads `server.max_lease_wall`.

use std::time::Duration;

use super::Config;
use super::retention::days_or_duration;
use crate::job_api::submit::DEFAULT_MAX_JOB_WALL;

/// What a refused ceiling must be. It never repeats the value: a file's refusal does not.
pub(crate) const MAX_JOB_WALL_MUST: &str =
    "must be a duration of at least one second, such as 30d, 12h or 90m";

/// A ceiling: a whole count of days (`30d`), or a duration as
/// [`super::parse_duration`] reads it (`12h`, `90m`, `3600s`, or a bare count of seconds),
/// of at least one second. Zero, a negative or fractional count, a count too large to hold,
/// a sign, a ceiling under a second and anything else are refused.
///
/// # Errors
/// [`MAX_JOB_WALL_MUST`] as text when `text` is not such a ceiling.
pub fn parse_max_job_wall(text: &str) -> Result<Duration, String> {
    days_or_duration(text)
        .filter(|wall| *wall >= Duration::from_secs(1))
        .ok_or_else(|| MAX_JOB_WALL_MUST.to_owned())
}

/// The ceiling `given` by the flag or the key, else [`DEFAULT_MAX_JOB_WALL`].
pub(super) fn or_default(given: Option<Duration>) -> Duration {
    given.unwrap_or(DEFAULT_MAX_JOB_WALL)
}

impl Config {
    /// The most a submitted job's wall limit may be (`--max-job-wall`, or
    /// `server.max_job_wall`): the value given, else [`DEFAULT_MAX_JOB_WALL`], 30 days.
    pub fn max_job_wall(&self) -> Duration {
        self.max_job_wall
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DEFAULT_MAX_JOB_WALL, MAX_JOB_WALL_MUST, parse_max_job_wall};
    use crate::config::Cli;

    /// Asserts that the ceiling has a named default of 30 days, the longest
    /// `server.max_lease_wall` accepts, and that the flag left out reads as no value (so the
    /// default is the job service's constant, not a second copy in the flag).
    #[test]
    fn max_job_wall_has_a_named_default() {
        assert_eq!(DEFAULT_MAX_JOB_WALL, Duration::from_secs(30 * 24 * 3600));
        let cli = Cli::try_parse_from(["js-lease-server", "--config", "js.toml"])
            .expect("the command line parses");
        assert_eq!(cli.max_job_wall, None);
        assert_eq!(super::or_default(None), DEFAULT_MAX_JOB_WALL);
        let given = Duration::from_secs(60);
        assert_eq!(super::or_default(Some(given)), given);
    }

    /// Asserts each spelling a ceiling may have: days, hours, minutes, seconds, a bare count
    /// and the shortest, one second.
    #[test]
    fn max_job_wall_reads_days_and_durations() {
        let cases = [
            ("30d", 2_592_000),
            ("12h", 43_200),
            ("90m", 5_400),
            ("3600s", 3_600),
            ("600", 600),
            ("1s", 1),
        ];
        for (text, secs) in cases {
            assert_eq!(
                parse_max_job_wall(text),
                Ok(Duration::from_secs(secs)),
                "{text}"
            );
        }
    }

    /// Asserts that zero, a negative, fractional or signed value, a ceiling under a second and
    /// text that is no duration are refused at start naming the flag, with what the value
    /// must be and not the value. It catches a ceiling of zero that would refuse every job.
    #[test]
    fn max_job_wall_of_zero_is_refused_naming_the_flag() {
        for bad in ["0", "0d", "0s", "-1d", "+1d", "1.5d", "999ms", "soon", "7D"] {
            let flag = format!("--max-job-wall={bad}");
            let refused = Cli::try_parse_from(["js-lease-server", "--config", "js.toml", &flag])
                .expect_err("the command line is refused");
            let said = refused.to_string();
            assert!(said.contains("--max-job-wall"), "{bad:?}: {said}");
            assert!(said.contains(MAX_JOB_WALL_MUST), "{bad:?}: {said}");
            assert_eq!(refused.exit_code(), 2, "{bad:?}");
        }
    }
}
