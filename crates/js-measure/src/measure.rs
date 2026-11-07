//! From what a drive saw and what the server reports to the numbers of the paper. Every
//! function here is arithmetic on values; a number whose inputs are missing is `None`
//! (`null` in the record), never a zero.
//!
//! # Where each number comes from
//!
//! - **host CPU and RAM share per minute**: `GET /v1/usage/nodes/{host}` rows (the mean
//!   CPU and the largest working set of each minute of a node) over the host's `shape` in
//!   `GET /v1/hosts`.
//! - **queue wait**: the job's planned submit time to the first poll that saw it
//!   `JOB_STATE_RUNNING`. A job is submitted at the first poll at or after its planned time
//!   and seen running at a later poll, so this is good to two poll intervals (one on each
//!   end), and a job that was over before a poll saw it running has none.
//! - **minutes**: the usage store buckets by the server's clock and this harness
//!   buckets its windows by its own. They are assumed to be in sync (the same host, or
//!   hosts under time sync); skew moves usage into a neighbouring minute.
//! - **submit to placed**: the job submitted to the first poll that saw it with a host.
//! - **makespan**: the first submit to the last poll that saw a job over; not available
//!   when any job has no finish (it failed or was given up).
//! - **booked vs used**: the job's CPU and memory over its placed time against the host's
//!   usage rows. A job that asked for a share books what it asked for; a job that asked for
//!   the whole host books the host's shape. The rows are per node, not per job, so a window
//!   counts only when the job was the only one on the host in every minute it overlaps.
//!
//! The run time and memory a cost curve predicts, and the ratios of observed to predicted, are
//! not measured: the job API carries no profile name, so no cost curve is learned for a job
//! (`GET /v1/usage/keys/{profile}` has nothing keyed by a job), and a predicted run time or
//! memory cannot be set against the observed ones.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::driver::{JobTiming, Observation};
use crate::farm::{HostShape, UsageRow};
use crate::metric::{Record, finite, name};
use crate::reported;
use crate::workload::Job;

/// Milliseconds in a minute of the usage store.
pub const MINUTE_MS: u64 = 60_000;

/// CPU and memory a window of a host used while one job held it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowUsage {
    /// CPU used, in thousandths of a core times milliseconds.
    pub cpu_milli_ms: u64,
    /// The largest working set of any minute of the window, in bytes.
    pub peak_ws_bytes: u64,
}

/// `part` over `whole`, `None` when `whole` is zero.
#[must_use]
pub fn share(part: u64, whole: u64) -> Option<f64> {
    (whole > 0).then_some(part as f64 / whole as f64)
}

/// `numerator` over `denominator`, `None` when either is missing or the divisor is not
/// above zero.
fn ratio(numerator: Option<f64>, denominator: Option<f64>) -> Option<f64> {
    let (n, d) = (numerator?, denominator?);
    (d > 0.0).then_some(n / d)
}

/// The mean of the values, `None` for none.
fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

/// The largest of the values, `None` for none.
fn largest(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::max)
}

/// `later - earlier` in milliseconds, `None` when either is missing or they are in the
/// wrong order.
fn gap(later: Option<u64>, earlier: Option<u64>) -> Option<f64> {
    Some(later?.checked_sub(earlier?)? as f64)
}

/// The usage of the window `[from_ms, to_ms)`: each minute it overlaps contributes its
/// mean CPU times the overlap, and the window's peak is the largest working set of those
/// minutes. `None` when the window is empty or when any of its minutes has no row or a
/// row that more than one job shared.
#[must_use]
pub fn window_usage(rows: &[UsageRow], from_ms: u64, to_ms: u64) -> Option<WindowUsage> {
    if to_ms <= from_ms {
        return None;
    }
    let mut usage = WindowUsage {
        cpu_milli_ms: 0,
        peak_ws_bytes: 0,
    };
    for minute in from_ms / MINUTE_MS..=(to_ms - 1) / MINUTE_MS {
        let row = rows
            .iter()
            .find(|row| row.minute == minute && row.leases == 1)?;
        let overlap = to_ms.min((minute + 1) * MINUTE_MS) - from_ms.max(minute * MINUTE_MS);
        usage.cpu_milli_ms += row.cpu_millis * overlap;
        usage.peak_ws_bytes = usage.peak_ws_bytes.max(row.ws_bytes);
    }
    Some(usage)
}

