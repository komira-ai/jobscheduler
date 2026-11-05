//! The scenario builder and the lease front, each shown to fail when it should: a witness
//! no seed satisfies, an expectation not met, a violation and its report, an action at
//! its virtual time, the seeds in order, a fault that never ends, hosts of an unknown
//! pool; the lease front's mapping of every answer to its event, driven through a
//! scripted client port; and a client call that the bus's faults never reach.
//!
//! Sizes: one or two hosts beating every 5 s for at most a few minutes, a few hundred
//! agent requests a seed; the scripted front runs no server at all.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, Response};
use js_conformance::agent::Defect;
use js_sim::{Chance, Faults};
use js_sim_cell::actions::{agents, kill_agents, replace_agents, set_bus_faults, submit_work};
use js_sim_cell::server::CLIENT_TOKEN;
use js_sim_cell::{
    Action, AgentNode, Cell, ClientEvent, ClientPort, Counter, Expect, Front, HostIs, HowEnded,
    Invariant, ItemId, LeaseFront, Scenario, ScenarioFailure, ServerSpec, Setup, Shape, Work,
};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;
use js_types::{HostId, LeaseId, Resources};
use serde_json::{Value, json};

/// The small scenario most tests here vary: one host beating at the server's default,
/// one run of [`RUN`], settled [`SETTLE`] after its last action.
const RUN: Duration = Duration::from_secs(10);
const SETTLE: Duration = Duration::from_secs(120);

/// The front's poll period.
const POLL_MS: u64 = 5_000;

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
    } = &failure
    else {
        panic!("not a witness failure: {failure}");
    };
    assert_eq!(
        (scenario.as_str(), witness.as_str(), *seeds),
        (name, "never", 2)
    );
    assert_eq!(
        failure.to_string(),
        format!("scenario {name}: witness never held in none of 2 seeds")
    );
    let passed = small(name)
        .try_run_seeds(2)
        .expect("passes without the witness");
    assert_eq!(passed.runs.len(), 2);
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
        .front(LeaseFront::factory())
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

/// Asserts that an item that asks for its own resources (half a core, 512 bytes, no GPU)
/// in place of the default share is leased and run with them and succeeds. Catches a
/// front that ignores `Work::needs` (the lease and the run ask for the default share).
#[test]
fn work_that_needs_its_own_resources_is_leased_and_run_with_them() {
    let needs = Resources::new(500, 512, 0, None).expect("resources");
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

/// Asserts that with every message of the agent wire dropped, the client's acquire, its
/// run, its give-up and its release still reach the server and are answered: the item
/// learns its lease and run, is given up when no start can reach the host, and its
/// release is answered. Catches a client that goes through the bus.
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
            ClientEvent::Queued { .. } => "queued",
            ClientEvent::Placed { .. } => "placed",
            ClientEvent::Started => "started",
            ClientEvent::Terminal { .. } => "terminal",
            ClientEvent::ReleaseSent => "release-sent",
            ClientEvent::Released => "released",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "submitted",
            "queued",
            "placed",
            "terminal",
            "release-sent",
            "released"
        ]
    );
    let gave_up = ClientEvent::Terminal {
        how: HowEnded::GaveUp,
    };
    assert_eq!(*events[3], gave_up);
    assert!(
        cell.ledger().launches().is_empty(),
        "no start reached the host"
    );
}

/// A client port that answers from a script and writes each call down.
struct Scripted {
    answers: VecDeque<(u16, Value)>,
    calls: Vec<String>,
}

impl ClientPort for Scripted {
    fn call(&mut self, request: Request<Body>) -> Response<String> {
        let bearer = request.headers().get(AUTHORIZATION).cloned();
        assert_eq!(
            bearer.and_then(|value| value.to_str().ok().map(str::to_owned)),
            Some(format!("Bearer {CLIENT_TOKEN}"))
        );
        self.calls
            .push(format!("{} {}", request.method(), request.uri()));
        let (status, body) = self.answers.pop_front().expect("an answer for every call");
        Response::builder()
            .status(status)
            .body(body.to_string())
            .expect("a response")
    }
}

/// A lease front of the cell's pool.
fn front() -> LeaseFront {
    LeaseFront::new(&ServerSpec::registered_pool("burst", Shape::gpu_host(), 1))
}

