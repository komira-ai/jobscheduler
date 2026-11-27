//! `js-cpu-replay`: replays the job service's recorded jobs against ways of packing CPU on a
//! host, for the design in `docs/design/cpu-over-time.md`.
//!
//! `convert` turns an export (the job records the job service's `GetUsage` returned, and the
//! agents' result lines) into a trace that names no job, host or class ([`convert`],
//! [`trace`]). `run` replays a trace: the same arrivals placed on six hosts of 88 threads by
//! each way of packing ([`report::ARMS`]), with the placer's own sizing
//! ([`sizing`]), each job's CPU spread over its life two ways that bracket what was recorded
//! ([`shape`]) and a host's cores shared between its jobs as the kernel shares them
//! ([`sim`]), and prints what each did ([`report`]). One arm books each class's CPU curve over
//! a job's age, learned from the series the jobs' hosts recorded, and places by the curves
//! ([`sim::curve`]); `run --learn-days` adds the gate before the curves place jobs: that arm
//! against the flat step on the days held out from those it learned on ([`gate`]). `shadow`
//! reads the job service's own log and prints, per class, how close its shadow profiles came to
//! the runs ([`shadow`]).
//!
//! The replay reads no clock, random source or environment variable: a trace gives the same
//! report every time. Only `convert --rows` opens a scratch store, with the wall clock and the
//! job store's random ids, and neither reaches the trace.

#![forbid(unsafe_code)]

pub mod cli;
pub mod convert;
pub mod gate;
pub mod report;
pub mod rows;
pub mod shadow;
pub mod shape;
pub mod sim;
pub mod sizing;
pub mod trace;

use std::process::ExitCode;

use clap::Parser as _;

pub use cli::{Cli, Command, Error, run};

/// Runs the command line: what it says goes to standard output, a failure to standard error.
#[must_use]
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli, &mut std::io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("js-cpu-replay: {error}");
            ExitCode::FAILURE
        }
    }
}
