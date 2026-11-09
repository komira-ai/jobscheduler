//! A fleet of two releases: hosts whose agents are the previous release's (v0.1.213) beside
//! hosts of this one, under every invariant of `Invariant::ALL`.
//!
//! The previous release's agent is a profile of the cell's agent ([`AgentProfile`]): the
//! wire has not changed since v0.1.208, so what differs is the hello it sends (its agent
//! version, its asking for a host credential and its capability set), which a test here
//! holds to the recording of that release (`crates/js-agent/tests/agent/skew/v0.1.213.json`).
//! The scenarios show the server taking both releases' agents at once and through an
//! upgrade of each, host by host, while jobs run; one shows that a hello the server cannot
//! take fails the same kind of scenario.
//!
//! Sizes, in agent requests a seed: the mixed fleet, eight hosts for [`MIXED_SETTLE`], about
//! 1,900; the upgrade, six hosts for [`UPGRADE_SETTLE`], about 1,400; the refusal, four
//! hosts, about 700; each over [`SEEDS`] seeds (the refusal over one).

use std::collections::BTreeMap;
use std::time::Duration;

use js_sim::{Event, Node, NodeId, NodeInput, Output};
use js_sim_cell::actions::{agents, upgrade_agents};
use js_sim_cell::bus::SERVER;
use js_sim_cell::{
    AgentKind, AgentNode, AgentProfile, AgentSpec, CallId, Expect, HowEnded, Ledger, Msg, Route,
    Scenario, ScenarioFailure, ServerSpec, Shape, Work,
};
use js_types::sched::{FarmTime, StateMachine};
use js_types::{HostId, Resources};
use serde_json::Value;

/// How many seeds each scenario runs.
const SEEDS: u64 = 8;

/// The previous release's recording.
const RECORDING: &str = include_str!("../../js-agent/tests/agent/skew/v0.1.213.json");

/// The hosts of the mixed fleet: the first half are of the previous release.
const MIXED_HOSTS: usize = 8;

/// How long the mixed fleet runs after its jobs are submitted: a whole-host job queues
/// behind the others on a host, and each run is [`RUN`].
const MIXED_SETTLE: Duration = Duration::from_secs(1_200);

/// How long every job runs.
const RUN: Duration = Duration::from_secs(60);

/// The hosts of the upgrade, all of the previous release at the start.
const UPGRADE_HOSTS: usize = 6;

/// How long after the start the first host is upgraded, and the gap between two.
const UPGRADE_FIRST: Duration = Duration::from_secs(20);
const UPGRADE_GAP: Duration = Duration::from_secs(25);

/// How long the upgrade runs after the start: a host restarted under a job reads lost
/// before its new process is welcomed.
const UPGRADE_SETTLE: Duration = Duration::from_secs(900);

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A pool of `hosts` hosts a whole-host job can start on.
fn pool(hosts: usize) -> ServerSpec {
    ServerSpec::whole_node_pool("burst", Shape::cpu_host(), hosts)
}

/// The recording's flow entries the agent sent, as `(path, body)`.
fn recorded() -> Vec<(String, Value)> {
    let recording: Value = serde_json::from_str(RECORDING).expect("the recording is JSON");
    let flow = recording["flow"].as_array().expect("a flow");
    flow.iter()
        .filter_map(|entry| entry.get("agent"))
        .map(|call| {
            let path = call["path"].as_str().expect("a path").to_owned();
            (path, call["body"].clone())
        })
        .collect()
}

/// The agent of `profile` on host `h1`, 8 cores and 32 GiB, as the recording's host,
/// without its GPUs.
fn node(profile: AgentProfile) -> AgentNode {
    let spec = AgentSpec {
        host: HostId::new("h1").expect("host id"),
        pool: "macs".to_owned(),
        join_token: "join-token-sim-0123456789-abcdefghij".to_owned(),
        capacity: Resources::new(8_000, 32 << 30, 0, None).expect("capacity"),
        capabilities: BTreeMap::from([("per-job".to_owned(), "true".to_owned())]),
        labels: BTreeMap::new(),
        hello_interval: secs(1),
        kind: AgentKind::Fake(None),
        instance: None,
        worker_lag: Duration::ZERO,
    };
    let mut node = AgentNode::new(spec, Ledger::default());
    node.set_profile(profile);
    node
}

