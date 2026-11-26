//! The log prefix of each attempt of a job (`docs/design/job-retry-and-completion-callback.md`,
//! section 3.8), which the start's sink names and the reader of `GetLogs` reads under, and the
//! attempts of a job as both need them ([`Attempts`]).
//!
//! **The key.** Attempt 1 keeps the prefix a job has always had, `prefix/H/job_id/`, so a job
//! without retries, and every log written before attempts, reads as before. Attempt `n ≥ 2` gets
//! `prefix/H/S/job_id/`, where `H` is the host's segment of that attempt's host
//! ([`js_logmint::host_prefix`]) and `S` is [`attempt_segment`]: the 64 lower-case hex digits of
//! SHA-256 of `job_id "/" n`. A job id is at most 63 bytes, so `S` is never a job id and no
//! job's attempt-1 prefix holds another job's later attempt; two later attempts differ in `S`.
//! The last segment is still the job id, as the agent's check of a start requires, and the key
//! stays under the host's prefix, which the host's credential covers, so no agent changes.
//!
//! **Length.** The base of a job's prefix (`prefix/H`, or `prefix/H/S`, [`ATTEMPT_SEGMENT_LEN`]
//! and its `/` longer) may be at most [`js_logstore::MAX_PREFIX_LEN`] bytes. A configured prefix
//! of at most 476 bytes (what `[logs]` takes) fits attempt 1, and one of at most
//! [`LONGEST_PREFIX_FOR_EVERY_ATTEMPT`] fits every attempt. Above that, an attempt after the
//! first runs without live logs, as a job whose id is too long does: [`attempt_prefix`] builds
//! it no prefix, so its start carries no sink and the reader answers `no-live-logs` for it.
//! [`warn_when_later_attempts_lack_room`] says so once at start, and nothing is refused.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use js_jobs::{AttemptRow, JobRecord};
use js_logmint::{LogPrefix, host_prefix};
use js_logstore::JobPrefix;
use sha2::{Digest as _, Sha256};

/// The length of an attempt's segment `S`: 64 hex digits, one more than the longest job id.
pub(crate) const ATTEMPT_SEGMENT_LEN: usize = 64;

/// The longest configured log prefix under which every attempt of a job has live logs: the
/// layout's 512 bytes less a host's segment and its `/` (36 bytes) and an attempt's segment
/// and its `/` (65 bytes).
pub(crate) const LONGEST_PREFIX_FOR_EVERY_ATTEMPT: usize = 411;

/// The segment `S` of attempt `attempt` of the job `job_id`: the lower-case hex of SHA-256 of
/// `job_id "/" attempt`.
pub(crate) fn attempt_segment(job_id: &str, attempt: u32) -> String {
    let digest = Sha256::digest(format!("{job_id}/{attempt}"));
    digest.iter().fold(
        String::with_capacity(ATTEMPT_SEGMENT_LEN),
        |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        },
    )
}

/// The prefix `P` of attempt `attempt` of the job `job_id` on the host `host`, under the
/// configured log prefix `prefix`: `prefix/H/job_id/` for attempt 1 (and for 0, which an
/// older record reads as 1), `prefix/H/S/job_id/` for a later one. `None` when the layout
/// builds none: a job id that does not begin with a letter or a digit or is longer than 63
/// bytes, or a base longer than the layout allows.
pub(crate) fn attempt_prefix(
    prefix: &LogPrefix,
    host: &str,
    job_id: &str,
    attempt: u32,
) -> Option<JobPrefix> {
    let host = host_prefix(prefix, host).ok()?;
    let host = host.as_str().trim_end_matches('/');
    let base = match attempt {
        0 | 1 => host.to_owned(),
        later => format!("{host}/{}", attempt_segment(job_id, later)),
    };
    JobPrefix::new(&base, job_id).ok()
}

/// Whether every attempt of a job has room for its prefix under the configured `prefix`:
/// the same check [`attempt_prefix`] makes, for a second attempt (every later one has a base
/// of the same length).
pub(crate) fn later_attempts_fit(prefix: &LogPrefix) -> bool {
    attempt_prefix(prefix, "", "job", 2).is_some()
}

/// Logs a warning when the configured `prefix` leaves no room for the segment of an attempt
/// after the first: such attempts run without live logs. Nothing is refused, so a
/// configuration that worked before attempts keeps working.
pub(crate) fn warn_when_later_attempts_lack_room(prefix: &LogPrefix) {
    if !later_attempts_fit(prefix) {
        tracing::warn!(
            "the log prefix is longer than {LONGEST_PREFIX_FOR_EVERY_ATTEMPT} bytes: a job's \
             attempts after the first run without live logs"
        );
    }
}

/// The attempts of a job, as its logs need them: the number of its current attempt, and the
/// host and times of each attempt its record still lists as ended, by number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attempts {
    /// The current attempt, or the last that ran, from 1.
    pub(crate) current: u32,
    /// The ended attempts the record lists, by number.
    pub(crate) ended: BTreeMap<u32, AttemptRow>,
}

impl Attempts {
    /// The attempts of `job`. This release's record holds no attempt number and no history:
    /// a submit refuses a retry policy (`job_api/validate.rs`), so every job is on its first
    /// attempt and none ended before it.
    pub(crate) fn of(job: &JobRecord) -> Attempts {
        let _ = job;
        Attempts {
            current: 1,
            ended: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests;
