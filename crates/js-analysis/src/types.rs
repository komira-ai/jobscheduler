//! The plain types of the input and the report. All of them serialise to JSON.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The label that marks a warm-up run. A warm-up is kept in the report, listed with its
/// label, and left out of every statistic.
pub const WARMUP: &str = "warmup";

/// The largest number of runs per arm that [`crate::min_runs`] looks for.
pub const MAX_PLAN_RUNS: usize = 200;

/// One run of one arm: the line a measurement crate emits (JSON lines, one record per line).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    /// The value of the varied parameter (for example a fill ratio), as the text the
    /// measurement used. Records with the same text are the same setting.
    pub setting: String,
    /// The arm: what was run (for example `legacy + MinIO`).
    pub arm: String,
    /// The index of the run within its setting and arm. Unique there.
    pub run_index: u32,
    /// The seed the run used.
    pub seed: u64,
    /// The build under test.
    pub commit: String,
    /// `warmup` for a warm-up run, any other text (or none) for a measured run.
    #[serde(default)]
    pub label: String,
    /// The measured metrics by name. `null` (or an absent name) means not available: the
    /// run produced no value, which is not zero and is never filled in.
    pub metrics: BTreeMap<String, Option<f64>>,
}

/// A closed interval.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Interval {
    /// The lower end.
    pub lo: f64,
    /// The upper end.
    pub hi: f64,
}

/// The summary of the measured values of one metric of one group.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Summary {
    /// The arithmetic mean.
    pub mean: f64,
    /// The median (the mean of the two middle values for an even count).
    pub median: f64,
    /// The sample standard deviation (divisor n-1); none for one value.
    pub std: Option<f64>,
    /// The smallest value.
    pub min: f64,
    /// The largest value.
    pub max: f64,
    /// std/mean; none for one value or a mean that is not positive.
    pub cv: Option<f64>,
    /// The 95% t interval of the mean; none for one value.
    pub mean_ci: Option<Interval>,
    /// The 95% interval of std/mean (modified McKay); none when `cv` is none or the
    /// approximation has no upper end.
    pub cv_ci: Option<Interval>,
}

/// How many runs per arm resolve a relative effect at the spread of this metric.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunPlan {
    /// The relative effect to resolve (0.05 is 5%).
    pub effect: f64,
    /// The smallest n at which the expected 95% half-width of the difference is at most
    /// the effect; none when it is above [`MAX_PLAN_RUNS`].
    pub min_runs_ci: Option<usize>,
    /// The smallest n for about 80% power to see the effect; none above [`MAX_PLAN_RUNS`].
    pub min_runs_power80: Option<usize>,
}

/// One metric of one group.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MetricReport {
    /// The metric's name.
    pub metric: String,
    /// The measured runs that have a value.
    pub n: usize,
    /// The measured runs that have none (not available).
    pub n_missing: usize,
    /// The statistics; none when no measured run has a value.
    pub summary: Option<Summary>,
    /// The run counts for the requested effects; empty when std/mean is none.
    pub plans: Vec<RunPlan>,
}

/// All runs of one arm at one setting.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GroupReport {
    /// The setting.
    pub setting: String,
    /// The arm.
    pub arm: String,
    /// The measured (not warm-up) runs.
    pub n_runs: usize,
    /// The warm-up runs.
    pub n_warmup: usize,
    /// Every run of the group, in input order, warm-ups included.
    pub runs: Vec<RunRecord>,
    /// One report per metric named in any run of the group, by name.
    pub metrics: Vec<MetricReport>,
}

impl GroupReport {
    /// The measured values of `metric` in run order: warm-ups and missing values left out.
    #[must_use]
    pub fn values(&self, metric: &str) -> Vec<f64> {
        self.runs
            .iter()
            .filter(|r| r.label != WARMUP)
            .filter_map(|r| r.metrics.get(metric).copied().flatten())
            .collect()
    }
}

