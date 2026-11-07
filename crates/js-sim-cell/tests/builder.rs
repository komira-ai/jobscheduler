//! The scenario builder, shown to fail when it should: a witness no seed satisfies, an
//! expectation not met, a violation and its report, an action at its virtual time, the
//! seeds in order, a fault that never ends, hosts of an unknown pool; and a client call
//! that the bus's faults never reach.
//!
//! Sizes: one or two hosts beating every 5 s for at most a few minutes, a few hundred
//! agent requests a seed.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use js_conformance::agent::Defect;
use js_sim::{Chance, Faults};
use js_sim_cell::actions::{agents, kill_agents, replace_agents, set_bus_faults, submit_work};
use js_sim_cell::{
    Action, AgentNode, Cell, ClientEvent, Counter, Expect, HostIs, HowEnded, Invariant, Scenario,
    ScenarioFailure, ServerSpec, Setup, Shape, Work,
};
use js_testkit::dir::TestDir;
use js_types::Resources;
use js_types::sched::FarmTime;

/// The small scenario most tests here vary: one host beating at the server's default,
/// one run of [`RUN`], settled [`SETTLE`] after its last action.
const RUN: Duration = Duration::from_secs(10);
const SETTLE: Duration = Duration::from_secs(120);

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// One host and one run of [`RUN`], under `name`.
fn small(name: &str) -> Scenario {
    Scenario::new(name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool("burst", Shape::gpu_host(), 1))
        .hosts("burst", 1)
        .work(1, Work::run_for(RUN))
        .settle_within(SETTLE)
}

/// An action that writes the seed and the virtual time it ran at.
#[derive(Clone, Default)]
struct Clock(Rc<RefCell<Vec<(u64, u64)>>>);

impl Action for Clock {
    fn run(&self, cell: &mut Cell) {
        let ran = (cell.seed(), cell.now().as_millis());
        self.0.borrow_mut().push(ran);
    }
}

/// An action that writes whether agent 0 has a process now.
#[derive(Clone, Default)]
struct Alive(Rc<RefCell<Vec<bool>>>);

impl Action for Alive {
    fn run(&self, cell: &mut Cell) {
        let alive = cell.agent(0).and_then(AgentNode::truth).is_some();
        self.0.borrow_mut().push(alive);
    }
}

/// Asserts that a scenario whose witness no seed satisfies fails naming the witness and
/// the number of seeds, as text too, and that the same scenario without the witness
/// passes. Catches a builder that judges no witness, or judges it on no seed.
#[test]
fn a_witness_that_no_seed_satisfies_fails_the_scenario() {
    let name = "a_witness_that_no_seed_satisfies";
    let never = |c: &js_sim_cell::Counters| c.get(Counter::StartAfterStop) >= 1_000;
    let failure = small(name)
        .witness("never", never)
        .try_run_seeds(2)
        .expect_err("no seed satisfies the witness");
    let ScenarioFailure::Unwitnessed {
        scenario,
        witness,
        seeds,
        ..
    } = &failure
    else {
        panic!("not a witness failure: {failure}");
    };
    assert_eq!(
        (scenario.as_str(), witness.as_str(), *seeds),
        (name, "never", 2)
    );
    let passed = small(name)
        .try_run_seeds(2)
        .expect("passes without the witness");
    assert_eq!(passed.runs.len(), 2);
    let counts: Vec<String> = Counter::ALL
        .iter()
        .map(|c| {
            let total: u64 = passed.runs.iter().map(|run| run.counters.get(*c)).sum();
            format!("{}={total}", c.name())
        })
        .collect();
    assert_eq!(
        failure.to_string(),
        format!(
            "scenario {name}: witness never held in none of 2 seeds; counted over them [{}]",
            counts.join("; ")
        )
    );
}

/// Asserts that `run_seeds` fails its test with the failure as text.
#[test]
#[should_panic(expected = "witness never held in none of 1 seeds")]
fn run_seeds_fails_its_test_with_the_failure_as_text() {
    let never = |c: &js_sim_cell::Counters| c.get(Counter::StartAfterStop) >= 1_000;
    small("run_seeds_fails_its_test")
        .witness("never", never)
        .run_seeds(1);
}

/// Asserts that an expectation a seed does not meet fails the scenario at that seed,
/// naming the statement, what it wanted and what it got, as text too; that every kind of
/// statement is judged (a host's state, a count, all terminal); and that the met twin
/// passes. Catches a builder that never judges an `Expect`.
#[test]
fn a_scenario_with_an_unsatisfied_expect_fails_naming_it() {
    let name = "an_unsatisfied_expect";
    let failure = small(name)
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Cancelled))
        .try_run_seeds(2)
        .expect_err("no item was cancelled");
    let ScenarioFailure::Unmet {
        scenario,
        seed,
        detail,
    } = &failure
    else {
        panic!("not an unmet expect: {failure}");
    };
    assert_eq!((scenario.as_str(), *seed), (name, 0));
    assert_eq!(detail, "terminal-count(cancelled): wanted 1, got 0");
    assert_eq!(
        failure.to_string(),
        format!("scenario {name} seed 0 failed expect terminal-count(cancelled): wanted 1, got 0")
    );
    let lost = small(name)
        .expect(Expect::host_state(0, HostIs::Lost))
        .try_run_seeds(1)
        .expect_err("the host is live");
    assert!(
        lost.to_string()
            .ends_with("host-state(h000): wanted lost, got live"),
        "{lost}"
    );
    small(name)
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Succeeded))
        .expect(Expect::host_state(0, HostIs::Live))
        .expect(Expect::host_state(7, HostIs::Absent))
        .run_seeds(2);
    assert_eq!(HostIs::Absent.name(), "absent");
}