/// Everything known of one job, each a number or missing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobFacts {
    /// Planned submit to the first poll that saw the job running, in milliseconds.
    pub queue_wait_ms: Option<f64>,
    /// Submit to the first poll that saw the job placed, in milliseconds.
    pub submit_to_placed_ms: Option<f64>,
    /// First poll that saw it running to the poll that saw it over, in milliseconds.
    pub runtime_ms: Option<f64>,
    /// The largest working set of the host over the job's window, in bytes.
    pub peak_ws_bytes: Option<f64>,
    /// CPU booked over the placed time, in thousandths of a core times milliseconds.
    pub booked_cpu_milli_ms: Option<f64>,
    /// CPU the host used over the placed time, same unit.
    pub used_cpu_milli_ms: Option<f64>,
    /// Memory booked, in bytes.
    pub booked_memory_bytes: Option<f64>,
}

impl JobFacts {
    /// The facts of one job, placed on a host of `shape` (when it could be read) with the
    /// host's `usage` over its window.
    #[must_use]
    pub fn of(
        timing: &JobTiming,
        job: &Job,
        shape: Option<HostShape>,
        usage: Option<&WindowUsage>,
    ) -> Self {
        let held_ms = gap(timing.finished_ms, timing.placed_ms);
        let (cpu_millis, memory_bytes) = match job.ask.whole_host {
            true => (
                shape.map(|shape| shape.cpu_millis),
                shape.map(|shape| shape.memory_bytes),
            ),
            false => (Some(job.ask.cpu_millis), Some(job.ask.memory_bytes)),
        };
        Self {
            queue_wait_ms: gap(timing.started_ms, Some(timing.submit_at_ms)),
            submit_to_placed_ms: gap(timing.placed_ms, timing.requested_ms),
            runtime_ms: gap(timing.finished_ms, timing.started_ms),
            peak_ws_bytes: usage.map(|u| u.peak_ws_bytes as f64),
            booked_cpu_milli_ms: held_ms.zip(cpu_millis).map(|(ms, cpu)| cpu as f64 * ms),
            used_cpu_milli_ms: usage.map(|u| u.cpu_milli_ms as f64),
            booked_memory_bytes: memory_bytes.map(|bytes| bytes as f64),
        }
    }

    /// The facts as the metrics of a `jobs.jsonl` row.
    #[must_use]
    pub fn metrics(&self) -> BTreeMap<String, Option<f64>> {
        [
            ("queue_wait_ms", self.queue_wait_ms),
            ("submit_to_placed_ms", self.submit_to_placed_ms),
            ("runtime_ms", self.runtime_ms),
            ("peak_ws_bytes", self.peak_ws_bytes),
            ("booked_cpu_milli_ms", self.booked_cpu_milli_ms),
            ("used_cpu_milli_ms", self.used_cpu_milli_ms),
            ("booked_memory_bytes", self.booked_memory_bytes),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), finite(value)))
        .collect()
    }
}

/// What the harness read of one host after the run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostReading {
    /// The host's size, when it could be read.
    pub shape: Option<HostShape>,
    /// The usage rows of the run's minutes, when they could be read.
    pub rows: Option<Vec<UsageRow>>,
}

/// Whose run it is: the identity every output row carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The setting under test.
    pub setting: String,
    /// The arm.
    pub arm: String,
    /// Place in the executed order.
    pub run_index: u32,
    /// The seed of the order.
    pub seed: u64,
    /// The commit under test.
    pub commit: String,
    /// `"warmup"` for a warm-up run, else empty.
    pub label: String,
}

/// One job of a run: one line of `jobs.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobRow {
    /// The setting under test.
    pub setting: String,
    /// The arm.
    pub arm: String,
    /// Place in the executed order.
    pub run_index: u32,
    /// The job's name.
    pub job: String,
    /// The host it was placed on.
    pub host: Option<String>,
    /// How it ended.
    pub outcome: Option<String>,
    /// Why it did not get through.
    pub failure: Option<String>,
    /// The job's facts.
    pub metrics: BTreeMap<String, Option<f64>>,
}

