//! The profiles a class learns in shadow (`docs/design/cpu-over-time.md`, step 2): for each
//! class, a CPU profile and a memory profile of the series its successful runs recorded
//! ([`js_sizing::Profile`]).
//!
//! Each run the history observes is checked against its class's profiles as they stand, then
//! learned, and a run freed while serving (not one read from the history at the start) logs one
//! line with what the profiles predicted and what the run did. Nothing that books a job reads
//! a profile: the sizes are the same with [`Curves::Off`] and [`Curves::Shadow`].
//!
//! A run teaches its class's CPU profile when it succeeded, its CPU series is whole (it does not
//! miss the run's start), the series agrees with the run's exact CPU time, and the run was not
//! slowed by contention or held at a ceiling, or was and used more than the class's level (a
//! slowed run may only raise its class). It teaches the memory profile when it succeeded and
//! its memory series is whole. A class's profiles are kept for at most [`MAX_PROFILES`] classes.
//! A restart rebuilds them from the job records the store keeps.

use std::collections::BTreeMap;

use js_jobs::{JobRecord, JobResult};
use js_sizing::{
    CPU_PRESSURE_MAX_PERMILLE, CPU_UNIT, Checked, Key, MEMORY_UNIT, Outcome, Profile, Run,
    RunSeries, series_agrees,
};
use tracing::info;

use super::{JobSizes, class_name, micros, millis};

/// The most classes whose profiles are kept: a class that starts beyond it evicts the class
/// whose last run ended earliest.
pub const MAX_PROFILES: usize = 2048;

/// Whether the history learns profiles: the server's `--placement-curves`. Placement is the same
/// in both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Curves {
    /// Learns nothing and logs nothing.
    #[default]
    Off,
    /// Learns a CPU and a memory profile for each class and logs one line for each run it
    /// checks while serving.
    Shadow,
}

impl Curves {
    /// The word the server's flag and file give this mode: `off` or `shadow`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
        }
    }

    /// The mode `word` names: exactly `off` or `shadow`; `None` for any other.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "off" => Some(Self::Off),
            "shadow" => Some(Self::Shadow),
            _ => None,
        }
    }
}

/// What one class has learned: a profile of its CPU and one of its memory, and when its latest
/// run ended.
#[derive(Debug)]
pub(super) struct ClassProfile {
    /// The CPU profile, in sixteenths of a core.
    cpu: Profile,
    /// The memory profile, in MiB.
    memory: Profile,
    /// The end of the latest run taught or checked, in milliseconds since the epoch.
    last_ended_ms: u64,
}

impl ClassProfile {
    /// A class that has seen no run.
    fn new() -> Self {
        Self {
            cpu: Profile::new(CPU_UNIT),
            memory: Profile::new(MEMORY_UNIT),
            last_ended_ms: 0,
        }
    }
}

/// The profiles of every class, at most [`MAX_PROFILES`] of them.
#[derive(Debug, Default)]
pub(super) struct Profiles {
    classes: BTreeMap<Key, ClassProfile>,
}

/// Makes room in `classes` for `key` under `cap` classes: when `key` is new and the map is full,
/// removes the class whose last run ended earliest (the smallest key of those that tie).
fn evict_for(classes: &mut BTreeMap<Key, ClassProfile>, key: &Key, cap: usize) {
    if classes.contains_key(key) | (classes.len() < cap) {
        return;
    }
    let oldest = classes
        .iter()
        .min_by_key(|(_, class)| class.last_ended_ms)
        .map(|(oldest, _)| oldest.clone());
    if let Some(oldest) = oldest {
        classes.remove(&oldest);
    }
}

/// A series as a result records it, with its points in the profile's unit.
struct Read {
    granularity_secs: u32,
    points: Vec<u32>,
    run_usec: u64,
    partial: bool,
}