/// Polls `front` at `at` (virtual milliseconds) with `answers` as the server's, which
/// must all be taken; returns the events heard (item, event) and the calls made.
fn poll(
    front: &mut LeaseFront,
    at: u64,
    answers: Vec<(u16, Value)>,
) -> (Vec<(u32, ClientEvent)>, Vec<String>) {
    let mut port = Scripted {
        answers: answers.into(),
        calls: Vec::new(),
    };
    let heard = front.poll(FarmTime::from_millis(at), &mut port);
    assert!(
        port.answers.is_empty(),
        "unused answers: {:?}",
        port.answers
    );
    let heard = heard.into_iter().map(|(item, event)| (item.0, event));
    (heard.collect(), port.calls)
}

/// One item of `work` submitted at time zero, polled every [`POLL_MS`] with the answers
/// of each poll in turn: every event it heard, and every call made.
fn one(work: Work, polls: Vec<Vec<(u16, Value)>>) -> (Vec<ClientEvent>, Vec<String>) {
    let mut front = front();
    front.submit(ItemId(0), &work, FarmTime::default());
    let mut events = Vec::new();
    let mut calls = Vec::new();
    for (n, answers) in (0..).zip(polls) {
        let (heard, made) = poll(&mut front, n * POLL_MS, answers);
        events.extend(heard.into_iter().map(|(_, event)| event));
        calls.extend(made);
    }
    assert_eq!(front.next_poll(), None, "the item is done");
    (events, calls)
}

fn granted(id: &str) -> (u16, Value) {
    (201, json!({ "id": id, "host": "h000", "expires_at_ms": 0 }))
}

fn queued(run: &str) -> (u16, Value) {
    (202, json!({ "run_id": run, "state": "queued" }))
}

fn run_state(state: &str) -> (u16, Value) {
    (200, json!({ "run_id": "r", "state": state }))
}

fn run_ended(state: &str, outcome: &str) -> (u16, Value) {
    (
        200,
        json!({ "run_id": "r", "state": state, "outcome": outcome }),
    )
}

fn status(code: u16) -> (u16, Value) {
    (code, json!({ "error": "code" }))
}

fn listed(id: &str, state: &str) -> (u16, Value) {
    let leases = json!([{ "id": id, "state": state, "host": "h000" }]);
    (200, json!({ "leases": leases }))
}

fn terminal(how: HowEnded) -> ClientEvent {
    ClientEvent::Terminal { how }
}

/// The events of an item queued on lease `1-<seq>` as run `run-1`.
fn queued_events(seq: u64) -> [ClientEvent; 2] {
    [
        ClientEvent::Queued {
            lease: LeaseId::new(1, seq),
            run: js_sim_cell::RunId::new("run-1"),
        },
        ClientEvent::Placed {
            host: HostId::new("h000").expect("a host id"),
        },
    ]
}

/// What a full successful item looks like on the wire and in events, and the end of a
/// run as each answer reads, with its release.
fn ended_as(answer: (u16, Value), release: u16) -> Vec<ClientEvent> {
    let work = Work::run_for(RUN);
    let polls = vec![vec![
        granted("1-1"),
        queued("run-1"),
        answer,
        status(release),
    ]];
    one(work, polls).0
}