/// One minute of one host: one line of `minutes.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MinuteRow {
    /// The setting under test.
    pub setting: String,
    /// The arm.
    pub arm: String,
    /// Place in the executed order.
    pub run_index: u32,
    /// The host.
    pub host: String,
    /// Minutes since the epoch.
    pub minute: u64,
    /// CPU in use over the host's CPU shape.
    pub cpu_share: Option<f64>,
    /// Working set over the host's memory shape.
    pub ram_share: Option<f64>,
    /// Allocations that ran on the host in the minute, as the usage row counts them.
    pub leases: u64,
}

/// The three outputs of one run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutput {
    /// The run's record.
    pub record: Record,
    /// One row per job.
    pub jobs: Vec<JobRow>,
    /// One row per host-minute.
    pub minutes: Vec<MinuteRow>,
}

fn is_ok(outcome: Option<&String>) -> bool {
    outcome.is_some_and(|outcome| outcome == "succeeded")
}

/// Everything a run measured, from what the drive saw (`obs`, one entry per `jobs`) and
/// the host readings by host id.
#[must_use]
pub fn assemble(
    who: &Identity,
    obs: &Observation,
    jobs: &[Job],
    hosts: &BTreeMap<String, HostReading>,
) -> RunOutput {
    let facts: Vec<JobFacts> = obs
        .jobs
        .iter()
        .zip(jobs)
        .map(|(timing, job)| {
            let reading = timing.host.as_ref().and_then(|host| hosts.get(host));
            let usage = reading.and_then(|reading| {
                window_usage(
                    reading.rows.as_ref()?,
                    timing.placed_ms?,
                    timing.finished_ms?,
                )
            });
            JobFacts::of(
                timing,
                job,
                reading.and_then(|reading| reading.shape),
                usage.as_ref(),
            )
        })
        .collect();
    let reports: Vec<BTreeMap<String, Option<f64>>> = obs
        .jobs
        .iter()
        .map(|timing| {
            timing
                .log_tail
                .as_ref()
                .map_or_else(BTreeMap::new, |tail| reported::parse(&tail.stdout))
        })
        .collect();
    let job_rows = obs
        .jobs
        .iter()
        .zip(&facts)
        .zip(&reports)
        .map(|((timing, facts), report)| JobRow {
            setting: who.setting.clone(),
            arm: who.arm.clone(),
            run_index: who.run_index,
            job: timing.name.clone(),
            host: timing.host.clone(),
            outcome: timing.outcome.clone(),
            failure: timing.failure.clone(),
            metrics: facts
                .metrics()
                .into_iter()
                .chain(reported::prefixed(report))
                .collect(),
        })
        .collect();
    let minutes = minute_rows(who, hosts);
    let record = Record {
        setting: who.setting.clone(),
        arm: who.arm.clone(),
        run_index: who.run_index,
        seed: who.seed,
        commit: who.commit.clone(),
        label: who.label.clone(),
        metrics: run_metrics(obs, &facts, &minutes, &reports),
    };
    RunOutput {
        record,
        jobs: job_rows,
        minutes,
    }
}

fn minute_rows(who: &Identity, hosts: &BTreeMap<String, HostReading>) -> Vec<MinuteRow> {
    let mut out = Vec::new();
    for (host, reading) in hosts {
        for row in reading.rows.iter().flatten() {
            let shape = reading.shape;
            out.push(MinuteRow {
                setting: who.setting.clone(),
                arm: who.arm.clone(),
                run_index: who.run_index,
                host: host.clone(),
                minute: row.minute,
                cpu_share: finite(shape.and_then(|s| share(row.cpu_millis, s.cpu_millis))),
                ram_share: finite(shape.and_then(|s| share(row.ws_bytes, s.memory_bytes))),
                leases: row.leases,
            });
        }
    }
    out
}

fn values(metrics: impl Iterator<Item = Option<f64>>) -> Vec<f64> {
    metrics.flatten().collect()
}

