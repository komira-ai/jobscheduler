//! `js-analysis`: the statistics of the benchmark arms (skeleton: the functions are not
//! written yet).

mod error;
mod types;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

pub use error::Error;
pub use types::{
    Comparison, CvComparison, GroupReport, Interval, MAX_PLAN_RUNS, MetricReport, Options, Pair,
    PermMethod, Permutation, Report, RunPlan, RunRecord, Summary, Verdict, WARMUP,
};

/// `js-analysis RUNS.jsonl`: reads run records (JSON lines), prints the report as
/// markdown, and writes it as JSON with `--json`.
#[derive(Clone, Debug, Parser)]
#[command(name = "js-analysis", version, about)]
pub struct Cli {
    /// The run records, one JSON object per line.
    pub input: PathBuf,
    /// The base arm of the comparisons (default: the first arm of each setting).
    #[arg(long)]
    pub baseline: Option<String>,
    /// Compare each arm with the one before it in its setting, not with the base.
    #[arg(long, conflicts_with = "baseline")]
    pub ladder: bool,
    /// Compare the std/mean of two arms: `--compare-cv BASE ARM`; repeatable.
    #[arg(long, num_args = 2, value_names = ["BASE", "ARM"])]
    pub compare_cv: Vec<String>,
    /// A relative effect to give run counts for (0.05 is 5%); repeatable.
    #[arg(long = "effect", default_values_t = vec![0.05, 0.1])]
    pub effects: Vec<f64>,
    /// Also write the report as JSON to this file.
    #[arg(long)]
    pub json: Option<PathBuf>,
}

/// Reads JSON lines, one record per non-blank line.
///
/// # Errors
/// The first line that is not a record.
pub fn parse_jsonl(_text: &str) -> Result<Vec<RunRecord>, Error> {
    todo!()
}

/// Groups the records and computes the report.
///
/// # Errors
/// A repeated run or an unknown arm.
pub fn analyze(_records: Vec<RunRecord>, _options: &Options) -> Result<Report, Error> {
    todo!()
}

/// The summary of `values`.
#[must_use]
pub fn summarize(_values: &[f64]) -> Option<Summary> {
    todo!()
}

/// The 95% interval of std/mean.
#[must_use]
pub fn cv_interval(_cv: f64, _n: usize) -> Option<Interval> {
    todo!()
}

/// The runs per arm that resolve an effect.
#[must_use]
pub fn min_runs(_cv: f64, _effect: f64, _power80: bool) -> Option<usize> {
    todo!()
}

/// The Welch comparison.
#[must_use]
pub fn compare(_setting: &str, _metric: &str, _base: Pair<'_>, _arm: Pair<'_>) -> Comparison {
    todo!()
}

/// The std/mean comparison.
#[must_use]
pub fn cv_compare(_setting: &str, _metric: &str, _base: Pair<'_>, _arm: Pair<'_>) -> CvComparison {
    todo!()
}

/// The permutation test.
#[must_use]
pub fn permutation_test(_base: &[f64], _arm: &[f64]) -> Permutation {
    todo!()
}

/// A quantile of Student's t.
#[must_use]
pub fn t_quantile(_p: f64, _df: f64) -> f64 {
    todo!()
}

/// The 95% two-sided critical value of t.
#[must_use]
pub fn t_critical(_df: f64) -> f64 {
    todo!()
}

/// A quantile of chi-square.
#[must_use]
pub fn chi_square_quantile(_p: f64, _df: f64) -> f64 {
    todo!()
}

/// The report as markdown.
#[must_use]
pub fn to_markdown(_report: &Report) -> String {
    todo!()
}

/// Writes the report as JSON.
///
/// # Errors
/// The writer failing.
pub fn write_json(_report: &Report, _out: &mut dyn Write) -> Result<(), Error> {
    todo!()
}

/// Runs the command.
///
/// # Errors
/// Whatever stops it.
pub fn run(_cli: &Cli, _out: &mut dyn Write) -> Result<(), Error> {
    todo!()
}

/// Runs the command line.
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
