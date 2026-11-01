//! The command line.

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use clap::Parser;

use crate::analyze::{analyze, parse_jsonl};
use crate::error::Error;
use crate::render::{to_markdown, write_json};
use crate::types::Options;

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

/// Runs the command: markdown to `out`, JSON to the `--json` file.
///
/// # Errors
/// Whatever stops the read, the analysis or a write.
pub fn run(cli: &Cli, out: &mut dyn Write) -> Result<(), Error> {
    let text = fs::read_to_string(&cli.input).map_err(|source| Error::Read {
        path: cli.input.clone(),
        source,
    })?;
    let options = Options {
        baseline: cli.baseline.clone(),
        ladder: cli.ladder,
        cv_pairs: cli
            .compare_cv
            .chunks_exact(2)
            .map(|c| (c[0].clone(), c[1].clone()))
            .collect(),
        effects: cli.effects.clone(),
    };
    let report = analyze(parse_jsonl(&text)?, &options)?;
    out.write_all(to_markdown(&report).as_bytes())?;
    if let Some(path) = &cli.json {
        write_json(&report, &mut File::create(path)?)?;
    }
    Ok(())
}