/// Asserts that an action runs at the virtual time it was scripted for, to the
/// millisecond, in every seed, and that actions of one time run in the order they were
/// added. The clock is the cell's: no wall time is read. Catches an action run early (at
/// the start, or at the step before its time) or late (at the next step after it).
#[test]
fn an_action_runs_at_its_virtual_time_and_not_before() {
    let clock = Clock::default();
    let late = Duration::from_millis(7_003);
    small("an_action_runs_at_its_virtual_time")
        .at(late, clock.clone())
        .at(secs(2), clock.clone())
        .at(secs(2), Alive::default())
        .at(secs(2), clock.clone())
        .run_seeds(2);
    let ran = clock.0.borrow().clone();
    let each = [
        (0, 2_000),
        (0, 2_000),
        (0, 7_003),
        (1, 2_000),
        (1, 2_000),
        (1, 7_003),
    ];
    assert_eq!(ran, each);
}

/// Asserts that a scenario failure prints the scenario's name, the seed, the trace hash
/// (equal on the replay), the violated invariant and the last forty trace lines, the last
/// of them the violation. Catches a report that loses a part, or a replay that differs.
#[test]
fn a_scenario_failure_prints_the_name_the_seed_the_hash_the_violated_invariant_and_forty_trace_lines()
 {
    let name = "a_scenario_failure_prints";
    let slow = Faults {
        min_delay: Duration::from_millis(1_500),
        max_delay: Duration::from_millis(1_500),
        ..Faults::default()
    };
    let failure = Scenario::new(name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(
            ServerSpec::registered_pool("burst", Shape::gpu_host(), 1).heartbeat(secs(1), secs(3)),
        )
        .broken_hosts("burst", 1, Defect::StartRunsTwice)
        .bus(slow)
        .work(2, Work::run_for(Duration::from_millis(200)))
        .settle_within(SETTLE)
        .try_run_seeds(3)
        .expect_err("a start run twice");
    let ScenarioFailure::Violated(report) = &failure else {
        panic!("not a violation: {failure}");
    };
    assert_eq!((report.scenario.as_str(), report.seed), (name, 0));
    assert_eq!(report.violation.invariant, Invariant::AtMostOneLaunch);
    assert_eq!(report.replay_hash, report.hash);
    assert_eq!(report.last_lines.len(), 40);
    let last = report.last_lines.last().expect("forty lines");
    assert!(
        last.ends_with(" cell violation at-most-one-launch"),
        "{last}"
    );
    let text = failure.to_string();
    let heading = format!(
        "scenario {name} seed 0 failed at check {}: {}",
        report.check, report.violation
    );
    assert!(text.starts_with(&heading), "{text}");
    assert!(
        text.contains(&format!("trace hash {}", report.hash)),
        "{text}"
    );
    assert!(text.contains("at-most-one-launch"), "{text}");
    assert!(text.ends_with(&format!("  {last}\n")), "{text}");
}

/// Asserts that `run_seeds` runs seeds 0, 1 and 2 once each, in that order, and returns
/// them in order with each seed's own trace hash. Catches a seed skipped, repeated or
/// run out of order.
#[test]
fn run_seeds_runs_each_seed_once_in_order() {
    let clock = Clock::default();
    let passed = small("run_seeds_runs_each_seed_once")
        .at(secs(1), clock.clone())
        .run_seeds(3);
    let seeds: Vec<u64> = clock.0.borrow().iter().map(|(seed, _)| *seed).collect();
    assert_eq!(seeds, [0, 1, 2]);
    let ran: Vec<u64> = passed.runs.iter().map(|run| run.seed).collect();
    assert_eq!(ran, [0, 1, 2]);
    assert_ne!(passed.runs[0].hash, passed.runs[1].hash);
}

/// Asserts that a scenario whose bus stays lossy after its last action fails
/// `settles-when-quiet` naming the active fault: a scenario that never goes quiet cannot
/// pass its end check. Catches an end check made, or skipped, while a fault is active.
#[test]
fn a_scenario_whose_fault_never_ends_fails_settles_when_quiet() {
    let lossy = Faults {
        drop: Chance::percent(10),
        ..Faults::default()
    };
    let failure = small("a_fault_that_never_ends")
        .bus(lossy)
        .try_run_seeds(1)
        .expect_err("a fault is still active");
    let ScenarioFailure::Violated(report) = &failure else {
        panic!("not a violation: {failure}");
    };
    assert_eq!(report.violation.invariant, Invariant::SettlesWhenQuiet);
    assert!(
        report.violation.detail.ends_with(": a fault is active"),
        "{failure}"
    );
}

/// Asserts that hosts of a pool the server does not have, and a scenario with no scratch
/// directory, fail to start, naming what is at fault, as text too. Catches hosts added
/// to the wrong pool without a word.
#[test]
fn a_scenario_that_cannot_be_built_fails_to_start_naming_why() {
    let failure = small("hosts_of_another_pool")
        .hosts("other", 1)
        .try_run_seeds(1)
        .expect_err("no pool named other");
    let ScenarioFailure::Start { seed, error, .. } = &failure else {
        panic!("not a failure to start: {failure}");
    };
    assert_eq!((*seed, error.flag()), (0, "hosts"));
    let text = failure.to_string();
    assert!(
        text.starts_with("scenario hosts_of_another_pool seed 0 did not start: "),
        "{text}"
    );
    assert!(text.contains("the server has no pool other"), "{text}");
    let unplaced = Scenario::new("no_scratch").try_run_seeds(1);
    let Err(ScenarioFailure::Start { error, .. }) = unplaced else {
        panic!("a scenario with no scratch directory ran");
    };
    assert_eq!(error.flag(), "scratch");
}

/// Asserts that `kill_agents` kills the agent it names (it has no process after) and
/// `replace_agents` starts it again (it has one, and its host reads live at the end),
/// and that more work submitted after the replacement settles. Catches an action that
/// acts on another agent, or not at all.
#[test]
fn kill_and_replace_actions_act_on_the_agents_they_name() {
    let alive = Alive::default();
    small("kill_and_replace")
        .server(ServerSpec::registered_pool("burst", Shape::gpu_host(), 2))
        .hosts("burst", 1)
        .at(secs(30), kill_agents(agents(0..1)))
        .at(secs(31), alive.clone())
        .at(secs(60), replace_agents(agents(0..1)))
        .at(secs(61), alive.clone())
        .at(secs(61), submit_work(2, Work::run_for(RUN)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(3, HowEnded::Succeeded))
        .expect(Expect::host_state(0, HostIs::Live))
        .expect(Expect::host_state(1, HostIs::Live))
        .run_seeds(1);
    assert_eq!(*alive.0.borrow(), [false, true]);
}

/// Asserts that an item that asks for its own resources (half a core, 512 bytes, one GPU)
/// in place of the default share is submitted and run with them and succeeds. Catches a
/// front that ignores `Work::needs` (the job asks for the default share).
#[test]
fn work_that_needs_its_own_resources_is_run_with_them() {
    let needs = Resources::new(500, 512, 1, Some("nvidia-a100".to_owned())).expect("resources");
    let work = Work::run_for(RUN).needs(needs.clone());
    assert_eq!(work.resources(), &needs);
    assert_eq!(work.runs_for(), RUN);
    Scenario::new("work_that_needs_its_own_resources")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool("burst", Shape::gpu_host(), 1))
        .hosts("burst", 1)
        .work(1, work)
        .expect(Expect::terminal_count(1, HowEnded::Succeeded))
        .settle_within(SETTLE)
        .run_seeds(1);
}

/// Asserts that with every message of the agent wire dropped, the client's submit, its
/// reads, its give-up and its cancel still reach the server and are answered: the item
/// learns its job is placed, is given up when no start can reach the host, and its cancel
/// is answered. Catches a client that goes through the bus.
#[test]
fn a_client_call_is_never_dropped_by_the_bus_faults() {
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), 1);
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell"),
        "never_dropped",
    );
    let mut cell = Cell::new(Setup::new(3), spec, dir, Faults::default()).expect("starts");
    cell.add_agent().expect("an agent name is a host id");
    cell.run_until(FarmTime::from_millis(5_000))
        .expect("registered");
    let all = Faults {
        drop: Chance::always(),
        ..Faults::default()
    };
    set_bus_faults(all).run(&mut cell);
    let item = cell.submit(&Work::run_for(RUN).give_up_after(secs(20)));
    cell.run_until(FarmTime::from_millis(60_000))
        .expect("every invariant holds");
    let heard = &cell.observations().items[&item].heard;
    let events: Vec<&ClientEvent> = heard.iter().map(|heard| &heard.event).collect();
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match event {
            ClientEvent::Submitted => "submitted",
            ClientEvent::Pending { .. } => "pending",
            ClientEvent::Queued { .. } => "queued",
            ClientEvent::Placed { .. } => "placed",
            ClientEvent::Started => "started",
            ClientEvent::Terminal { .. } => "terminal",
            ClientEvent::ReleaseSent => "release-sent",
            ClientEvent::Released => "released",
        })
        .collect();
    assert_eq!(kinds, ["submitted", "placed", "terminal", "released"]);
    let gave_up = ClientEvent::Terminal {
        how: HowEnded::GaveUp,
    };
    assert_eq!(*events[2], gave_up);
    assert!(
        cell.ledger().launches().is_empty(),
        "no start reached the host"
    );
}
