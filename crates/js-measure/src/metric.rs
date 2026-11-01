//! The record of one run of a plan, in the format the analysis reads: one JSON object per
//! line, the identity of the run, and named metrics that are a number or `null`.
//!
//! A number the code cannot read today is `null`, never `0` and never `NaN`: the analysis
//! leaves it out of that metric's statistics and counts it as missing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The metric names of a run record. A per-job row (`jobs.jsonl`) uses the `JOB_*` names.
pub mod name {
    /// First submit to last finish, in milliseconds; null when any job has no finish.
    pub const MAKESPAN_MS: &str = "makespan_ms";
    /// Jobs of the workload.
    pub const JOBS: &str = "jobs";
    /// Jobs that did not end with outcome `succeeded` (or never ran).
    pub const JOBS_FAILED: &str = "jobs_failed";
    /// Jobs whose outcome was `out_of_memory`.
    pub const MEMORY_KILLS: &str = "memory_kills";
    /// Jobs that did not succeed but printed a result line: their numbers are left out of the
    /// `reported_` metrics. 0 when there are none.
    pub const REPORTS_DROPPED: &str = "reports_dropped";
    /// Jobs that succeeded but printed no result line: no `reported_` metric is recorded for
    /// the run. 0 when there are none.
    pub const REPORTS_MISSING: &str = "reports_missing";
    /// Mean over jobs of the wait from the job's planned submit time to the first poll that
    /// saw its run running (good to two poll intervals).
    pub const QUEUE_WAIT_MS_MEAN: &str = "queue_wait_ms_mean";
    /// Largest of the same.
    pub const QUEUE_WAIT_MS_MAX: &str = "queue_wait_ms_max";
    /// Mean over jobs of the time from sending the lease request to holding the lease.
    pub const REQUEST_TO_TOKEN_MS_MEAN: &str = "request_to_token_ms_mean";
    /// Largest of the same.
    pub const REQUEST_TO_TOKEN_MS_MAX: &str = "request_to_token_ms_max";
    /// Mean over the host-minutes of the run window of a host's CPU in use, as a share of
    /// the host's CPU shape.
    pub const HOST_CPU_SHARE_MEAN: &str = "host_cpu_share_mean";
    /// Largest of the same.
    pub const HOST_CPU_SHARE_MAX: &str = "host_cpu_share_max";
    /// Mean of a host's working set as a share of the host's memory shape.
    pub const HOST_RAM_SHARE_MEAN: &str = "host_ram_share_mean";
    /// Largest of the same.
    pub const HOST_RAM_SHARE_MAX: &str = "host_ram_share_max";
    /// Mean over jobs of observed run time over the run time the server's curve for the
    /// job's profile predicts (the k of the paper).
    pub const RUNTIME_ACTUAL_OVER_PREDICTED_MEAN: &str = "runtime_actual_over_predicted_mean";
    /// Mean over jobs of the peak working set seen over the peak the curve predicts.
    pub const MEMORY_ACTUAL_OVER_PREDICTED_MEAN: &str = "memory_actual_over_predicted_mean";
    /// CPU used over CPU booked, both summed over the jobs that have a reading.
    pub const CPU_USED_OVER_BOOKED: &str = "cpu_used_over_booked";
    /// Peak working set used over memory booked, summed over the jobs that have a reading.
    pub const MEMORY_USED_OVER_BOOKED: &str = "memory_used_over_booked";
}

/// The label of a warm-up run.
pub const WARMUP: &str = "warmup";

/// A reading as it is written: a finite number stays, anything else (no reading, NaN, an
/// infinity) becomes `None`, which is written as `null`.
#[must_use]
pub fn finite(reading: Option<f64>) -> Option<f64> {
    reading.filter(|value| value.is_finite())
}

/// One run of a plan: one line of `records.jsonl`.
///
/// This is the shape `js-analysis` reads (`RunRecord`, `deny_unknown_fields`): no field
/// may be added here without a change there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The value of the varied parameter, as text (for example `0.8` for a fill of 0.8):
    /// records with the same text are the same setting.
    pub setting: String,
    /// The arm: what was run at that setting (for example a placement rule), or the
    /// control's label.
    pub arm: String,
    /// The place of the run in the executed order of the whole plan, from 0 (so unique
    /// within its setting and arm, and the order effects stay visible).
    pub run_index: u32,
    /// The seed the order was drawn from.
    pub seed: u64,
    /// The code commit under test, as given to `--commit`.
    pub commit: String,
    /// `"warmup"` on a run that warms the farm up (kept apart in the analysis), else empty.
    #[serde(default)]
    pub label: String,
    /// The metrics by name; see [`name`]. `null` is not available.
    pub metrics: BTreeMap<String, Option<f64>>,
}
