//! The negative cases of the harness itself: an agent that lacks a behaviour the server
//! relies on must turn the cell red. Each scenario runs twice: with agents that lack the
//! behaviour, where it must fail the invariant named, and with correct agents, where it
//! must pass and its witness must hold, which shows that the situation the defect needs
//! happened.
//!
//! Sizes: a handful of hosts beating every second for about two minutes, a few hundred
//! agent requests a seed.

use std::time::Duration;

use js_conformance::agent::Defect;
use js_sim_cell::{Counter, Expect, Invariant, Scenario, ScenarioFailure, ServerSpec, Shape, Work};

/// Every scenario here: hosts beat every second and are held lost after three.
const INTERVAL: Duration = Duration::from_secs(1);
const GRACE: Duration = Duration::from_secs(3);

/// The repeated start: runs shorter than the interval, and a worker slower than the
/// interval in acting on a reply, so the heartbeat that leaves before the worker acted on
/// the start does not list the run and the server sends the start again (design section 8,
/// row 31).
const REPEAT_HOSTS: usize = 2;
const REPEAT_RUNS: u32 = 4;
const REPEAT_RUN: Duration = Duration::from_millis(200);
const REPEAT_WORKER_LAG: Duration = Duration::from_millis(1_500);
const REPEAT_SETTLE: Duration = Duration::from_secs(120);
const REPEAT_SEEDS: u64 = 4;

/// The repeated-start scenario, with agents that lack `defect` or correct ones.
fn repeated_start(defect: Option<Defect>) -> Scenario {
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), REPEAT_HOSTS)
        .heartbeat(INTERVAL, GRACE);
    let scenario = Scenario::new("an_agent_that_relaunches_a_repeated_start")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .worker_lag(REPEAT_WORKER_LAG)
        .work(REPEAT_RUNS, Work::run_for(REPEAT_RUN))
        .expect(Expect::all_items_terminal())
        .witness("start_repeated", |c| c.get(Counter::StartRepeated) >= 1)
        .settle_within(REPEAT_SETTLE);
    match defect {
        Some(defect) => scenario.broken_hosts("burst", REPEAT_HOSTS, defect),
        None => scenario.hosts("burst", REPEAT_HOSTS),
    }
}

/// The violation a failed scenario reports; any other failure fails the test.
fn violated(failure: ScenarioFailure) -> (u64, Invariant, String) {
    match failure {
        ScenarioFailure::Violated(report) => {
            let text = report.to_string();
            (report.seed, report.violation.invariant, text)
        }
        other => panic!("not a violation: {other}"),
    }
}

/// Asserts that with runs shorter than the heartbeat interval and a worker slower than it,
/// agents that run a repeated start (`StartRunsTwice`) fail `at-most-one-launch` at the
/// first seed, and that the report names that seed; and that the same scenario with
/// correct agents passes every seed with its witness held: a repeated start was
/// delivered. Catches a harness that never lets the defect show (no start is ever sent
/// twice, or the ledger misses the second launch).
#[test]
fn an_agent_that_relaunches_a_repeated_start_fails_at_most_one_launch() {
    let failure = repeated_start(Some(Defect::StartRunsTwice))
        .try_run_seeds(REPEAT_SEEDS)
        .expect_err("an agent that runs a start twice fails");
    let (seed, invariant, text) = violated(failure);
    assert_eq!((seed, invariant), (0, Invariant::AtMostOneLaunch));
    assert!(text.contains(" seed 0 failed at check "), "{text}");
    let passed = repeated_start(None).run_seeds(REPEAT_SEEDS);
    let repeated: u64 = passed
        .runs
        .iter()
        .map(|run| run.counters.get(Counter::StartRepeated))
        .sum();
    assert!(repeated >= 1, "the witness held");
}
