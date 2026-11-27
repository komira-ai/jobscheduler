//! What `/metrics` reads of the placement at each scrape: the scheduler's admission counters,
//! how each host is charged ([`HostLoad`]), how the runs freed while serving did against what
//! they booked ([`RunFit`]) and how the checks of those runs against their classes' profiles
//! came out ([`CurveChecks`]). All of it is plain data, taken under the engine's lock by
//! [`crate::engine::Engine::telemetry`] and rendered by the caller without it.
//!
//! The fit and the checks count the runs the passes free while the server is serving; a run
//! read from the job store's history at the server's start is not counted, so every figure
//! here starts at zero with the server and only grows.

use std::collections::BTreeMap;

use js_jobs::JobResult;
use js_sizing::{Outcome, Run};
use js_types::HostId;

/// The upper bounds of the buckets of a [`RatioHistogram`], in thousandths: a quarter, a half,
/// three quarters, one, one and a quarter, one and a half, two, three and five times.
pub const FIT_BOUNDS_PERMILLE: [u64; 9] = [250, 500, 750, 1000, 1250, 1500, 2000, 3000, 5000];

/// A histogram of a ratio, in thousandths, over the buckets [`FIT_BOUNDS_PERMILLE`] bounds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RatioHistogram {
    /// The observations of each bucket, not cumulative: `counts[i]`, for `i` below 9, holds
    /// those at most [`FIT_BOUNDS_PERMILLE`]`[i]` and above the bound before it (above 0 for
    /// the first); `counts[9]` holds those above the last bound.
    pub counts: [u64; 10],
    /// The sum of the observations, in thousandths; it stops at `u64::MAX`.
    pub sum_permille: u64,
}

impl RatioHistogram {
    /// Takes one observation of `permille` thousandths into its bucket and its sum.
    pub fn observe(&mut self, permille: u64) {
        let bucket = FIT_BOUNDS_PERMILLE.partition_point(|bound| *bound < permille);
        self.counts[bucket] += 1;
        self.sum_permille = self.sum_permille.saturating_add(permille);
    }

    /// How many observations the histogram holds.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.counts.iter().sum()
    }
}

/// What the runs freed while serving did against what they booked. Only successful runs give
/// a ratio.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFit {
    /// A run's mean CPU over its booked CPU (`k`): its CPU time over its wall time, over the
    /// cores it booked. A run under a second has no CPU sample ([`js_sizing::MIN_WALL_US`]: the
    /// estimator takes its CPU time as none), nor has one that used no CPU time or booked
    /// none.
    pub cpu_mean_over_booked: RatioHistogram,
    /// A run's CPU peak over its mean CPU (`r`): the most cores its group used over the
    /// agent's window, over its mean. A run with no reported peak or no mean has none.
    pub cpu_peak_over_mean: RatioHistogram,
    /// A run's peak memory over its booked memory (`k_m`). A run with no measured peak or no
    /// booked memory has none.
    pub memory_peak_over_booked: RatioHistogram,
    /// The runs that ended killed at their own memory limit.
    pub memory_kills: u64,
}

impl RunFit {
    /// Takes the run `run` of `result`, which booked `booked_cpu_millis` of CPU and ended as
    /// `run.outcome` says: a memory kill is counted, and a success gives its ratios.
    pub(crate) fn record(&mut self, run: &Run, result: &JobResult, booked_cpu_millis: u64) {
        self.memory_kills += u64::from(run.outcome == Outcome::OwnLimitKill);
        if run.outcome != Outcome::Success {
            return;
        }
        let cpu = u128::from(run.cpu_usage_us);
        let wall = u128::from(run.wall_us);
        let mean_millis = cpu * 1000 / wall.max(1);
        let mean = ratio(cpu * 1_000_000, wall * u128::from(booked_cpu_millis));
        let peak = result
            .usage
            .and_then(|usage| usage.cpu_peak)
            .and_then(|peak| ratio(u128::from(peak.millicores) * 1000, mean_millis));
        let memory = run
            .peak_memory_bytes
            .and_then(|peak| ratio(u128::from(peak) * 1000, u128::from(run.booked_memory_bytes)));
        mean.filter(|_| cpu > 0)
            .into_iter()
            .for_each(|k| self.cpu_mean_over_booked.observe(k));
        peak.into_iter()
            .for_each(|r| self.cpu_peak_over_mean.observe(r));
        memory
            .into_iter()
            .for_each(|k| self.memory_peak_over_booked.observe(k));
    }
}

/// `numerator` over `denominator`, rounded down and held at `u64::MAX`; `None` over 0.
fn ratio(numerator: u128, denominator: u128) -> Option<u64> {
    let quotient = numerator.checked_div(denominator)?;
    Some(u64::try_from(quotient).unwrap_or(u64::MAX))
}

/// The checks of the runs freed while serving against their class's profile, for one
/// resource, each run counted under the status of its check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckCounts {
    /// Runs the profile checked and learned.
    pub learned: u64,
    /// Runs the profile checked and chose not to learn.
    pub not_learned: u64,
    /// Runs whose host recorded no series of the resource.
    pub no_series: u64,
    /// Runs whose series misses the start of the run.
    pub partial: u64,
    /// Runs whose CPU series disagrees with the run's exact CPU time.
    pub disagrees: u64,
    /// Runs that were no success, which the profile never checks.
    pub not_a_success: u64,
    /// Checked runs that overran their predicted peak: the profile had a prediction for the
    /// run (it held enough runs to score it) and the run's peak was more than a quarter above
    /// it ([`js_sizing::OVERRUN_PERMILLE`]). Counted besides the run's status.
    pub overran: u64,
}

impl CheckCounts {
    /// Counts one run whose check came out as `status` (the words of the log line:
    /// `learned`, `not-learned`, `no-series`, `partial`, `disagrees`; any other is
    /// `not-a-success`), and as an overrun when `overran`.
    pub(crate) fn count(&mut self, status: &str, overran: bool) {
        let counter = match status {
            "learned" => &mut self.learned,
            "not-learned" => &mut self.not_learned,
            "no-series" => &mut self.no_series,
            "partial" => &mut self.partial,
            "disagrees" => &mut self.disagrees,
            _ => &mut self.not_a_success,
        };
        *counter += 1;
        self.overran += u64::from(overran);
    }
}

/// The checks of the runs freed while serving, for each resource.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CurveChecks {
    /// The checks of the CPU profile.
    pub cpu: CheckCounts,
    /// The checks of the memory profile.
    pub memory: CheckCounts,
}

/// One host's load as the scheduler charges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostLoad {
    /// The CPU the host offers, in millicores.
    pub cpu_capacity_millis: u64,
    /// The memory the host offers for booking, in bytes: its memory cut to the share the
    /// scheduler books.
    pub memory_capacity_bytes: u64,
    /// How the scheduler charges the host now.
    pub load: js_sched::LoadView,
}

/// What `/metrics` reads of the placement at each scrape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Telemetry {
    /// What admission has done since the scheduler was built.
    pub counters: js_sched::LoadCounters,
    /// How each host that is a worker of the scheduler is charged; a host the engine keeps
    /// with no worker is absent.
    pub hosts: BTreeMap<HostId, HostLoad>,
    /// How the runs freed while serving did against what they booked.
    pub fit: RunFit,
    /// The checks of those runs against their classes' profiles; all zero with
    /// [`crate::sizes::Curves::Off`].
    pub checks: CurveChecks,
}

#[cfg(test)]
#[path = "telemetry_tests.rs"]
mod tests;
