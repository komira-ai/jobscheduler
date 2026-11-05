//! The machinery the fault family added, each part shown to do what it says and to fail
//! when it should: a server node restarted over its own files, and one whose restart is
//! refused and is down until a restart succeeds; a refused restart in a scenario,
//! reported as a failure to start; an agent killed for good, which is a host gone and not
//! a fault; the count of a cut agent that runs a run it never listed; the expectation
//! that hosts launched work after a time; and the verdicts the agents read, in the cell's
//! ledger.
//!
//! Sizes: one or two hosts beating every 5 s for at most two minutes, a few hundred agent
//! requests a test.

#[path = "cell/support.rs"]
mod support;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use axum::Router;
use js_sim::{Event, Faults, Node, NodeInput, Output};
use js_sim_cell::actions::{agents, partition_agents, restart_server};
use js_sim_cell::bus::{CallId, Route, agent_name};
use js_sim_cell::server::JOIN_TOKEN;
use js_sim_cell::{
    Action, AgentNode, Cell, CellNode, Counter, Expect, Msg, RunId, Scenario, ScenarioFailure,
    ServerNode, ServerSpec, ServerView, Setup, Shape, Verdict, Work,
};
use js_types::sched::{FarmTime, StateMachine};
use serde_json::json;

use crate::support::{call, lease, ms, scratch, start_run};

/// How long the run of each test lasts, and when the scenarios here settle.
const RUN_MS: u64 = 10_000;
const SETTLE: Duration = Duration::from_secs(60);

/// The flag the server names when its lease file is still locked by another server.
const LOCKED: &str = "--state-file";

fn pool(hosts: usize) -> ServerSpec {
    ServerSpec::registered_pool("burst", Shape::gpu_host(), hosts)
}

/// A cell of `hosts` correct agents on the server's default heartbeat.
fn cell(name: &str, hosts: usize) -> Cell {
    let mut cell = Cell::new(Setup::new(3), pool(hosts), scratch(name), Faults::default())
        .expect("the cell starts");
    for _ in 0..hosts {
        cell.add_agent().expect("an agent name is a host id");
    }
    cell
}

fn run_to(cell: &mut Cell, at: u64) {
    cell.run_until(ms(at)).expect("no violation");
}

/// The index of the agent of the host `name`.
fn index_of(name: &str) -> usize {
    let index = name.trim_start_matches('h').parse::<usize>();
    index.expect("a host named by its index")
}

/// An action that takes a clone of the server's agent router, which holds the lease
/// table and its lock on the lease file, and keeps it for the life of the action.
#[derive(Clone, Default)]
struct HoldLeaseFile(Rc<RefCell<Option<Router>>>);

impl Action for HoldLeaseFile {
    fn run(&self, cell: &mut Cell) {
        let server = cell.server().and_then(ServerNode::server);
        *self.0.borrow_mut() = server.map(|server| server.agent_router());
    }
}

/// Asserts that a server node whose restart is refused (a router of the old server still
/// holds the lease file) names the flag at fault and is down: an agent's call goes
/// unanswered, a client's call is answered 503 with no body, a tick only arms the next,
/// and the listing is empty; that a second restart, once the router is dropped, brings
/// it up again over the same files, answering calls. Catches a restart that loses the
/// server's directory (the lease file a resumed boot requires), a node that answers while
/// down, and one that cannot come back up.
#[test]
fn a_server_whose_restart_is_refused_is_down_until_a_restart_succeeds() {
    let spec = pool(1);
    let server = ServerNode::start(&spec, scratch("restart-node")).expect("starts");
    let mut node = CellNode::Server(Box::new(server));
    let server = node.as_server_mut().expect("the server");
    let held = server.server().expect("up").agent_router();
    let refused = server.restart(&spec).expect_err("the lease file is locked");
    assert_eq!(refused.flag(), LOCKED, "{refused}");
    assert!(!server.is_up());
    assert!(server.server().is_none() && server.registry().is_none());
    assert_eq!(server.view(), ServerView::default());
    let empty = json!({});
    let answer = server.client_call(ms(1), support::request("GET", "/v1/leases", &empty));
    assert_eq!(
        (answer.status().as_u16(), answer.body().as_str()),
        (503, "")
    );
    let input = |at: u64, event| NodeInput {
        now: FarmTime::from_millis(at),
        entropy: 0,
        event,
    };
    let hello = Msg::Request {
        id: CallId(0),
        route: Route::Hello,
        bearer: JOIN_TOKEN.to_owned(),
        body: "{}".to_owned(),
    };
    let from = agent_name(0);
    node.apply(input(2, Event::Message { from, msg: hello }));
    assert!(
        node.take_outputs().is_empty(),
        "a down server answers nothing"
    );
    node.apply(input(5_000, Event::Timer { tag: 0 }));
    let outputs = node.take_outputs();
    assert!(matches!(outputs.as_slice(), [Output::Timer { tag: 0, .. }]));
    drop(held);
    let server = node.as_server_mut().expect("the server");
    server.restart(&spec).expect("starts again");
    assert!(server.is_up());
    let answer = server.client_call(ms(6_000), support::request("GET", "/v1/leases", &empty));
    assert_eq!(answer.status().as_u16(), 200, "{}", answer.body());
}