/// What one dimension (CPU or memory) of a run came to.
#[derive(Debug, Clone, Copy)]
struct Part {
    /// `learned`, `not-learned`, `no-series`, `partial`, `disagrees` or `not-a-success`.
    status: &'static str,
    /// What the check found; `None` when the run was not checked.
    checked: Option<Checked>,
}

impl Part {
    /// A dimension that was not checked, for `status`.
    const fn skipped(status: &'static str) -> Self {
        Self {
            status,
            checked: None,
        }
    }
}

/// What a dimension logs: each field of its check, in the unit of the log, absent when the run
/// was not checked or the profile had nothing to predict from.
struct Shown {
    status: &'static str,
    runs: Option<usize>,
    shaped: Option<bool>,
    mean: Option<u64>,
    level: Option<u64>,
    peak: Option<u64>,
    predicted_peak: Option<u64>,
    curve_error: Option<u64>,
    flat_error: Option<u64>,
    fit_permille: Option<u64>,
}

impl Shown {
    /// `part` with each value in the unit `scale` converts the profile's to.
    fn of(part: Part, scale: fn(u64) -> u64) -> Self {
        let checked = part.checked;
        let value = |sixteenths: u32| scale(u64::from(sixteenths));
        Self {
            status: part.status,
            runs: checked.map(|checked| checked.runs),
            shaped: checked.map(|checked| checked.shaped),
            mean: checked.map(|checked| value(checked.actual_mean)),
            level: checked
                .and_then(|checked| checked.predicted_level)
                .map(value),
            peak: checked.map(|checked| value(checked.actual_peak)),
            predicted_peak: checked
                .and_then(|checked| checked.predicted_peak)
                .map(value),
            curve_error: checked.and_then(|checked| checked.curve_error).map(scale),
            flat_error: checked.and_then(|checked| checked.flat_error).map(scale),
            fit_permille: checked.and_then(|checked| checked.window.fit_permille()),
        }
    }
}

/// A CPU value in sixteenths of a core as millicores.
fn millicores(sixteenths: u64) -> u64 {
    sixteenths.saturating_mul(1000) / 16
}

/// A memory value, which the log gives in the profile's own unit (MiB).
fn mebibytes(value: u64) -> u64 {
    value
}

/// Whether a CPU series agrees with the run's exact CPU time, in microseconds; a run with no
/// usage has none to agree with.
fn cpu_agrees(series: RunSeries<'_>, usage_usec: Option<u64>) -> bool {
    usage_usec.is_some_and(|usec| series_agrees(series, usec))
}

/// A memory series has no exact figure to agree with.
fn memory_agrees(_: RunSeries<'_>, _: Option<u64>) -> bool {
    true
}