/// Feeds `event` to `node` at `at` ms, and returns what it sent as requests and what
/// timers it armed.
fn feed(node: &mut AgentNode, at: u64, event: Event<Msg>) -> (Vec<(Route, Value)>, Vec<u64>) {
    node.apply(NodeInput {
        now: FarmTime::from_millis(at),
        entropy: 0,
        event,
    });
    let mut calls = Vec::new();
    let mut timers = Vec::new();
    for output in node.take_outputs() {
        match output {
            Output::Send {
                msg: Msg::Request { route, body, .. },
                ..
            } => calls.push((route, serde_json::from_str(&body).expect("a JSON body"))),
            Output::Timer { tag, .. } => timers.push(tag),
            _ => {}
        }
    }
    (calls, timers)
}

/// The names of an object's fields.
fn names(value: &Value) -> Vec<String> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect()
}

/// Asserts that an agent of the previous-release profile says the hello the recording of
/// v0.1.213 holds, field by field (the host's id, pool, shape and GPUs are the host's, and
/// the recorded host has two), that the first heartbeat of its session has the recorded
/// heartbeat's fields, and that the current profile's hello differs from it in the agent
/// version, the credential flag and the capabilities. Catches a profile that drifts from
/// the recording (a version, a capability or the credential flag changed), and a profile
/// that changes nothing.
#[test]
fn the_previous_release_hello_is_the_recorded_one_field_by_field() {
    let recorded = recorded();
    let hello = &recorded[0];
    assert_eq!(hello.0, "/v1/agent/hello");
    let heartbeat = recorded
        .iter()
        .find(|(path, _)| path == "/v1/agent/heartbeat")
        .expect("a recorded heartbeat");

    let mut previous = node(AgentProfile::PreviousRelease);
    assert_eq!(previous.profile(), AgentProfile::PreviousRelease);
    let (calls, _) = feed(&mut previous, 0, Event::Start);
    let (route, ours) = &calls[0];
    assert_eq!(*route, Route::Hello);
    let host_bound = ["host_id", "pool", "shape", "gpu_devices"];
    let kept = |value: &Value| {
        let mut keys = names(value);
        keys.retain(|key| !host_bound.contains(&key.as_str()));
        keys
    };
    for key in kept(&hello.1) {
        assert_eq!(ours.get(&key), hello.1.get(&key), "the hello's {key}");
    }
    assert_eq!(kept(ours), kept(&hello.1));

    // The welcome the recording's server sent, and the first heartbeat after it.
    let recording: Value = serde_json::from_str(RECORDING).expect("JSON");
    let welcome = recording["flow"][0]["server"]["body"].to_string();
    let reply = Msg::Reply {
        id: CallId(0),
        status: 200,
        retry_after: None,
        body: welcome,
    };
    let from = Event::Message {
        from: NodeId::new(SERVER),
        msg: reply,
    };
    let (_, timers) = feed(&mut previous, 10, from);
    let (beats, _) = feed(&mut previous, 20, Event::Timer { tag: timers[0] });
    let (route, beat) = &beats[0];
    assert_eq!(*route, Route::Heartbeat);
    assert_eq!(names(beat), names(&heartbeat.1));

    let mut current = node(AgentProfile::Current);
    let (calls, _) = feed(&mut current, 0, Event::Start);
    let body = &calls[0].1;
    let differs: Vec<&str> = ["agent_version", "accepts_host_credential", "capabilities"]
        .into_iter()
        .filter(|key| body.get(key) != ours.get(key))
        .collect();
    assert_eq!(
        differs,
        ["agent_version", "accepts_host_credential", "capabilities"]
    );
    assert_eq!(body.get("accepts_host_credential"), None);
}