/// Asserts that a restart in a scenario that the server refuses (an action holds a router
/// of the old server, so the lease file stays locked) fails the seed as a failure to
/// start, naming the scenario, the seed and the flag at fault, in place of the
/// violations of the down server that follow it. Catches a refused restart that is lost,
/// or reported as an invariant the scenario did not break.
#[test]
fn a_restart_the_server_refuses_fails_the_seed_as_a_failure_to_start() {
    let hold = HoldLeaseFile::default();
    let failure = Scenario::new("a_refused_restart")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(1))
        .hosts("burst", 1)
        .work(1, Work::run_for(Duration::from_millis(RUN_MS)))
        .at(Duration::from_secs(20), hold.clone())
        .at(Duration::from_secs(21), restart_server())
        .settle_within(SETTLE)
        .try_run_seeds(1)
        .expect_err("a refused restart fails the seed");
    match &failure {
        ScenarioFailure::Start {
            scenario,
            seed,
            error,
        } => assert_eq!(
            (scenario.as_str(), *seed, error.flag()),
            ("a_refused_restart", 0, LOCKED)
        ),
        other => panic!("not a failure to start: {other}"),
    }
    assert!(hold.0.borrow().is_some(), "the action held a router");
}

/// Asserts that a restart of the server in a cell keeps the agents, the client and the
/// ledger: the agent that ran a run before the restart is answered `unknown-host` by the
/// new server, says hello again and is listed live, the run is launched once, the
/// verdict the agent read on its result is in the cell's ledger, and no restart was
/// refused. Catches a restart that drops the agents, replays the ledger, or leaves the
/// new server unreachable from the agents.
#[test]
fn a_restarted_server_keeps_the_agents_and_the_ledger() {
    let mut cell = cell("restart-cell", 1);
    run_to(&mut cell, 10_000);
    let lease = lease(&mut cell).expect("a lease is granted");
    let run = start_run(&mut cell, &lease, RUN_MS).expect("a run is queued");
    run_to(&mut cell, 40_000);
    let verdicts = cell.ledger().verdicts();
    let judged: Vec<(RunId, Verdict)> = verdicts
        .iter()
        .map(|a| (a.run.clone(), a.verdict))
        .collect();
    assert_eq!(judged, [(RunId::new(run), Verdict::Accepted)]);
    let (status, _) = call(
        &mut cell,
        "DELETE",
        &format!("/v1/leases/{lease}"),
        &json!({}),
    );
    assert!((200..300).contains(&status), "release answered {status}");
    cell.restart_server();
    assert!(cell.take_refused().is_none());
    assert!(cell.server().is_some_and(ServerNode::is_up));
    run_to(&mut cell, 60_000);
    let counters = cell.counters(0).expect("an agent");
    assert!(counters.get(Counter::UnknownHost) >= 1, "{counters:?}");
    let host = js_types::HostId::new("h000").expect("a host id");
    let listed = cell.observations().server.hosts.get(&host);
    assert!(listed.is_some_and(|host| !host.lost), "listed live again");
    assert_eq!(cell.ledger().launches().len(), 1);
}

