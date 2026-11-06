//! A sweep: the seeds of a scenario family far past the few a pull request runs, split
//! into shards, each shard one run of the test binary `tests/sweep.rs` (the nightly
//! `sim-sweep` workflow runs them; docs/simulation.md says how).
//!
//! [`run`] reads the binary's flags and runs every scenario of the family, or the one
//! `--scenario` names, over the shard's seeds with [`Scenario::try_run_seed_range`]. It
//! prints a line for each scenario that passed and, for each that failed, the failure as
//! text (for a violation the whole [`crate::FailureReport`]) followed by the command that
//! runs that scenario at that seed again. It goes on to the next scenario after a failure,
//! so one run reports every scenario that fails in the shard.
//!
//! A test binary is also run by a plain `cargo test`, with libtest's arguments (a filter,
//! `--nocapture`, `--list`) or none. A run given none of the sweep's flags sweeps nothing:
//! it says why on stderr and passes. (`--help` prints the sweep's usage.)
//!
//! A witness is judged over the seeds of one shard, so a sharded sweep needs each witness of
//! the family to hold in nearly every shard-sized range of seeds, not only somewhere in the
//! whole sweep.

use std::ffi::OsString;
use std::io::Write;
use std::ops::Range;
use std::process::ExitCode;

use clap::Parser;

use crate::scenario::{Scenario, ScenarioFailure};

/// The command that runs the sweep, up to its flags: what a replay line starts with.
pub const COMMAND: &str = "cargo test --release --locked -p js-sim-cell --test sweep --";

/// The sweep's flags; one of them, or `--help`, makes a run a sweep.
const FLAGS: [&str; 5] = ["--start", "--count", "--shards", "--shard", "--scenario"];

/// The flags of a sweep.
#[derive(Debug, Parser)]
#[command(
    name = "sweep",
    about = "Run a scenario family over a range of seeds, one shard of it"
)]
struct Flags {
    /// The first seed of the sweep, over all its shards.
    #[arg(long, default_value_t = 0)]
    start: u64,
    /// How many seeds the sweep runs, over all its shards.
    #[arg(long)]
    count: u64,
    /// How many shards the seeds are split into, each a run of its own.
    #[arg(long, default_value_t = 1)]
    shards: u64,
    /// Which shard this run is, from 0.
    #[arg(long, default_value_t = 0)]
    shard: u64,
    /// Run only the scenario of this name.
    #[arg(long)]
    scenario: Option<String>,
}

/// The seeds of shard `shard` of `shards` of the `count` seeds from `start`: contiguous,
/// in order, the shards' sizes differing by at most one.
///
/// # Errors
///
/// The flags that do not fit together, as the line to print.
fn shard_seeds(flags: &Flags) -> Result<Range<u64>, String> {
    let Flags {
        start,
        count,
        shards,
        shard,
        ..
    } = *flags;
    if count == 0 {
        return Err("--count must be at least 1".to_owned());
    }
    if shards == 0 {
        return Err("--shards must be at least 1".to_owned());
    }
    if shard >= shards {
        return Err(format!(
            "--shard {shard} is not below --shards {shards} (shards count from 0)"
        ));
    }
    if count < shards {
        return Err(format!(
            "--count {count} is fewer than --shards {shards}, so a shard would run no seed"
        ));
    }
    if start.checked_add(count).is_none() {
        return Err(format!(
            "--start {start} and --count {count} run past the last seed, {}",
            u64::MAX
        ));
    }
    let at = |index: u64| {
        let offset = u128::from(count) * u128::from(index) / u128::from(shards);
        // At most `count`, so it fits, and `start + count` was checked above.
        start + u64::try_from(offset).unwrap_or(count)
    };
    Ok(at(shard)..at(shard + 1))
}

/// Whether `args` hold a flag of the sweep (`--count 5` or `--count=5`), or ask for its
/// usage (`--help`, `-h`).
fn is_sweep(args: &[OsString]) -> bool {
    args.iter().filter_map(|arg| arg.to_str()).any(|arg| {
        let flag = |flag: &&str| arg == *flag || arg.starts_with(&format!("{flag}="));
        FLAGS.iter().any(flag) || arg == "--help" || arg == "-h"
    })
}

/// The replay line for `failure` of `scenario`, a failure of a sweep over `seeds`: the
/// failing seed alone, or the whole range for a witness no seed of it satisfied.
fn replay(scenario: &str, seeds: &Range<u64>, failure: &ScenarioFailure) -> String {
    let (start, count) = match failure {
        ScenarioFailure::Start { seed, .. } | ScenarioFailure::Unmet { seed, .. } => (*seed, 1),
        ScenarioFailure::Violated(report) => (report.seed, 1),
        ScenarioFailure::Unwitnessed { .. } => (seeds.start, seeds.end - seeds.start),
    };
    format!("replay: {COMMAND} --scenario {scenario} --start {start} --count {count}")
}

/// Runs the sweep `args` (the program's arguments, its name first) ask for over `family`,
/// writing what passed and what failed to `out` and why a run is not a sweep or its flags
/// are wrong to `err`: the exit status is 0 when every scenario run passed (or the run is
/// not a sweep), 1 when one failed, 2 when the flags are wrong (0 for `--help`).
pub fn run(
    family: &[Scenario],
    args: impl IntoIterator<Item = OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let args: Vec<OsString> = args.into_iter().collect();
    if !is_sweep(&args) {
        let seen = args.iter().skip(1).map(|arg| arg.to_string_lossy());
        let seen: Vec<_> = seen.collect();
        let _ = writeln!(
            err,
            "sweep: no sweep flag ({}), so no seed runs; arguments: [{}]",
            FLAGS.join(", "),
            seen.join(" ")
        );
        return ExitCode::SUCCESS;
    }
    let flags = match Flags::try_parse_from(&args) {
        Ok(flags) => flags,
        Err(error) => {
            let shown = error.render();
            let _ = if error.use_stderr() {
                write!(err, "{shown}")
            } else {
                write!(out, "{shown}")
            };
            return ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(2));
        }
    };
    let seeds = match shard_seeds(&flags) {
        Ok(seeds) => seeds,
        Err(why) => {
            let _ = writeln!(err, "sweep: {why}");
            return ExitCode::from(2);
        }
    };
    let chosen: Vec<&Scenario> = family
        .iter()
        .filter(|scenario| {
            flags
                .scenario
                .as_deref()
                .is_none_or(|n| n == scenario.name())
        })
        .collect();
    if chosen.is_empty() {
        let names: Vec<&str> = family.iter().map(Scenario::name).collect();
        let named = flags.scenario.unwrap_or_default();
        let _ = writeln!(
            err,
            "sweep: no scenario {named}; the family has {}",
            names.join(", ")
        );
        return ExitCode::from(2);
    }
    let _ = writeln!(
        out,
        "sweep: seeds {}..{}, shard {} of {} of seeds {}..{}",
        seeds.start,
        seeds.end,
        flags.shard,
        flags.shards,
        flags.start,
        flags.start + flags.count
    );
    let mut failed = 0;
    for scenario in chosen {
        let name = scenario.name();
        match scenario.try_run_seed_range(seeds.clone()) {
            Ok(passed) => {
                let ran = passed.runs.len();
                let _ = writeln!(out, "scenario {name}: {ran} seeds passed");
            }
            Err(failure) => {
                failed += 1;
                let _ = writeln!(out, "{failure}");
                let _ = writeln!(out, "{}", replay(name, &seeds, &failure));
            }
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
