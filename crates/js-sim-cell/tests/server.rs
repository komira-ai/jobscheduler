//! The real lease server with real agents in the cell: registration through the real
//! hello and its limiter, loss and forgetting by the real registry, a lease and a run
//! made through the client router, started, ended and acknowledged on the real wire, and
//! the store the server keeps its state in (`--store`): the file the cell hands it, a host
//! joined with a minted token, a read of the store while the server is down, and a second
//! server on the same store.
//!
//! Each scenario names its host count, horizon and heartbeat interval below. The bus
//! takes 1 to 10 ms a message unless a scenario says otherwise, and drops nothing.

#[path = "cell/support.rs"]
mod support;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use js_conformance::agent::Defect;
use js_lease::{ManualClock, Timestamp};
use js_registry::HostState;
use js_sim::{Event, Faults, Node, NodeInput};
use js_sim_cell::bus::{agent_name, trace_msg};
use js_sim_cell::server::{CLIENT_TOKEN, SQLITE_FILE, STORE_LINE_FILE};
use js_sim_cell::{
    AgentNode, AgentSpec, CallId, Cell, CellNode, Checker, Counter, EPOCH_MS, Invariant, Ledger,
    Msg, Route, RunId, SecondServer, ServerNode, ServerSpec, Setup, Shape, Store, StoreRead,
    Verdict,
};
#[cfg(feature = "postgres")]
use js_testkit::database::{server_url, with_database};
use js_types::sched::{FarmTime, StateMachine};
use serde_json::json;

use crate::support::{call, lease, ms, scratch, start_run};

/// Registration: as many hosts as one join token's default burst would refuse some of,
/// heard within this horizon.
const REGISTER_HOSTS: usize = 64;
const REGISTER_HORIZON_MS: u64 = 2_500;

/// The limiter: two hosts and a burst of one.
const LIMITED_HOSTS: usize = 2;
const LIMITED_HORIZON_MS: u64 = 5_000;

/// Forgetting: two hosts, one cut off past its grace and the pool's `lost_retention`.
const FORGET_HOSTS: usize = 2;
const LOST_RETENTION: Duration = Duration::from_secs(120);
const FORGET_CUT_MS: (u64, u64) = (10_000, 160_000);
const FORGET_HORIZON_MS: u64 = 175_000;

/// The lease and its run: one host, a run of three seconds.
const RUN_HOSTS: usize = 1;
const RUN_MS: u64 = 3_000;
const RUN_HORIZON_MS: u64 = 60_000;

/// The start sent again: one host beating every second, held lost after three, and a bus
/// slower than the interval each way, so a heartbeat leaves before the answer to the one
/// that took the start comes back.
const RESEND_HOSTS: usize = 1;
const RESEND_INTERVAL: Duration = Duration::from_secs(1);
const RESEND_GRACE: Duration = Duration::from_secs(3);
const RESEND_DELAY: Duration = Duration::from_millis(1_500);
const RESEND_RUN_MS: u64 = 200;
const RESEND_HORIZON_MS: u64 = 40_000;

/// Loss: two hosts, one cut off for longer than the grace.
const LOST_HOSTS: usize = 2;
const LOST_CUT_MS: (u64, u64) = (10_000, 40_000);
const LOST_HORIZON_MS: u64 = 50_000;

/// Processes: two hosts, one killed and replaced.
const KILL_HOSTS: usize = 2;
const KILL_AT_MS: u64 = 10_000;
const REPLACE_AT_MS: u64 = 40_000;
const KILL_HORIZON_MS: u64 = 50_000;

/// Every scenario here beats at the server's default interval and grace, but the resend.
const INTERVAL: Duration = Duration::from_secs(5);
const GRACE: Duration = Duration::from_secs(15);

/// A cell of `spec` with `hosts` correct agents and `faults` on the bus.
fn cell(name: &str, spec: ServerSpec, hosts: usize, faults: Faults) -> Cell {
    let mut cell = Cell::new(Setup::new(11), spec, scratch(name), faults).expect("starts");
    for _ in 0..hosts {
        cell.add_agent().expect("an agent name is a host id");
    }
    cell
}

