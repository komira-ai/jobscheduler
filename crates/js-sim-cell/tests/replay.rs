//! Whether a seed replays: the real server, twenty agents beating every five seconds, one
//! hand-made lease and run, 10% of the agent wire's messages dropped, and ten agents cut
//! off for thirty seconds before the cut heals.
//!
//! A run is a function of its seed. These tests decide whether it is: the same seed twice
//! in one process, two seeds against each other, and pinned seeds against the golden file
//! (each test binary runs in its own process, so the golden file also pins the hash across
//! processes). The failure report and the stop at the first violation are shown with a
//! violation the test makes itself, through the run loop's test hook.
//!
//! Size: [`HOSTS`] agents over [`HORIZON_MS`] at [`INTERVAL`] make about
//! `HOSTS * HORIZON_MS / INTERVAL` = 480 heartbeats a seed, with the hellos besides; the
//! golden test runs [`SEEDS`] seeds.

#[path = "cell/support.rs"]
mod support;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use js_lease_server::StartupError;
use js_sim::golden::GoldenFile;
use js_sim::{Chance, Faults, TraceHash};
use js_sim_cell::replay::REPORT_LINES;
use js_sim_cell::{
    Cell, Failure, FailureReport, Invariant, Observations, ServerSpec, Setup, Shape, Violation, run,
};

use crate::support::{lease, ms, scratch, start_run};

/// How many agents the cell runs.
const HOSTS: usize = 20;

/// How long each seed runs, in virtual milliseconds.
const HORIZON_MS: u64 = 120_000;

/// How often the agents beat: the server's default, which the spec keeps.
const INTERVAL: Duration = Duration::from_secs(5);

/// How many seeds the golden file pins.
const SEEDS: u64 = 16;

/// The scenario's name, in reports and in the golden file.
const SCENARIO: &str = "replay";

/// When the client makes its lease and run, and how long the run takes.
const LEASE_AT_MS: u64 = 20_000;
const RUN_MS: u64 = 10_000;

/// The agents cut off, and when the cut starts and heals.
const CUT: [usize; 10] = [10, 11, 12, 13, 14, 15, 16, 17, 18, 19];
const CUT_AT_MS: u64 = 40_000;
const HEAL_AT_MS: u64 = 70_000;

/// The check a test makes fail.
const FAIL_AT_CHECK: u64 = 100;

/// The server of the scenario.
fn spec() -> ServerSpec {
    ServerSpec::registered_pool("burst", Shape::gpu_host(), HOSTS)
}

/// The scenario for `setup`.
fn build(setup: Setup) -> Result<Cell, StartupError> {
    let faults = Faults {
        drop: Chance::percent(10),
        ..Faults::default()
    };
    let mut cell = Cell::new(setup, spec(), scratch(SCENARIO), faults)?;
    for _ in 0..HOSTS {
        cell.add_agent().expect("an agent name is a host id");
    }
    cell.at(ms(LEASE_AT_MS), |cell| {
        if let Some(lease) = lease(cell) {
            start_run(cell, &lease, RUN_MS);
        }
    });
    cell.at(ms(CUT_AT_MS), |cell| cell.partition(&[&CUT]));
    cell.at(ms(HEAL_AT_MS), Cell::heal);
    Ok(cell)
}

/// The scenario for `setup`, with a violation made at check [`FAIL_AT_CHECK`], and every
/// check the hook was called at written into `seen`.
fn failing(setup: Setup, seen: &Rc<RefCell<Vec<u64>>>) -> Result<Cell, StartupError> {
    let mut cell = build(setup)?;
    let seen = Rc::clone(seen);
    cell.check_also(move |check, observations: &Observations| {
        seen.borrow_mut().push(check);
        (check == FAIL_AT_CHECK).then(|| Violation {
            invariant: Invariant::AtMostOneLaunch,
            at: observations.at,
            detail: "made by the test".to_owned(),
        })
    });
    Ok(cell)
}

/// The hash of `seed`'s run to the horizon, which must hold.
fn hash(seed: u64) -> TraceHash {
    match run(SCENARIO, seed, ms(HORIZON_MS), build) {
        Ok(cell) => cell.trace_hash(),
        Err(failure) => panic!("seed {seed} failed: {failure:?}"),
    }
}

/// The report of a run that fails at [`FAIL_AT_CHECK`].
fn report(seed: u64, seen: &Rc<RefCell<Vec<u64>>>) -> FailureReport {
    match run(SCENARIO, seed, ms(HORIZON_MS), |setup| failing(setup, seen)) {
        Err(Failure::Violated(report)) => *report,
        Err(Failure::Start(err)) => panic!("the cell did not start: {err}"),
        Ok(_) => panic!("the made violation did not stop the run"),
    }
}

/// Asserts that one seed run twice in one process gives one hash, and that a run that
/// keeps its trace lines gives the same hash as one that keeps only the hash; and that the
/// run did what the scenario asks (the lease's run launched once, its agent listed live at
/// the end), so the hash covers a run that reached the server. Catches a handler path that
/// reads a clock, iterates a hashed collection to decide an outcome, or lets a random value
/// reach the trace; and a trace whose lines depend on whether they are kept.
#[test]
fn a_seed_replays_to_the_same_trace_hash() {
    assert_eq!(spec().heartbeat_interval(), INTERVAL);
    let first = run(SCENARIO, 7, ms(HORIZON_MS), build).expect("seed 7 runs");
    assert_eq!(first.trace_hash(), hash(7));
    let mut kept = build(Setup::new(7).with_trace_lines()).expect("the cell starts");
    kept.run_until(ms(HORIZON_MS)).expect("seed 7 runs");
    assert_eq!(kept.trace_hash(), first.trace_hash());
    let lines = kept.trace_lines().expect("lines are kept");
    assert_eq!(lines[0], "kbf-trace-format 1 sim");
    assert!(lines.iter().any(|line| line.ends_with(" cell launch h000")));
    assert_eq!(first.ledger().launches().len(), 1);
    let listed = &first.observations().server.hosts;
    assert_eq!(listed.len(), HOSTS);
    assert!(
        listed.values().all(|host| !host.lost),
        "every host is live at the end"
    );
}