/// Asserts that a fleet of four previous-release hosts and four current ones, over a pool
/// of eight, runs six whole-host jobs and twelve ordinary jobs to a terminal state each,
/// every job launched once, nothing left booked, and every host serving at the end, under
/// all invariants. Catches a server that cannot place on, or loses the jobs of, a host
/// whose hello is the previous release's, and a profile that does not reach the hello (the
/// agents would be the same).
#[test]
fn a_half_previous_release_fleet_runs_ordinary_and_whole_host_jobs_to_the_end() {
    let jobs = 18;
    Scenario::new("a_half_previous_release_fleet_runs_ordinary_and_whole_host_jobs_to_the_end")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(MIXED_HOSTS))
        .previous_release_hosts("burst", MIXED_HOSTS / 2)
        .hosts("burst", MIXED_HOSTS / 2)
        .work(12, Work::run_for(RUN))
        .work(6, Work::whole_host(RUN))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(jobs, HowEnded::Succeeded))
        .expect(Expect::jobs_read(jobs))
        .expect(Expect::jobs_ended(jobs))
        .expect(Expect::each_job_launched_once())
        .expect(Expect::no_allocation_live())
        .expect(Expect::every_host_serving())
        .settle_within(MIXED_SETTLE)
        .run_seeds(SEEDS);
}

/// Asserts that upgrading six previous-release hosts one by one, 25 s apart, while twelve
/// ordinary and four whole-host jobs run leaves every job terminal exactly once, none
/// launched twice, no allocation live and every host serving at the end, under all
/// invariants. An upgrade restarts the agent, so a job on the host it restarts may end
/// lost; the scenario does not say which. Catches a server that double-places or strands a
/// job when a host's hello changes release under it, and an upgrade that does not restart
/// the process (the hosts would never read lost).
#[test]
fn upgrading_every_agent_one_by_one_while_jobs_run_ends_each_job_once() {
    let jobs = 16;
    let mut scenario =
        Scenario::new("upgrading_every_agent_one_by_one_while_jobs_run_ends_each_job_once")
            .scratch(env!("CARGO_TARGET_TMPDIR"))
            .server(pool(UPGRADE_HOSTS))
            .previous_release_hosts("burst", UPGRADE_HOSTS)
            .work(12, Work::run_for(secs(120)))
            .work(4, Work::whole_host(secs(120)));
    for host in 0..UPGRADE_HOSTS {
        let after = UPGRADE_FIRST + UPGRADE_GAP * u32::try_from(host).expect("a small index");
        scenario = scenario.at(after, upgrade_agents(agents(host..host + 1)));
    }
    scenario
        .expect(Expect::all_items_terminal())
        .expect(Expect::jobs_read(jobs))
        .expect(Expect::jobs_ended(jobs))
        .expect(Expect::each_job_launched_once())
        .expect(Expect::no_allocation_live())
        .expect(Expect::every_host_serving())
        .settle_within(UPGRADE_SETTLE)
        .run_seeds(SEEDS);
}

/// Asserts that the same kind of half-and-half fleet fails when its two old hosts say a
/// hello the server refuses (protocol majors 2 to 3, 400 `unsupported-protocol`): they are
/// never welcomed, and the scenario's expectation that every host is serving is unmet, by
/// exact detail. Catches a profile that is not exercised (the hosts would join and the
/// scenario would pass), and a profile whose hello does not reach the server.
#[test]
fn a_previous_release_host_the_server_cannot_take_fails_the_scenario() {
    let failure = Scenario::new("a_previous_release_host_the_server_cannot_take_fails")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(4))
        .hosts_as("burst", 2, AgentProfile::ProtocolAhead)
        .hosts("burst", 2)
        .work(4, Work::run_for(RUN))
        .expect(Expect::all_items_terminal())
        .expect(Expect::every_host_serving())
        .settle_within(secs(600))
        .try_run_seeds(1);
    match failure {
        Err(ScenarioFailure::Unmet { detail, seed, .. }) => {
            assert_eq!(
                (detail.as_str(), seed),
                ("every-host-serving: wanted 4, got 2", 0)
            );
        }
        other => panic!("not an unmet expectation: {other:?}"),
    }
}