/// Asserts that an agent killed for good leaves the cell's observations and is noted as
/// killed, but is not a fault: the quiet starts at the kill, while an agent killed to be
/// replaced is one; that its host has no process and runs nothing unlisted; and that
/// killing for good an agent that does not exist changes nothing. Catches a host gone
/// for good that keeps a scenario from ever settling, and a kill the ledger of kills
/// misses.
#[test]
fn an_agent_killed_for_good_is_a_host_gone_and_not_a_fault() {
    let mut cell = cell("gone", 2);
    run_to(&mut cell, 10_000);
    cell.kill_for_good(0);
    cell.kill_for_good(99);
    let observations = cell.observations();
    assert!(!observations.fault_active);
    assert_eq!(observations.quiet_since, ms(10_000));
    let host = js_types::HostId::new("h000").expect("a host id");
    assert!(!observations.agents.contains_key(&host));
    let kills: Vec<(String, u64)> = observations
        .kills
        .iter()
        .map(|kill| (kill.host.as_str().to_owned(), kill.at.as_millis()))
        .collect();
    assert_eq!(kills, [("h000".to_owned(), 10_000)]);
    assert_eq!(cell.agent(0).map(AgentNode::runs_unlisted), Some(false));
    run_to(&mut cell, 40_000);
    assert!(cell.observations().server.hosts[&host].lost);
    cell.kill(1);
    assert!(cell.observations().fault_active);
}

/// Asserts that a cut counts `cut_with_unlisted_run` once for an agent that runs a run
/// it received after its last heartbeat (no hello or heartbeat of its own listed it), and
/// not for an agent that runs nothing; and that once the agent has sent a heartbeat
/// listing the run, it no longer runs a run it never listed. Catches a count that reads
/// what the server saw rather than what the agent sent, and one bumped for an idle host.
#[test]
fn a_cut_agent_that_runs_a_run_it_never_listed_is_counted() {
    let mut cell = cell("cut", 2);
    run_to(&mut cell, 20_000);
    let lease = lease(&mut cell).expect("a lease is granted");
    let (_, listing) = call(&mut cell, "GET", "/v1/leases", &json!({}));
    let host = listing["leases"][0]["host"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    start_run(&mut cell, &lease, 60_000).expect("a run is queued");
    run_to(&mut cell, 22_000);
    let index = index_of(&host);
    assert_eq!(cell.agent(index).map(AgentNode::runs_unlisted), Some(true));
    assert_eq!(
        cell.agent(1 - index).map(AgentNode::runs_unlisted),
        Some(false)
    );
    partition_agents(agents(0..2), Duration::from_secs(10)).run(&mut cell);
    let cut = |cell: &Cell, i: usize| cell.counters(i).map(|c| c.get(Counter::CutWithUnlistedRun));
    assert_eq!(
        (cut(&cell, index), cut(&cell, 1 - index)),
        (Some(1), Some(0))
    );
    run_to(&mut cell, 27_000);
    assert_eq!(cell.agent(index).map(AgentNode::runs_unlisted), Some(false));
}

/// Asserts that `launched_on` holds for a host that launched a run at or after its time,
/// and that for hosts that launched nothing after it the scenario fails naming the time,
/// the hosts wanted and those that did. Catches an expectation that reads launches before
/// its time, and one that passes with no launch at all.
#[test]
fn launched_on_names_the_hosts_that_launched_nothing_after_its_time() {
    let scenario = |after: Duration| {
        Scenario::new("launched_on")
            .scratch(env!("CARGO_TARGET_TMPDIR"))
            .server(pool(2))
            .hosts("burst", 2)
            .work(1, Work::run_for(Duration::from_millis(RUN_MS)))
            .expect(Expect::launched_on(&[0, 1], after))
            .settle_within(SETTLE)
    };
    let failure = scenario(Duration::from_secs(30))
        .try_run_seeds(1)
        .expect_err("no launch after 30 s");
    let ScenarioFailure::Unmet { detail, .. } = &failure else {
        panic!("not an unmet expectation: {failure}");
    };
    assert_eq!(
        detail,
        "launched-on(after 30000): wanted [h000; h001], got []"
    );
    let one = Scenario::new("launched_on_one")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(1))
        .hosts("burst", 1)
        .work(1, Work::run_for(Duration::from_millis(RUN_MS)))
        .expect(Expect::launched_on(&[0], Duration::ZERO))
        .settle_within(SETTLE);
    one.run_seeds(1);
}