/// A pool of `hosts` gpu hosts.
fn pool(hosts: usize) -> ServerSpec {
    ServerSpec::registered_pool("burst", Shape::gpu_host(), hosts)
}

/// Each host the registry lists, with its state.
fn listed(cell: &Cell) -> Vec<(String, HostState)> {
    let hosts = cell.server().expect("a server").hosts();
    let each = |host: js_registry::HostView| (host.id.as_str().to_owned(), host.state);
    hosts.into_iter().map(each).collect()
}

/// How many calls of agent `index` on `route` the server answered `status`.
fn answered(cell: &Cell, index: usize, route: Route, status: u16) -> u64 {
    let server = cell.server().expect("a server");
    server.answered(&agent_name(index), route, status)
}

fn run_to(cell: &mut Cell, at: u64) {
    cell.run_until(ms(at)).expect("every invariant holds");
}

/// Asserts that 64 agents sharing one join token all say hello through the real route and
/// are listed live by the registry within 2.5 s, with no hello refused. Catches a spec that
/// leaves `max_machines` at less than the hosts (the hellos past it are refused
/// `pool-full`) or the hello burst at its default of 60 (four hosts wait a second each).
#[test]
fn agents_register_through_the_real_hello_and_are_listed_by_the_registry() {
    let spec = pool(REGISTER_HOSTS);
    assert_eq!((spec.heartbeat_interval(), spec.grace()), (INTERVAL, GRACE));
    assert_eq!(spec.pool(), "burst");
    let mut cell = cell("register", spec, REGISTER_HOSTS, Faults::default());
    run_to(&mut cell, REGISTER_HORIZON_MS);
    let hosts = listed(&cell);
    assert_eq!(hosts.len(), REGISTER_HOSTS);
    assert!(
        hosts.iter().all(|(_, state)| *state == HostState::Live),
        "{hosts:?}"
    );
    let refused: u64 = (0..REGISTER_HOSTS)
        .map(|i| answered(&cell, i, Route::Hello, 429))
        .sum();
    assert_eq!(refused, 0);
    assert_eq!(cell.observations().server.hosts.len(), REGISTER_HOSTS);
    let server = cell.server().expect("a server").server();
    assert_eq!(server.map(|server| server.agent_errors()), Some(0));
}

/// Asserts that with a burst of one the real limiter refuses one of two hellos with a wait
/// (429 and `Retry-After`), that the refused agent says hello once more after the wait and
/// is welcomed, and that each host is welcomed once. Catches an agent that hammers the
/// server (more than one refusal) or gives up (no second hello).
#[test]
fn a_refused_hello_is_retried_after_the_wait_the_server_names() {
    let spec = pool(LIMITED_HOSTS).hello_burst(1);
    let mut cell = cell("limited", spec, LIMITED_HOSTS, Faults::default());
    run_to(&mut cell, 500);
    assert_eq!(listed(&cell).len(), 1, "one host waits");
    run_to(&mut cell, LIMITED_HORIZON_MS);
    let refused: Vec<u64> = (0..LIMITED_HOSTS)
        .map(|i| answered(&cell, i, Route::Hello, 429))
        .collect();
    assert_eq!(refused.iter().sum::<u64>(), 1, "{refused:?}");
    for i in 0..LIMITED_HOSTS {
        assert_eq!(answered(&cell, i, Route::Hello, 200), 1, "h00{i}");
    }
    let live = listed(&cell)
        .iter()
        .filter(|(_, s)| *s == HostState::Live)
        .count();
    assert_eq!(live, LIMITED_HOSTS);
}

