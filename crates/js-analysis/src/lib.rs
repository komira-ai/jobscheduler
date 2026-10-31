//! `js-analysis`: the statistics of the benchmark arms.
//!
//! A measurement crate emits one [`RunRecord`] per run as a JSON line. This crate groups the
//! records by setting and arm (first appearance order, run order kept), leaves warm-ups out
//! of the statistics but lists them, leaves not-available values out of each metric's
//! statistics and counts them, and reports per group and metric the mean, median, standard
//! deviation, range, std/mean and the 95% intervals of the mean and of std/mean. Two arms
//! differ when the 95% Welch interval of the difference of their means excludes zero; the
//! rules and formulas are in `docs/design/bench-analysis.md`.
//!
//! Nothing is imputed, trimmed or dropped, no clock or random source is read (the sampled
//! permutation test uses a fixed seed), and no environment variable is read.

mod analyze;
mod cli;
mod compare;
mod describe;
mod error;
mod quantile;
mod render;
mod types;

use std::process::ExitCode;

use clap::Parser as _;

pub use analyze::{analyze, parse_jsonl};
pub use cli::{Cli, run};
pub use compare::{compare, cv_compare, permutation_test};
pub use describe::{cv_interval, min_runs, summarize};
pub use error::Error;
pub use quantile::{chi_square_quantile, t_critical, t_quantile};
pub use render::{to_markdown, write_json};
pub use types::{
    Comparison, CvComparison, GroupReport, Interval, MAX_PLAN_RUNS, MetricReport, Options, Pair,
    PermMethod, Permutation, Report, RunPlan, RunRecord, Summary, Verdict, WARMUP,
};

/// Runs the command line: the report goes to standard output, a failure to standard error.
#[must_use]
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli, &mut std::io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("js-analysis: {error}");
            ExitCode::FAILURE
        }
    }
}