/// Asserts that two seeds give two hashes. Catches a seed that never reaches the bus or
/// the agents.
#[test]
fn different_seeds_make_different_traces() {
    assert_ne!(hash(1), hash(2));
}

fn golden() -> GoldenFile {
    GoldenFile::new(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/trace-hashes.txt"),
        "js-sim-cell",
        "cargo test -p js-sim-cell --test replay -- --ignored regenerate_golden_hashes",
    )
}

fn golden_runs() -> Vec<(String, TraceHash)> {
    let each = |seed| (format!("{SCENARIO}/seed-{seed}"), hash(seed));
    (0..SEEDS).map(each).collect()
}

/// Asserts that the hashes of the pinned seeds equal the golden file's. Catches any change
/// to a decision of the server or the agents, or to the trace text, and, because the file
/// was written by another process, a run that differs between processes.
#[test]
fn pinned_seeds_match_the_golden_file() {
    golden().assert_matches(&golden_runs());
}

#[test]
#[ignore = "regenerates the golden trace hashes; run it only on purpose"]
fn regenerate_golden_hashes() {
    golden().regenerate(&golden_runs()).unwrap();
}

/// Asserts that a run stopped by a violation reports the scenario, the seed, the trace
/// hash, the violation and the last forty trace lines, the last of them the violation;
/// that the run again with trace lines kept has the first run's hash; and that the report
/// as text holds every part. Catches a re-run that diverges from the first (a source of
/// nondeterminism the seed does not cover) and a report that loses a part.
#[test]
fn a_failure_report_names_the_scenario_the_seed_the_hash_and_the_last_forty_trace_lines() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let report = report(5, &seen);
    assert_eq!(report.scenario, SCENARIO);
    assert_eq!(report.seed, 5);
    assert_eq!(report.replay_hash, report.hash);
    assert_eq!(report.violation.invariant, Invariant::AtMostOneLaunch);
    assert_eq!(report.violation.detail, "made by the test");
    assert_eq!(report.check, FAIL_AT_CHECK);
    assert_eq!(report.last_lines.len(), REPORT_LINES);
    let last = report.last_lines.last().expect("forty lines");
    assert!(
        last.ends_with(" cell violation at-most-one-launch"),
        "{last}"
    );
    let text = report.to_string();
    let heading = format!(
        "scenario {SCENARIO} seed 5 failed at check {FAIL_AT_CHECK}: {}",
        report.violation
    );
    assert!(text.starts_with(&heading), "{text}");
    let hashes = format!("trace hash {} (replayed: {})", report.hash, report.hash);
    assert!(text.contains(&hashes), "{text}");
    assert!(
        text.contains(&format!("last {REPORT_LINES} trace lines:\n")),
        "{text}"
    );
    assert!(text.ends_with(&format!("  {last}\n")), "{text}");
}

/// Asserts that a violation made at check N stops the run at check N, with every check
/// from 1 to N made in order (one per step), that the cell's clock stopped there, and that
/// running on returns the same violation without another check. Catches a check that is
/// batched or skipped, and a run that goes on past its first violation.
#[test]
fn the_run_loop_stops_at_the_first_violation_and_checks_after_every_step() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut cell = failing(Setup::new(3), &seen).expect("the cell starts");
    let violation = cell
        .run_until(ms(HORIZON_MS))
        .expect_err("the made violation");
    assert_eq!(violation.detail, "made by the test");
    assert_eq!(cell.checks(), FAIL_AT_CHECK);
    let every: Vec<u64> = (1..=FAIL_AT_CHECK).collect();
    assert_eq!(*seen.borrow(), every);
    let stopped = cell.now();
    assert_eq!(violation.at, stopped);
    assert!(
        stopped < ms(LEASE_AT_MS),
        "stopped at {}",
        stopped.as_millis()
    );
    assert_eq!(cell.run_until(ms(HORIZON_MS)), Err(violation));
    assert_eq!(cell.checks(), FAIL_AT_CHECK);
    assert_eq!(cell.now(), stopped);
    assert_eq!(seen.borrow().len(), every.len());
}

/// Asserts that a cell whose server refuses its configuration (a pool of no machines) is
/// reported as a failure to start, naming the pool file, and that a shape that is not a
/// host (a GPU with no class) is refused before the server starts. Catches a refusal that
/// is lost or reported as a violation.
#[test]
fn a_server_that_refuses_its_configuration_is_a_failure_to_start() {
    let empty = |setup| {
        // A burst of one, so the flags parse and the pool file is what is refused.
        let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), 0).hello_burst(1);
        Cell::new(setup, spec, scratch("empty"), Faults::default())
    };
    match run(SCENARIO, 0, ms(HORIZON_MS), empty) {
        Err(Failure::Start(err)) => assert_eq!(err.flag(), "--agent-pool-config", "{err}"),
        Err(other) => panic!("not a failure to start: {other:?}"),
        Ok(_) => panic!("a pool of no machines started"),
    }
    let shape = Shape {
        gpu_class: None,
        ..Shape::gpu_host()
    };
    let spec = ServerSpec::registered_pool("burst", shape, 1);
    let refused = Cell::new(Setup::new(0), spec, scratch("shape"), Faults::default());
    assert_eq!(refused.err().map(|err| err.flag()), Some("shape"));
}