/// Asserts that a host cut off past its grace and the pool's `lost_retention` (120 s) is
/// forgotten by the real registry (and the cell's listing drops it), that after the cut
/// heals its next heartbeat is answered 409 `unknown-host`, and that it then says hello
/// once and is listed live again. Catches an agent that does not follow the 409 rule
/// against the real server, and a listing that keeps a forgotten host.
#[test]
fn a_host_the_server_has_forgotten_is_answered_unknown_host_and_says_hello_again() {
    let spec = pool(FORGET_HOSTS).lost_retention(LOST_RETENTION);
    let mut cell = cell("forget", spec, FORGET_HOSTS, Faults::default());
    let (cut, heal) = FORGET_CUT_MS;
    run_to(&mut cell, cut);
    cell.partition(&[&[0]]);
    run_to(&mut cell, heal - 1);
    let forgotten = listed(&cell);
    assert_eq!(forgotten, vec![("h001".to_owned(), HostState::Live)]);
    let host = js_types::HostId::new("h000").expect("a host id");
    assert!(!cell.observations().server.hosts.contains_key(&host));
    assert_eq!(answered(&cell, 0, Route::Heartbeat, 409), 0);
    cell.heal();
    run_to(&mut cell, FORGET_HORIZON_MS);
    assert_eq!(answered(&cell, 0, Route::Heartbeat, 409), 1);
    assert_eq!(
        answered(&cell, 0, Route::Hello, 200),
        2,
        "seated again once"
    );
    assert_eq!(listed(&cell)[0], ("h000".to_owned(), HostState::Live));
}

/// Asserts that a lease and a run made through the client router reach the agent as one
/// start, that the run is launched once, ends on its own timer and its result is
/// acknowledged `Accepted`; that the ledger shows one launch and one completion; and that
/// after the lease is released every step invariant held and the end check finds nothing.
/// Catches a start lost between the real server and the agent, a result the server does
/// not acknowledge, and a booking left behind.
#[test]
fn a_lease_and_a_run_made_through_the_client_router_are_started_once_and_their_result_is_acknowledged()
 {
    let mut cell = cell("run", pool(RUN_HOSTS), RUN_HOSTS, Faults::default());
    // Scripted past the horizon, so the loop meets an action later than its end and
    // leaves it unrun: the last assertion shows the agent was never killed.
    cell.at(ms(10 * RUN_HORIZON_MS), |cell| cell.kill(0));
    run_to(&mut cell, 10_000);
    let lease = lease(&mut cell).expect("a lease is granted");
    let run = start_run(&mut cell, &lease, RUN_MS).expect("a run is queued");
    run_to(&mut cell, 30_000);
    let launches = cell.ledger().launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].run, RunId::new(run.clone()));
    let completions = cell.ledger().completions();
    assert_eq!(completions.len(), 1);
    assert!(completions[0].at.as_millis() - launches[0].at.as_millis() >= RUN_MS);
    let truth = cell.agent(0).and_then(AgentNode::truth).expect("a process");
    assert_eq!(
        truth.acknowledged.get(&RunId::new(run)),
        Some(&Verdict::Accepted)
    );
    let counters = cell.counters(0).expect("an agent");
    assert_eq!(counters.get(Counter::StartRepeated), 0);
    let (status, _) = call(
        &mut cell,
        "DELETE",
        &format!("/v1/leases/{lease}"),
        &json!({}),
    );
    assert!((200..300).contains(&status), "release answered {status}");
    run_to(&mut cell, RUN_HORIZON_MS);
    let settled = Checker::end(cell.observations(), Duration::from_secs(20));
    assert_eq!(settled, Ok(Vec::new()));
    assert!(
        cell.observations().agents.contains_key(&launches[0].host),
        "not killed yet"
    );
}

