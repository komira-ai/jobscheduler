//! `js-cpu-replay`: replays the job service's recorded jobs against ways of packing CPU on a
//! host, for the design in `docs/design/cpu-packing.md`.
//!
//! `convert` turns an export (the job records the job service's `GetUsage` returned, and the
//! agents' result lines) into a trace that names no job, host or class ([`convert`],
//! [`trace`]). `run` replays a trace: the same arrivals placed on six hosts of 88 threads by
//! each way of packing ([`report::ARMS`]), with the placer's own sizing
//! ([`sizing`]), each job's CPU spread over its life two ways that bracket what was recorded
//! ([`shape`]) and a host's cores shared between its jobs as the kernel shares them
//! ([`sim`]), and prints what each did ([`report`]).
//!
//! Nothing reads a clock, a random source or an environment variable: a trace gives the same
//! report every time.

#![forbid(unsafe_code)]

pub mod cli;
pub mod convert;
pub mod report;
pub mod rows;
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