/// Checks `read` against `profile`, then learns it as `clean` or not (`ended_ms` is when the run
/// ended), unless the series is missing, partial or disagrees with `usage_usec`.
fn part(
    profile: &mut Profile,
    read: Option<&Read>,
    (ended_ms, clean): (u64, bool),
    usage_usec: Option<u64>,
    agrees: fn(RunSeries<'_>, Option<u64>) -> bool,
) -> Part {
    let Some(read) = read else {
        return Part::skipped("no-series");
    };
    if read.partial {
        return Part::skipped("partial");
    }
    let series = RunSeries {
        granularity_secs: read.granularity_secs,
        points: &read.points,
        run_usec: read.run_usec,
    };
    if !agrees(series, usage_usec) {
        return Part::skipped("disagrees");
    }
    let checked = profile.check_and_learn(series, ended_ms, clean);
    let status = if checked.learned {
        "learned"
    } else {
        "not-learned"
    };
    Part {
        status,
        checked: Some(checked),
    }
}

impl Profiles {
    /// The profiles of the class `key`, made when it has none (and room made first); its latest
    /// run ended at `ended_ms` or later.
    fn class_mut(&mut self, key: Key, ended_ms: u64) -> &mut ClassProfile {
        evict_for(&mut self.classes, &key, MAX_PROFILES);
        let class = self.classes.entry(key).or_insert_with(ClassProfile::new);
        class.last_ended_ms = class.last_ended_ms.max(ended_ms);
        class
    }

    /// Checks and learns both dimensions of the successful run of `job` that `result` records.
    fn check(&mut self, job: &JobRecord, result: &JobResult, run: &Run) -> (Part, Part) {
        let ended_ms = millis(result.timing.ended_at.as_duration());
        let class = self.class_mut(JobSizes::key_of(job), ended_ms);
        // A run slowed by contention or held at a ceiling may only raise its class.
        let quiet = run
            .node_cpu_pressure_permille
            .is_some_and(|permille| permille < CPU_PRESSURE_MAX_PERMILLE);
        let clean = quiet & !run.cpu_held;
        let cpu = result.cpu_series.as_ref().map(|series| Read {
            granularity_secs: series.granularity_secs,
            points: series
                .points
                .iter()
                .map(|point| u32::from(*point))
                .collect(),
            run_usec: series.run_usec,
            partial: series.partial,
        });
        let memory = result.memory_series.as_ref().map(|series| Read {
            granularity_secs: series.granularity_secs,
            points: series.points.clone(),
            run_usec: series.run_usec,
            partial: series.partial,
        });
        let usage_usec = result.usage.map(|usage| micros(usage.cpu_time));
        let cpu = part(
            &mut class.cpu,
            cpu.as_ref(),
            (ended_ms, clean),
            usage_usec,
            cpu_agrees,
        );
        let memory = part(
            &mut class.memory,
            memory.as_ref(),
            (ended_ms, true),
            None,
            memory_agrees,
        );
        (cpu, memory)
    }

    /// Checks and learns the run of `job` that `result` records, `run` as the estimator takes
    /// it; logs the check when `log` is set.
    fn feed(&mut self, job: &JobRecord, result: &JobResult, run: &Run, log: bool) {
        let (cpu, memory) = if run.outcome == Outcome::Success {
            self.check(job, result, run)
        } else {
            (
                Part::skipped("not-a-success"),
                Part::skipped("not-a-success"),
            )
        };
        if log {
            let class = class_name(job);
            let cpu = Shown::of(cpu, millicores);
            let memory = Shown::of(memory, mebibytes);
            info!(
                job = %job.id,
                %class,
                cpu = cpu.status,
                cpu_runs = cpu.runs,
                cpu_shaped = cpu.shaped,
                cpu_mean_millicores = cpu.mean,
                cpu_level_millicores = cpu.level,
                cpu_peak_millicores = cpu.peak,
                cpu_predicted_peak_millicores = cpu.predicted_peak,
                cpu_curve_error = cpu.curve_error,
                cpu_flat_error = cpu.flat_error,
                cpu_fit_permille = cpu.fit_permille,
                memory = memory.status,
                memory_runs = memory.runs,
                memory_shaped = memory.shaped,
                memory_mean_mib = memory.mean,
                memory_level_mib = memory.level,
                memory_peak_mib = memory.peak,
                memory_predicted_peak_mib = memory.predicted_peak,
                memory_curve_error = memory.curve_error,
                memory_flat_error = memory.flat_error,
                memory_fit_permille = memory.fit_permille,
                "a run was checked against its class's profile"
            );
        }
    }
}

impl JobSizes {
    /// Checks the run of `job`, `run` as the estimator takes it, against its class's profiles and
    /// learns it, when profiles are kept; logs the check when `log` is set. A job with no result
    /// has no run.
    pub(super) fn profile_run(&mut self, job: &JobRecord, run: &Run, log: bool) {
        let Some(profiles) = self.profiles.as_mut() else {
            return;
        };
        job.result
            .iter()
            .for_each(|result| profiles.feed(job, result, run, log));
    }
}

#[cfg(test)]
#[path = "sizes_profile_tests.rs"]
mod tests;