/// Asserts that a run shorter than the heartbeat interval, on a bus slower than the
/// interval, is started again by the real server (design section 8, row 31: the heartbeat
/// that leaves before the start arrives does not list the run), that a correct agent
/// counts the repeated start and launches the run once, and that an agent that runs a
/// repeated start fails `AtMostOneLaunch`. The first half is the witness that the second
/// can fail. Catches a correct agent that launches a repeated start, and a cell that does
/// not see a second launch.
#[test]
fn the_server_sends_a_start_again_to_a_host_that_already_ran_it_and_a_correct_agent_does_not_launch_it_twice()
 {
    let outcome = |defect: Option<Defect>| {
        let spec = pool(RESEND_HOSTS).heartbeat(RESEND_INTERVAL, RESEND_GRACE);
        let faults = Faults {
            min_delay: RESEND_DELAY,
            max_delay: RESEND_DELAY,
            ..Faults::default()
        };
        let mut cell = Cell::new(Setup::new(11), spec, scratch("resend"), faults).expect("starts");
        let added = match defect {
            Some(defect) => cell.add_broken_agent(defect),
            None => cell.add_agent(),
        };
        added.expect("an agent name is a host id");
        cell.run_until(ms(15_000)).expect("no run yet");
        let lease = lease(&mut cell).expect("a lease is granted");
        start_run(&mut cell, &lease, RESEND_RUN_MS).expect("a run is queued");
        let ended = cell.run_until(ms(RESEND_HORIZON_MS));
        (cell, ended)
    };
    let (correct, ended) = outcome(None);
    assert_eq!(ended, Ok(()));
    let counters = correct.counters(0).expect("an agent");
    assert!(counters.get(Counter::StartRepeated) >= 1, "{counters:?}");
    assert_eq!(correct.ledger().launches().len(), 1);
    assert_eq!(correct.ledger().completions().len(), 1);
    let (broken, ended) = outcome(Some(Defect::StartRunsTwice));
    let violation = ended.expect_err("a second launch");
    assert_eq!(violation.invariant, Invariant::AtMostOneLaunch);
    assert_eq!(broken.ledger().launches().len(), 2);
}

/// Asserts that an agent cut off for longer than the grace reads lost in the registry and
/// in the cell's listing, and after the cut heals beats again (more heartbeats answered
/// 200) and reads live, without a new hello. Catches a node that does not resume its
/// heartbeats after a partition.
#[test]
fn a_silent_host_reads_lost_and_a_cut_agent_that_returns_is_heard_again() {
    let mut cell = cell("lost", pool(LOST_HOSTS), LOST_HOSTS, Faults::default());
    let (cut, heal) = LOST_CUT_MS;
    run_to(&mut cell, cut);
    cell.partition(&[&[0]]);
    assert!(cell.observations().fault_active);
    run_to(&mut cell, heal - 1);
    assert_eq!(listed(&cell)[0], ("h000".to_owned(), HostState::Lost));
    let host = js_types::HostId::new("h000").expect("a host id");
    assert!(cell.observations().server.hosts[&host].lost);
    let beats = answered(&cell, 0, Route::Heartbeat, 200);
    cell.heal();
    assert!(!cell.observations().fault_active);
    assert_eq!(cell.observations().quiet_since, ms(heal - 1));
    run_to(&mut cell, LOST_HORIZON_MS);
    assert_eq!(listed(&cell)[0], ("h000".to_owned(), HostState::Live));
    assert!(!cell.observations().server.hosts[&host].lost);
    assert!(answered(&cell, 0, Route::Heartbeat, 200) > beats);
    assert_eq!(answered(&cell, 0, Route::Hello, 200), 1);
}

/// Asserts that a killed agent process leaves the cell's observations, holds the cell
/// faulted and lets its host read lost, and that its replacement says hello again, is
/// listed live and ends the fault; that killing or replacing an agent that does not exist
/// changes nothing. Catches a kill that leaves a dead process's truth in the
/// observations, and a replacement that never registers.
#[test]
fn a_killed_agent_leaves_the_observations_and_its_replacement_registers_again() {
    let mut cell = cell("kill", pool(KILL_HOSTS), KILL_HOSTS, Faults::default());
    run_to(&mut cell, KILL_AT_MS);
    let host = js_types::HostId::new("h000").expect("a host id");
    assert!(cell.observations().agents.contains_key(&host));
    cell.kill(0);
    cell.kill(99);
    assert!(!cell.observations().agents.contains_key(&host));
    assert!(cell.observations().fault_active);
    run_to(&mut cell, REPLACE_AT_MS);
    assert_eq!(listed(&cell)[0], ("h000".to_owned(), HostState::Lost));
    assert!(cell.agent(0).and_then(AgentNode::truth).is_none());
    cell.replace(0);
    cell.replace(99);
    assert!(!cell.observations().fault_active);
    assert_eq!(cell.observations().quiet_since, ms(REPLACE_AT_MS));
    run_to(&mut cell, KILL_HORIZON_MS);
    assert_eq!(listed(&cell)[0], ("h000".to_owned(), HostState::Live));
    assert_eq!(answered(&cell, 0, Route::Hello, 200), 2);
    assert!(cell.observations().agents.contains_key(&host));
    assert!(cell.agent(99).is_none() && cell.counters(99).is_none());
    assert_eq!(cell.sim().now(), ms(KILL_HORIZON_MS));
}

