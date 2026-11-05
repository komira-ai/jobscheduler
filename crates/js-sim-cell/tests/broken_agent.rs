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
use js_sim::{Chance, Faults};
use js_sim_cell::actions::{cancel_work, items, set_bus_faults};
use js_sim_cell::{Counter, Expect, Invariant, Scenario, ScenarioFailure, ServerSpec, Shape, Work};

/// Every scenario here: hosts beat every second and are held lost after three.
const INTERVAL: Duration = Duration::from_secs(1);
const GRACE: Duration = Duration::from_secs(3);

/// The stale start allows a longer silence: its bus delays a heartbeat by up to 4 s.
const STALE_GRACE: Duration = Duration::from_secs(6);

/// The repeated start: runs shorter than the interval, on a bus slower than the interval
/// each way, so the heartbeat that leaves before the start arrives does not list the run
/// and the server sends the start again (design section 8, row 31).
const REPEAT_HOSTS: usize = 2;
const REPEAT_RUNS: u32 = 4;
const REPEAT_RUN: Duration = Duration::from_millis(200);
const REPEAT_DELAY: Duration = Duration::from_millis(1_500);
const REPEAT_SETTLE: Duration = Duration::from_secs(120);
const REPEAT_SEEDS: u64 = 4;

/// The stale start: runs queued on every host, a cancel of every item while their starts
/// are still on the way, and a bus that delays a message by more than the interval and
/// lets a later one overtake it, so a start can arrive after the stop that follows it.
const STALE_HOSTS: usize = 8;
const STALE_RUNS: u32 = 16;
const STALE_RUN: Duration = Duration::from_secs(30);
const STALE_GIVE_UP: Duration = Duration::from_secs(300);
const STALE_FAULTS_AT: Duration = Duration::from_secs(2);
const STALE_MIN_DELAY: Duration = Duration::from_millis(1_000);
const STALE_MAX_DELAY: Duration = Duration::from_millis(4_000);
const STALE_REORDER_PERCENT: u32 = 100;
const STALE_CANCEL_AT: Duration = Duration::from_millis(6_500);
const STALE_QUIET_AT: Duration = Duration::from_secs(30);
const STALE_SETTLE: Duration = Duration::from_secs(120);
const STALE_SEEDS: u64 = 8;

/// The repeated-start scenario, with agents that lack `defect` or correct ones.
fn repeated_start(defect: Option<Defect>) -> Scenario {
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), REPEAT_HOSTS)
        .heartbeat(INTERVAL, GRACE);
    let slow = Faults {
        min_delay: REPEAT_DELAY,
        max_delay: REPEAT_DELAY,
        ..Faults::default()
    };
    let scenario = Scenario::new("an_agent_that_relaunches_a_repeated_start")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .bus(slow)
        .work(REPEAT_RUNS, Work::run_for(REPEAT_RUN))
        .expect(Expect::all_items_terminal())
        .witness("start_repeated", |c| c.get(Counter::StartRepeated) >= 1)
        .settle_within(REPEAT_SETTLE);
    match defect {
        Some(defect) => scenario.broken_hosts("burst", REPEAT_HOSTS, defect),
        None => scenario.hosts("burst", REPEAT_HOSTS),
    }
}

/// The stale-start scenario, with agents that lack `defect` or correct ones.
fn stale_start(defect: Option<Defect>) -> Scenario {
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), STALE_HOSTS)
        .heartbeat(INTERVAL, STALE_GRACE);
    let late = Faults {
        min_delay: STALE_MIN_DELAY,
        max_delay: STALE_MAX_DELAY,
        reorder: Chance::percent(STALE_REORDER_PERCENT),
        ..Faults::default()
    };
    let work = Work::run_for(STALE_RUN).give_up_after(STALE_GIVE_UP);
    let scenario = Scenario::new("an_agent_that_never_retires_a_stopped_run")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .work(STALE_RUNS, work)
        .at(STALE_FAULTS_AT, set_bus_faults(late))
        .at(STALE_CANCEL_AT, cancel_work(items(0..STALE_RUNS)))
        .at(STALE_QUIET_AT, set_bus_faults(Faults::default()))
        .expect(Expect::all_items_terminal())
        .witness("start_after_stop", |c| c.get(Counter::StartAfterStop) >= 1)
        .settle_within(STALE_SETTLE);
    match defect {
        Some(defect) => scenario.broken_hosts("burst", STALE_HOSTS, defect),
        None => scenario.hosts("burst", STALE_HOSTS),
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

/// Asserts that with runs shorter than the heartbeat interval on a bus slower than it,
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

/// Asserts that when every item is cancelled while its starts are still on the way, on a
/// bus that delays a message past the heartbeat interval and lets a later one overtake an
/// earlier, agents that do not retire a stopped run's id (`ForgetsRetiredIds`) fail
/// `at-most-one-launch`, and that correct agents pass every seed with the witness held:
/// some start arrived after the stop that follows it. Catches a harness in which a stale
/// start never reaches an agent, and an agent node that launches a start it was told to
/// stop.
#[test]
fn an_agent_that_never_retires_a_stopped_run_fails_when_a_stale_start_arrives() {
    let failure = stale_start(Some(Defect::ForgetsRetiredIds))
        .try_run_seeds(STALE_SEEDS)
        .expect_err("an agent that forgets a stopped run fails");
    let (_, invariant, text) = violated(failure);
    assert_eq!(invariant, Invariant::AtMostOneLaunch, "{text}");
    let passed = stale_start(None).run_seeds(STALE_SEEDS);
    let stale: u64 = passed
        .runs
        .iter()
        .map(|run| run.counters.get(Counter::StartAfterStop))
        .sum();
    assert!(stale >= 1, "the witness held");
}