/// A named set of values, one side of a comparison.
#[derive(Clone, Copy, Debug)]
pub struct Pair<'a> {
    /// The arm's name.
    pub name: &'a str,
    /// The measured values.
    pub values: &'a [f64],
}

/// How an exact or sampled permutation test was run.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermMethod {
    /// Every split was enumerated.
    Exact,
    /// A fixed number of random splits from a fixed seed.
    MonteCarlo,
}

impl PermMethod {
    /// The name used in JSON and in the table.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::MonteCarlo => "monte-carlo",
        }
    }
}

/// A two-sided permutation test of the difference of means.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Permutation {
    /// The p-value.
    pub p: f64,
    /// Exact enumeration or sampling.
    pub method: PermMethod,
    /// The splits counted: all of them (exact) or the sample size (monte-carlo).
    pub splits: u64,
}

/// What a comparison concludes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// The interval excludes zero.
    Differs,
    /// The interval contains zero: the data do not tell the arms apart.
    NotDistinguished,
    /// A side has fewer than two values, so there is no interval.
    NotEstimable,
}

impl Verdict {
    /// The name used in JSON and in the table.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Differs => "differs",
            Self::NotDistinguished => "not-distinguished",
            Self::NotEstimable => "not-estimable",
        }
    }
}

/// One arm against a base arm on one metric at one setting (Welch).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Comparison {
    /// The setting both arms ran at.
    pub setting: String,
    /// The metric.
    pub metric: String,
    /// The base arm.
    pub base: String,
    /// The arm compared to it.
    pub arm: String,
    /// Values of the base.
    pub n_base: usize,
    /// Values of the arm.
    pub n_arm: usize,
    /// mean(arm) - mean(base); none when a side has no value.
    pub diff: Option<f64>,
    /// The 95% Welch interval of `diff`; none when a side has fewer than two values.
    pub diff_ci: Option<Interval>,
    /// The Welch-Satterthwaite degrees of freedom; none when both variances are zero.
    pub df: Option<f64>,
    /// mean(arm)/mean(base) - 1; none unless both means are positive and `diff_ci` exists.
    pub rel_change: Option<f64>,
    /// The 95% interval of the relative change (delta method on the log of the ratio).
    pub rel_ci: Option<Interval>,
    /// The permutation cross-check; none when a side has no value.
    pub permutation: Option<Permutation>,
    /// The conclusion.
    pub verdict: Verdict,
    /// True when either side has fewer than 5 values or the half-width exceeds the difference.
    pub underpowered: bool,
}

/// std/mean of one arm against a base arm's, on one metric at one setting.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CvComparison {
    /// The setting both arms ran at.
    pub setting: String,
    /// The metric.
    pub metric: String,
    /// The base arm.
    pub base: String,
    /// The arm compared to it.
    pub arm: String,
    /// Values of the base.
    pub n_base: usize,
    /// Values of the arm.
    pub n_arm: usize,
    /// std/mean of the base.
    pub cv_base: Option<f64>,
    /// std/mean of the arm.
    pub cv_arm: Option<f64>,
    /// cv_arm / cv_base.
    pub ratio: Option<f64>,
    /// The approximate 95% interval of the ratio.
    pub ratio_ci: Option<Interval>,
    /// The conclusion.
    pub verdict: Verdict,
    /// True when either side has fewer than 10 values.
    pub underpowered: bool,
}

/// The whole analysis.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    /// One group per setting and arm, in order of first appearance.
    pub groups: Vec<GroupReport>,
    /// The arm comparisons.
    pub comparisons: Vec<Comparison>,
    /// The std/mean comparisons that were asked for.
    pub cv_comparisons: Vec<CvComparison>,
}

/// What to compare and which effects to plan for.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The base arm of the comparisons; the first arm of each setting when none.
    pub baseline: Option<String>,
    /// Compare each arm with the one before it in the setting instead of with the base.
    pub ladder: bool,
    /// Pairs (base, arm) whose std/mean is compared.
    pub cv_pairs: Vec<(String, String)>,
    /// Relative effects to give run counts for.
    pub effects: Vec<f64>,
}