/// Asserts the server node on its own: its first input arms a tick after the server's
/// tick period, a tick runs and arms the next, an answer sent to the server is ignored, a
/// call is answered to its caller with its number, and the cell's node kinds name what
/// they are; and that a cell keeps no trace lines unless asked. Catches a server node
/// that ticks at another period, answers what is not a call, or loses a call's number.
#[test]
fn the_server_node_ticks_on_its_period_and_answers_only_calls() {
    let spec = pool(1);
    let server = ServerNode::start(&spec, scratch("node")).expect("starts");
    let mut node = CellNode::Server(Box::new(server));
    let input = |at: u64, event| NodeInput {
        now: FarmTime::from_millis(at),
        entropy: 0,
        event,
    };
    let tick = |outputs: &[js_sim::Output<Msg>]| matches!(outputs, [js_sim::Output::Timer { after, tag: 0 }] if *after == INTERVAL);
    node.apply(input(0, Event::Start));
    assert!(tick(&node.take_outputs()), "the first tick is armed");
    node.apply(input(5_000, Event::Timer { tag: 0 }));
    assert!(tick(&node.take_outputs()), "the next tick is armed");
    let answer = Msg::Reply {
        id: CallId(3),
        status: 200,
        retry_after: None,
        body: String::new(),
    };
    let from = agent_name(0);
    let ignored = Event::Message {
        from: from.clone(),
        msg: answer,
    };
    node.apply(input(5_001, ignored));
    assert!(node.take_outputs().is_empty());
    let hello = Msg::Request {
        id: CallId(4),
        route: Route::Hello,
        bearer: CLIENT_TOKEN.to_owned(),
        body: "{}".to_owned(),
    };
    node.apply(input(
        5_002,
        Event::Message {
            from: from.clone(),
            msg: hello,
        },
    ));
    let replies = node.take_outputs();
    assert_eq!(replies.len(), 1);
    let js_sim::Output::Send { to, msg } = &replies[0] else {
        panic!("a reply");
    };
    assert_eq!(to, &from);
    assert_eq!(CellNode::trace_msg(msg), "answer #4 401");
    assert!(node.as_agent().is_none() && node.as_agent_mut().is_none());
    assert!(node.as_server().is_some() && node.as_server_mut().is_some());
    let server = node.as_server().expect("the server");
    assert_eq!(server.answered(&from, Route::Hello, 401), 1);
    assert_eq!(trace_msg(msg), "answer #4 401");

    let agent = AgentSpec {
        host: js_types::HostId::new("h000").expect("a host id"),
        pool: "burst".to_owned(),
        join_token: CLIENT_TOKEN.to_owned(),
        capacity: Shape::gpu_host().resources().expect("a host"),
        capabilities: Shape::gpu_host().capabilities,
        labels: Default::default(),
        hello_interval: INTERVAL,
        kind: js_sim_cell::AgentKind::Fake(None),
        instance: None,
    };
    let mut agent = CellNode::Agent(Box::new(AgentNode::new(agent, Ledger::new())));
    assert!(agent.as_server().is_none() && agent.as_server_mut().is_none());
    assert!(agent.as_agent().is_some() && agent.as_agent_mut().is_some());
    agent.apply(input(0, Event::Start));
    assert_eq!(agent.take_outputs().len(), 2, "a hello and a tick");

    let mut plain = cell("plain", pool(1), 0, Faults::default());
    assert!(plain.trace_lines().is_none());
    assert_eq!(Setup::new(11).seed(), 11);
}