/// Asserts that each answer of the server produces exactly the event the lease front's
/// module doc says: 201 a held lease and its run queued (`Queued`, `Placed`), 202 a
/// pending lease read from the list until held, 409 a retry with the same ask not tried
/// again in that poll, the run's states (`Started` once; `exited` with `succeeded`,
/// `ended` with `killed`, `exited` with another outcome, `ended` with none,
/// `never-started`), 410 `run-forgotten` and 404, each with its own `HowEnded`, and the
/// release after each (`ReleaseSent` once, `Released` on 200, 202 or 404, a retry on
/// anything else). Catches a status mapped to the wrong `HowEnded`, a second `Started`,
/// a lost release, and a second lease asked for while one is pending.
#[test]
fn the_lease_front_maps_each_server_answer_to_one_event() {
    let [queued_1, placed] = queued_events(1);
    let released = [ClientEvent::ReleaseSent, ClientEvent::Released];
    let path = "/v1/leases/1-1/runs/run-1";

    // 201, then the run read until it succeeds; `Started` is heard once.
    let polls = vec![
        vec![granted("1-1"), queued("run-1"), run_state("queued")],
        vec![run_state("starting")],
        vec![run_state("running")],
        vec![run_state("exited")],
        vec![run_ended("exited", "succeeded"), status(200)],
    ];
    let (events, calls) = one(Work::run_for(RUN), polls);
    let mut wanted = vec![queued_1.clone(), placed.clone(), ClientEvent::Started];
    wanted.push(terminal(HowEnded::Succeeded));
    wanted.extend(released.clone());
    assert_eq!(events, wanted);
    let read = format!("GET {path}");
    let read = read.as_str();
    let calls_wanted = [
        "POST /v1/leases",
        "POST /v1/leases/1-1/runs",
        read,
        read,
        read,
        read,
        read,
        "DELETE /v1/leases/1-1",
    ];
    assert_eq!(calls, calls_wanted);

    // Each terminal state of the run, and the release answered 200, 202 or 404.
    let mut started = vec![queued_1.clone(), placed.clone(), ClientEvent::Started];
    let cases = [
        (run_ended("ended", "killed"), 202, HowEnded::Cancelled),
        (run_ended("exited", "failed"), 404, HowEnded::Failed),
    ];
    for (answer, release, how) in cases {
        let mut wanted = started.clone();
        wanted.push(terminal(how));
        wanted.extend(released.clone());
        assert_eq!(ended_as(answer, release), wanted, "{how:?}");
    }
    started.truncate(2);
    let cases = [
        (run_state("ended"), HowEnded::Lost),
        (run_state("never-started"), HowEnded::Failed),
        (status(410), HowEnded::Forgotten),
        (status(404), HowEnded::Lost),
    ];
    for (answer, how) in cases {
        let mut wanted = started.clone();
        wanted.push(terminal(how));
        wanted.extend(released.clone());
        assert_eq!(ended_as(answer, 200), wanted, "{how:?}");
    }

    // 202 pending: the list read until the lease is held; a failed read waits.
    let polls = vec![
        vec![(202, json!({ "id": "1-1", "state": "pending" }))],
        vec![status(503)],
        vec![listed("1-1", "pending")],
        vec![listed("1-1", "held"), queued("run-1"), status(500)],
        vec![run_ended("exited", "succeeded"), status(200)],
    ];
    let (events, calls) = one(Work::run_for(RUN), polls);
    let mut wanted = vec![queued_1.clone(), placed.clone(), ClientEvent::Started];
    wanted.push(terminal(HowEnded::Succeeded));
    wanted.extend(released.clone());
    assert_eq!(events, wanted);
    assert_eq!(calls.iter().filter(|c| *c == "POST /v1/leases").count(), 1);

    // A pending lease that is gone is asked for again at once; a refusal of the request
    // fails the item.
    let polls = vec![
        vec![(202, json!({ "id": "1-1", "state": "pending" }))],
        vec![(200, json!({ "leases": [] })), status(400)],
    ];
    let (events, _) = one(Work::run_for(RUN), polls);
    assert_eq!(events, [terminal(HowEnded::Failed)]);

    // A queue refused 409 is tried again; any other refusal fails the item, and its
    // release is sent again until it is answered.
    let polls = vec![
        vec![granted("1-1"), status(409)],
        vec![status(422), status(500)],
        vec![status(200)],
    ];
    let (events, calls) = one(Work::run_for(RUN), polls);
    let mut wanted = vec![terminal(HowEnded::Failed)];
    wanted.extend(released.clone());
    assert_eq!(events, wanted);
    let release = calls
        .iter()
        .filter(|c| *c == "DELETE /v1/leases/1-1")
        .count();
    assert_eq!(release, 2);

    // 409 for want of capacity: the second item asking the same is not tried in that
    // poll; both are tried again at the next.
    let mut front = front();
    let work = Work::run_for(RUN);
    front.submit(ItemId(0), &work, FarmTime::default());
    front.submit(ItemId(1), &work, FarmTime::default());
    assert_eq!(front.next_poll(), Some(FarmTime::default()));
    let (heard, calls) = poll(&mut front, 0, vec![status(409)]);
    assert!(heard.is_empty());
    assert_eq!(calls, ["POST /v1/leases"]);
    assert_eq!(front.next_poll(), Some(FarmTime::from_millis(POLL_MS)));
    let (heard, _) = poll(&mut front, POLL_MS, vec![status(409)]);
    assert!(heard.is_empty());
}