fn run_metrics(
    obs: &Observation,
    facts: &[JobFacts],
    minutes: &[MinuteRow],
    reports: &[BTreeMap<String, Option<f64>>],
) -> BTreeMap<String, Option<f64>> {
    let some = |pick: fn(&JobFacts) -> Option<f64>| -> Vec<f64> {
        facts.iter().filter_map(pick).collect()
    };
    let queue = some(|f| f.queue_wait_ms);
    let placed = some(|f| f.submit_to_placed_ms);
    let cpu = values(minutes.iter().map(|m| m.cpu_share));
    let ram = values(minutes.iter().map(|m| m.ram_share));
    let first = obs.jobs.iter().filter_map(|j| j.requested_ms).min();
    // Every job must have finished: a failed or given-up job would otherwise shorten it.
    let last = obs
        .jobs
        .iter()
        .map(|j| j.finished_ms)
        .collect::<Option<Vec<u64>>>()
        .and_then(|all| all.into_iter().max());
    let both: Vec<&JobFacts> = facts
        .iter()
        .filter(|f| f.booked_cpu_milli_ms.is_some() && f.used_cpu_milli_ms.is_some())
        .collect();
    let with_peak: Vec<&JobFacts> = facts
        .iter()
        .filter(|f| f.peak_ws_bytes.is_some() && f.booked_memory_bytes.is_some())
        .collect();
    let sum = |of: &[&JobFacts], pick: fn(&JobFacts) -> Option<f64>| -> Option<f64> {
        Some(of.iter().filter_map(|f| pick(f)).sum())
    };
    let failed = obs
        .jobs
        .iter()
        .filter(|j| !is_ok(j.outcome.as_ref()))
        .count();
    let kills = obs
        .jobs
        .iter()
        .filter(|j| j.outcome.as_deref() == Some("out_of_memory"))
        .count();
    // Only a job that succeeded speaks for the run: a wrong answer's numbers are not the
    // benchmark's. What a failed job printed is counted, and stays in its own row. A job
    // that succeeded and printed no line stays in the merge, so it lacks every key and no
    // reported number is recorded; it is counted too.
    let (ok, failed_jobs): (Vec<_>, Vec<_>) = obs
        .jobs
        .iter()
        .zip(reports)
        .partition(|(job, _)| is_ok(job.outcome.as_ref()));
    let kept: Vec<_> = ok.iter().map(|(_, report)| (*report).clone()).collect();
    let missing = ok.iter().filter(|(_, report)| report.is_empty()).count();
    let dropped = failed_jobs
        .iter()
        .filter(|(_, report)| !report.is_empty())
        .count();
    let mut out = reported::merge(&kept);
    for (key, count) in [
        (name::REPORTS_DROPPED, dropped),
        (name::REPORTS_MISSING, missing),
    ] {
        out.insert(key.to_owned(), Some(count as f64));
    }
    let mut put = |key: &str, value: Option<f64>| {
        out.insert(key.to_owned(), finite(value));
    };
    put(name::MAKESPAN_MS, gap(last, first));
    put(name::JOBS, Some(facts.len() as f64));
    put(name::JOBS_FAILED, Some(failed as f64));
    put(name::MEMORY_KILLS, Some(kills as f64));
    put(name::QUEUE_WAIT_MS_MEAN, mean(&queue));
    put(name::QUEUE_WAIT_MS_MAX, largest(&queue));
    put(name::SUBMIT_TO_PLACED_MS_MEAN, mean(&placed));
    put(name::SUBMIT_TO_PLACED_MS_MAX, largest(&placed));
    put(name::HOST_CPU_SHARE_MEAN, mean(&cpu));
    put(name::HOST_CPU_SHARE_MAX, largest(&cpu));
    put(name::HOST_RAM_SHARE_MEAN, mean(&ram));
    put(name::HOST_RAM_SHARE_MAX, largest(&ram));
    put(
        name::CPU_USED_OVER_BOOKED,
        ratio(
            sum(&both, |f| f.used_cpu_milli_ms),
            sum(&both, |f| f.booked_cpu_milli_ms),
        ),
    );
    put(
        name::MEMORY_USED_OVER_BOOKED,
        ratio(
            sum(&with_peak, |f| f.peak_ws_bytes),
            sum(&with_peak, |f| f.booked_memory_bytes),
        ),
    );
    out
}
