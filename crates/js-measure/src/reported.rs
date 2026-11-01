//! The numbers a job reports about itself.
//!
//! A job that wants to be measured prints one line to its standard output:
//!
//! ```text
//! js-measure-result {"steady_ms":12.3,"first_run_ms":45.6,"cache_hit":null}
//! ```
//!
//! The line is the tag [`TAG`], one space, and a JSON object whose values are numbers (any
//! other value is ignored, see below). The harness reads the end of the run's output from the lease server after the
//! run is over (the log tail, at most 8 KiB of each stream) and takes the numbers from it.
//!
//! # The rules
//!
//! - Only standard output is read. The last line that begins with the tag decides; the
//!   lines before it are ignored, so a job may print the line again as it improves it.
//! - A key whose value is a number is recorded as that number. A key whose value is
//!   anything else (a string such as a verdict, a boolean, an array, an object) or `null`
//!   is skipped: no key is recorded, so the analysis never sees an always-missing metric.
//! - No line, a line that is not a JSON object, or a tail that holds no complete line
//!   (the output was cut, or the server holds no tail for the run) records no key at all.
//!   The harness cannot know a key it never read, so the analysis counts the run as having
//!   no value for it. Nothing is invented and nothing is set to zero.
//! - Every key is recorded under the name [`PREFIX`] followed by the key, so a job can
//!   never overwrite a number the harness measured itself (`makespan_ms`,
//!   `request_to_token_ms_mean`, ...). A key `steady_ms` is `reported_steady_ms`.
//! - A workload of several jobs records, for each key, the mean of the numbers of the jobs
//!   that succeeded, and no key when any of those jobs has no number for it. A job that
//!   succeeded and printed no line (a cut tail, a forgotten run, a runner that crashed
//!   after exit 0) lacks every key, so the run records no `reported_` key at all.
//! - Only a job whose outcome is `succeeded` speaks for the run. The report of any other
//!   job (it answered wrongly, ran out of memory, was killed) is left out of the record's
//!   numbers. The job's own row in `jobs.jsonl` still shows what it printed, with its
//!   outcome.
//! - The harness counts both cases outside the job's `reported_` namespace, so a job's own
//!   key can never collide: `reports_dropped` is the number of jobs that did not succeed
//!   but printed a line, `reports_missing` the number of jobs that succeeded and printed
//!   none. Each is always recorded, 0 when there are none (a measured count, not
//!   missing data).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::metric::finite;

/// The tag a job's result line begins with.
pub const TAG: &str = "js-measure-result";

/// What a reported key is prefixed with in the records.
pub const PREFIX: &str = "reported_";

/// The keys of the last result line of `stdout`, each a number or `None`.
#[must_use]
pub fn parse(stdout: &str) -> BTreeMap<String, Option<f64>> {
    stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(TAG)?.strip_prefix(' '))
        .and_then(|json| serde_json::from_str::<BTreeMap<String, Value>>(json).ok())
        .map(|keys| {
            keys.into_iter()
                .filter_map(|(key, value)| Some((key, Some(value.as_f64()?))))
                .collect()
        })
        .unwrap_or_default()
}

/// `reported` with every key prefixed.
#[must_use]
pub fn prefixed(reported: &BTreeMap<String, Option<f64>>) -> BTreeMap<String, Option<f64>> {
    reported
        .iter()
        .map(|(key, value)| (format!("{PREFIX}{key}"), *value))
        .collect()
}

/// The record's metrics from the jobs' reports: see the module rules.
#[must_use]
pub fn merge(jobs: &[BTreeMap<String, Option<f64>>]) -> BTreeMap<String, Option<f64>> {
    let keys: BTreeSet<&String> = jobs.iter().flat_map(BTreeMap::keys).collect();
    keys.into_iter()
        .filter_map(|key| {
            let numbers: Option<Vec<f64>> = jobs
                .iter()
                .map(|job| job.get(key).copied().flatten())
                .collect();
            let mean = numbers.map(|all| all.iter().sum::<f64>() / all.len() as f64);
            Some((format!("{PREFIX}{key}"), finite(Some(mean?))))
        })
        .collect()
}