/// Asserts what a cancel does at each stage, as the lease front's module doc says: before
/// a lease, `Cancelled` with nothing to release; with a pending or held lease,
/// `Cancelled` and the release; with a queued run, the run's stop sent once and the
/// run's end read as `Cancelled` (`ended` with no outcome, `never-started`), or
/// `Forgotten` when the stop is answered 410; and that a cancel of an item the front does
/// not hold changes nothing. Catches a cancel that leaks a lease, sends the stop twice,
/// or reports a cancelled run as lost or failed.
#[test]
fn the_lease_front_cancels_each_stage_as_its_module_doc_says() {
    let released = [ClientEvent::ReleaseSent, ClientEvent::Released];
    let at = FarmTime::default;
    let work = Work::run_for(RUN);

    // Before a lease.
    let mut front = front();
    front.cancel(ItemId(9), at());
    assert_eq!(front.next_poll(), None, "an item it does not hold");
    front.submit(ItemId(0), &work, at());
    front.cancel(ItemId(0), at());
    let (heard, calls) = poll(&mut front, 0, Vec::new());
    assert_eq!(heard, [(0, terminal(HowEnded::Cancelled))]);
    assert!(calls.is_empty());
    assert_eq!(front.next_poll(), None);

    // With a pending lease.
    let mut front = front_with(&work);
    poll(&mut front, 0, vec![(202, json!({ "id": "1-1" }))]);
    front.cancel(ItemId(0), FarmTime::from_millis(1));
    let (heard, calls) = poll(&mut front, 1, vec![status(200)]);
    let mut wanted = vec![(0, terminal(HowEnded::Cancelled))];
    wanted.extend(released.clone().map(|event| (0, event)));
    assert_eq!(heard, wanted);
    assert_eq!(calls, ["DELETE /v1/leases/1-1"]);

    // With a queued run: the stop once, then the run's end.
    let stop = "DELETE /v1/leases/1-1/runs/run-1";
    let read = "GET /v1/leases/1-1/runs/run-1";
    for (end, how) in [
        (run_state("ended"), HowEnded::Cancelled),
        (run_state("never-started"), HowEnded::Cancelled),
    ] {
        let mut front = front_with(&work);
        poll(
            &mut front,
            0,
            vec![granted("1-1"), queued("run-1"), run_state("queued")],
        );
        front.cancel(ItemId(0), FarmTime::from_millis(1));
        assert_eq!(front.next_poll(), Some(FarmTime::from_millis(1)));
        let (heard, calls) = poll(&mut front, 1, vec![status(202), run_state("stopping")]);
        assert!(heard.is_empty());
        assert_eq!(calls, [stop, read]);
        let (heard, calls) = poll(&mut front, POLL_MS, vec![end, status(200)]);
        let mut wanted = vec![(0, terminal(how))];
        wanted.extend(released.clone().map(|event| (0, event)));
        assert_eq!(heard, wanted);
        assert_eq!(calls, [read, "DELETE /v1/leases/1-1"]);
    }

    // A stop answered 410: the run is forgotten.
    let mut front = front_with(&work);
    poll(
        &mut front,
        0,
        vec![granted("1-1"), queued("run-1"), run_state("queued")],
    );
    front.cancel(ItemId(0), FarmTime::from_millis(1));
    let (heard, calls) = poll(&mut front, 1, vec![status(410), status(200)]);
    let mut wanted = vec![(0, terminal(HowEnded::Forgotten))];
    wanted.extend(released.clone().map(|event| (0, event)));
    assert_eq!(heard, wanted);
    assert_eq!(calls, [stop, "DELETE /v1/leases/1-1"]);

    // With a held lease whose run is not queued yet.
    let mut front = front_with(&work);
    poll(&mut front, 0, vec![granted("1-1"), status(409)]);
    front.cancel(ItemId(0), FarmTime::from_millis(1));
    let (heard, _) = poll(&mut front, 1, vec![status(200)]);
    assert_eq!(heard[0], (0, terminal(HowEnded::Cancelled)));
}

/// A front holding one item of `work`, submitted at time zero.
fn front_with(work: &Work) -> LeaseFront {
    let mut front = front();
    front.submit(ItemId(0), work, FarmTime::default());
    front
}

/// Asserts that an item not terminal by its give-up time is given up at the first poll
/// at or after it, `GaveUp`, and its lease released, whether it waits for a lease (no
/// release) or its run is queued (the release); and that the lease asked for and the run
/// queued carry the work's give-up time and duration. Catches an item waited for past
/// its bound, or given up with its lease held.
#[test]
fn the_lease_front_gives_up_an_item_at_its_give_up_time_and_releases_its_lease() {
    let work = Work::run_for(RUN).give_up_after(Duration::from_millis(7_000));
    let polls = vec![
        vec![granted("1-1"), queued("run-1"), run_state("queued")],
        vec![run_state("running")],
        vec![status(200)],
    ];
    let (events, _) = one(work.clone(), polls);
    let [queued_1, placed] = queued_events(1);
    let wanted = [
        queued_1,
        placed,
        ClientEvent::Started,
        terminal(HowEnded::GaveUp),
        ClientEvent::ReleaseSent,
        ClientEvent::Released,
    ];
    assert_eq!(events, wanted);
    let polls = vec![vec![status(409)], vec![status(409)], Vec::new()];
    let (events, _) = one(work, polls);
    assert_eq!(events, [terminal(HowEnded::GaveUp)]);
}
