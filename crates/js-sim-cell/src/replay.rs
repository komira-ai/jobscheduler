//! Running a seed, and what a failing seed reports.
//!
//! A run is a function of its seed: [`run`] builds the cell for the seed, runs it with only
//! its trace's hash kept, and on a violation builds and runs the same seed again with the
//! trace kept as text. The [`FailureReport`] holds the scenario's name, the seed, the hash
//! of the first run, the violation, the last [`REPORT_LINES`] lines of the second run and
//! that run's hash, which equals the first unless something outside the seed (a clock, a
//! thread, a hashed collection's order, a random value) reached a decision.

use std::fmt;

use js_lease_server::StartupError;
use js_sim::TraceHash;
use js_types::sched::FarmTime;

use crate::cell::{Cell, Setup};
use crate::invariant::Violation;

/// How many trace lines a failure report keeps.
pub const REPORT_LINES: usize = 40;

/// A failing seed, as a complete bug report.
#[derive(Clone, Debug)]
pub struct FailureReport {
    /// The scenario's name.
    pub scenario: String,
    /// The seed.
    pub seed: u64,
    /// The hash of the run that failed.
    pub hash: TraceHash,
    /// The first violation.
    pub violation: Violation,
    /// The number of the check that found it, from 1.
    pub check: u64,
    /// The hash of the same seed run again with its trace kept.
    pub replay_hash: TraceHash,
    /// The last lines of that run's trace, oldest first.
    pub last_lines: Vec<String>,
}

/// The report: a heading line, the hashes, then the trace lines.
impl fmt::Display for FailureReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "scenario {} seed {} failed at check {}: {}",
            self.scenario, self.seed, self.check, self.violation
        )?;
        writeln!(
            f,
            "trace hash {} (replayed: {})",
            self.hash, self.replay_hash
        )?;
        writeln!(f, "last {} trace lines:", self.last_lines.len())?;
        self.last_lines
            .iter()
            .try_for_each(|line| writeln!(f, "  {line}"))
    }
}

/// Why a seed did not run to its end.
#[derive(Debug)]
pub enum Failure {
    /// The cell could not be built: the server refused its configuration.
    Start(StartupError),
    /// An invariant did not hold.
    Violated(Box<FailureReport>),
}

/// Runs `scenario` at `seed` until `end`: `build` makes the cell for a [`Setup`] (it adds
/// the agents and scripts the actions), and is called again, with trace lines kept, when
/// the run fails.
///
/// # Errors
///
/// [`Failure::Start`] when `build` fails, [`Failure::Violated`] when an invariant does not
/// hold.
pub fn run(
    scenario: &str,
    seed: u64,
    end: FarmTime,
    build: impl Fn(Setup) -> Result<Cell, StartupError>,
) -> Result<Cell, Failure> {
    let mut cell = build(Setup::new(seed)).map_err(Failure::Start)?;
    let Err(violation) = cell.run_until(end) else {
        return Ok(cell);
    };
    let (hash, check) = (cell.trace_hash(), cell.checks());
    drop(cell);
    let mut again = build(Setup::new(seed).with_trace_lines()).map_err(Failure::Start)?;
    let _replayed = again.run_until(end);
    let lines = again.trace_lines().unwrap_or_default();
    let last_lines = lines[lines.len().saturating_sub(REPORT_LINES)..].to_vec();
    Err(Failure::Violated(Box::new(FailureReport {
        scenario: scenario.to_owned(),
        seed,
        hash,
        violation,
        check,
        replay_hash: again.trace_hash(),
        last_lines,
    })))
}