/// Asserts that the cell hands its server the store as the server documents `--store`: a
/// SQLite spec, the default, passes `--store` naming an owner-only file in the server's
/// directory that holds the one line `sqlite:<dir>/state.db`, with `--allow-sqlite-store`,
/// and the server's own parser and configuration take both; a started server makes the
/// file the line names; the cell reads the store only while the server is down (refused
/// while it is up, an empty store once it is stopped), and a restart opens it again. With
/// the feature `postgres`: a Postgres spec's flags are refused, naming `--store`, without
/// the node that makes its database, and a node's store file holds the URL of a database of
/// its own and no SQLite file is made. Catches a store file the server would refuse (another
/// spelling, a relative path, a file others can read), a SQLite store without its gate, a
/// server whose store is not where the file says, and a read of the store beside a running
/// server.
#[test]
fn the_store_choice_opens_the_store_the_server_flag_names() {
    let spec = pool(1);
    assert_eq!(spec.kept_in(), Store::Sqlite, "SQLite is the default");
    let files = scratch("store-flags");
    let args = spec.args(files.path()).expect("the files are written");
    let at = args.iter().position(|arg| arg == "--store");
    let file = PathBuf::from(&args[at.expect("--store is passed") + 1]);
    assert_eq!(file, files.path().join(STORE_LINE_FILE));
    let line = fs::read_to_string(&file).expect("the store file is written");
    let db = files.path().join(SQLITE_FILE);
    assert_eq!(line, format!("sqlite:{}\n", db.display()));
    let mode = fs::metadata(&file)
        .expect("the store file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "owner-only, as the server demands");
    assert!(args.contains(&OsString::from("--allow-sqlite-store")));
    let clock = ManualClock::new(Timestamp::from_millis(EPOCH_MS));
    let taken = spec.config(files.path(), &clock);
    assert!(taken.is_ok(), "{:?}", taken.err());

    let dir = scratch("store-node");
    let path = dir.path().to_path_buf();
    let mut server = ServerNode::start(&spec, dir).expect("starts");
    assert!(path.join(SQLITE_FILE).is_file(), "the store the file names");
    let up = server.stored("hosts/");
    assert_eq!(up, Err("the server is up, and holds its store".to_owned()));
    server.stop();
    server.stop();
    assert!(!server.is_up());
    assert_eq!(server.stored("hosts/"), Ok(Vec::new()));
    server
        .restart(&spec)
        .expect("starts again over the same store");
    assert!(server.is_up());
    server.stop();
    fs::write(path.join(SQLITE_FILE), "not a state store").expect("the file is written");
    let unread = server.stored("hosts/");
    assert!(unread.is_err(), "not a store the cell can open: {unread:?}");

    #[cfg(feature = "postgres")]
    {
        let postgres = pool(1).store(Store::Postgres);
        assert_eq!(postgres.kept_in(), Store::Postgres);
        let refused = postgres.args(scratch("store-pg-flags").path());
        assert_eq!(
            refused.expect_err("the node makes the database").flag(),
            "--store"
        );
        let dir = scratch("store-pg-node");
        let path = dir.path().to_path_buf();
        let server = ServerNode::start(&postgres, dir).expect("starts over its database");
        let name = server.database().expect("a database of its own");
        let line = fs::read_to_string(path.join(STORE_LINE_FILE)).expect("the store file");
        assert_eq!(line, format!("{}\n", with_database(&server_url(), name)));
        assert!(!path.join(SQLITE_FILE).exists());
    }
}

/// Asserts the store side of a cell: a host joins with a token the cell minted through the
/// operator route (a token it did not mint adds no host) and is seated; a stop of the server
/// reads the store while it is down, the token used by that host and the host bound to it;
/// a second server is started over the store while the first is down and refused, naming
/// `--store` and the writer lock, once the first runs again; a stop over a store that
/// cannot be opened is kept as a read that failed and traced `store unread`; and a read
/// pairs each used
/// token with its binding, naming a token used by a host with no binding to it and a host
/// bound to a token it did not use, and passing a host bound to the pool's shared token.
/// Catches a mint whose token the agent never gets, a read that misses a row or reads a
/// field the server does not write, and a judge that passes a token without its binding or
/// a binding without its token.
#[test]
fn a_store_read_pairs_each_used_token_with_its_binding() {
    let dir = scratch("store-side");
    let path = dir.path().to_path_buf();
    let setup = Setup::new(11).with_trace_lines();
    let mut cell = Cell::new(setup, pool(1), dir, Faults::default()).expect("starts");
    assert!(cell.join(0).is_none(), "no token minted yet");
    cell.mint_join_token();
    assert_eq!(cell.minted(), 1);
    let host = cell.join(0).expect("a host of its own token");
    assert_eq!(host.as_str(), "h000");
    run_to(&mut cell, 2_000);
    assert_eq!(listed(&cell), [("h000".to_owned(), HostState::Live)]);
    cell.stop_server();
    let read = cell.store_reads()[0].clone().expect("the store is read");
    assert_eq!(read.at, ms(2_000));
    assert_eq!(
        read.joins.values().collect::<Vec<_>>(),
        [&Some("h000".to_owned())]
    );
    let bound = read.bindings.get("h000").expect("h000 is bound");
    assert!(read.joins.contains_key(bound), "to the token it used");
    assert!(read.unpaired().is_empty(), "{:?}", read.unpaired());
    cell.start_second_server();
    cell.restart_server();
    cell.start_second_server();
    let seconds = cell.second_servers();
    let refused = "--store: another process holds the store's writer lock".to_owned();
    let second = |at, refused| SecondServer {
        at: ms(at),
        refused,
    };
    assert_eq!(seconds, [second(2_000, None), second(2_000, Some(refused))]);
    cell.stop_server();
    fs::write(path.join(SQLITE_FILE), "not a state store").expect("the file is written");
    cell.stop_server();
    let reads = cell.store_reads();
    assert_eq!(reads.len(), 3);
    assert!(reads[1].is_ok() && reads[2].is_err(), "{reads:?}");
    let lines = cell.trace_lines().expect("lines are kept");
    let unread = lines
        .iter()
        .filter(|line| line.ends_with(" cell store unread"));
    assert_eq!(unread.count(), 1);

    let used = |host: &str| format!("{{\"version\":1,\"used_by\":\"{host}\"}}").into_bytes();
    let bound = |digest: &str| format!("{{\"version\":1,\"credential\":\"{digest}\"}}");
    let row = |key: &str, value: Vec<u8>| (key.to_owned(), value);
    let joins = vec![
        row("hosts/join/aa", used("h000")),
        row("hosts/join/bb", b"{\"version\":1}".to_vec()),
        row("hosts/join/cc", used("h002")),
    ];
    let bindings = vec![
        row("hosts/binding/h000", bound("aa").into_bytes()),
        row("hosts/binding/h001", bound("bb").into_bytes()),
        row("hosts/binding/h003", bound("pool").into_bytes()),
        row("hosts/binding/h004", b"not json".to_vec()),
    ];
    let read = StoreRead::from_rows(ms(5), joins, bindings);
    assert_eq!(read.joins.get("bb"), Some(&None), "unused");
    assert_eq!(read.bindings.get("h004"), Some(&String::new()));
    assert_eq!(
        read.unpaired(),
        [
            "a token used by h002, which has no binding to it",
            "h001 is bound to a token it did not use",
        ]
    );
}

/// Asserts that a run's Postgres database is there while its cell runs and dropped when the
/// cell is: a store opens on it during the run and is refused once the cell is gone. Catches
/// a database left behind by every run, and one dropped while its server still runs on it.
#[cfg(feature = "postgres")]
#[test]
fn the_postgres_database_of_a_scenario_is_dropped_when_the_run_ends() {
    use js_state::postgres::{PostgresConfig, PostgresStore};

    let spec = pool(1).store(Store::Postgres);
    let mut cell = cell("store-pg-drop", spec, 1, Faults::default());
    cell.settle(Duration::from_secs(20)).expect("settles");
    let name = cell
        .server()
        .and_then(ServerNode::database)
        .map(str::to_owned);
    let url = with_database(&server_url(), &name.expect("a database of its own"));
    let config = PostgresConfig::parse(&url).expect("the URL parses");
    assert!(
        PostgresStore::open(&config).is_ok(),
        "there while the run runs"
    );
    drop(cell);
    assert!(
        PostgresStore::open(&config).is_err(),
        "dropped with the run"
    );
}
