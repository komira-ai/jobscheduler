//! The parts of the whole-host machinery, each shown to work and to fail when it should:
//! the lease front's whole-host path (its acquire under a request id, its wait and its
//! refusals, driven through a scripted client port, and its grant after the agent's
//! ready report on the real server), the actions that set how agents answer a prepare
//! and that request whole hosts, the whole-host expectations, and what the server spec
//! adds for them (the whole-node class, the placement and its architecture).
//!
//! Sizes: one or two hosts beating every 5 s for at most 1,900 s of virtual time, under
//! 800 agent requests a seed; the scripted front runs no server at all.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, Response};
use clap::ValueEnum;
use js_agent_wire::RefusalReason;
use js_lease_server::Placement;
use js_sim::Faults;
use js_sim_cell::actions::{
    agent_prepare_delay, agent_refuses_prepare, agent_silent_on_prepare, agents, request_whole_host,
};
use js_sim_cell::server::{ARCH, ARCH_KEY, virtual_time};
use js_sim_cell::{
    Cell, ClientEvent, EPOCH_MS, Expect, Front, HowEnded, ItemId, LeaseFront, Prepare,
    REQUEST_PREFIX, Scenario, ScenarioFailure, ServerSpec, Setup, Shape, Work,
};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;
use js_types::{HostId, LeaseId};
use serde_json::{Value, json};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> FarmTime {
    FarmTime::from_millis(n)
}

/// A scratch directory of its own, removed when dropped.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell"),
        name,
    )
}

/// `placement` as the server's command line spells it.
fn spelled(placement: Placement) -> String {
    let value = placement
        .to_possible_value()
        .expect("every placement has a name");
    value.get_name().to_owned()
}

/// A whole-node pool of `hosts` hosts.
fn whole_pool(hosts: usize) -> ServerSpec {
    ServerSpec::whole_node_pool("burst", Shape::gpu_host(), hosts)
}

/// A client port that answers from a script and writes each call and its body down.
struct Scripted {
    answers: VecDeque<(u16, Value)>,
    calls: Vec<String>,
    bodies: Vec<Value>,
}

impl js_sim_cell::ClientPort for Scripted {
    fn call(&mut self, request: Request<Body>) -> Response<String> {
        self.calls
            .push(format!("{} {}", request.method(), request.uri()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");
        let bytes = runtime
            .block_on(axum::body::to_bytes(request.into_body(), usize::MAX))
            .expect("a body");
        self.bodies
            .push(serde_json::from_slice(&bytes).unwrap_or(Value::Null));
        let (status, body) = self.answers.pop_front().expect("an answer for every call");
        Response::builder()
            .status(status)
            .body(body.to_string())
            .expect("a response")
    }
}

/// What one poll of a front did: the events heard, the calls made and their bodies.
type Polled = (Vec<ClientEvent>, Vec<String>, Vec<Value>);

/// Polls `front` at `at` ms with `answers` as the server's, which must all be taken.
fn poll(front: &mut LeaseFront, at: u64, answers: Vec<(u16, Value)>) -> Polled {
    let mut port = Scripted {
        answers: answers.into(),
        calls: Vec::new(),
        bodies: Vec::new(),
    };
    let heard = front.poll(ms(at), &mut port);
    assert!(port.answers.is_empty(), "unused: {:?}", port.answers);
    let events = heard.into_iter().map(|(_, event)| event).collect();
    (events, port.calls, port.bodies)
}

/// A lease front of a whole-node pool holding whole-host item `item`, submitted at 0.
fn whole_front(item: u32) -> LeaseFront {
    let mut front = LeaseFront::new(&whole_pool(1));
    front.submit(
        ItemId(item),
        &Work::whole_host(secs(10)),
        FarmTime::default(),
    );
    front
}

fn pending(id: &str) -> (u16, Value) {
    (202, json!({ "id": id, "state": "pending" }))
}

fn listed(leases: Value) -> (u16, Value) {
    (200, json!({ "leases": leases }))
}

fn refused(code: &str) -> (u16, Value) {
    (409, json!({ "error": code }))
}

/// Asserts the whole-host path of the lease front. Through a scripted port: the acquire
/// asks for the class `whole-node` under the request id `sim-item-<n>`, is answered 202
/// pending, the lease list is read while the lease is pending (no event), and once it is
/// held its run is queued and `Queued` then `Placed` (the grant) are heard. On the real
/// server, with an agent that takes 20 s to prepare: the item's lease is granted only
/// after the agent's ready report for exactly that lease, and the report comes at least
/// the delay after the start. Catches a whole-host item acquired as an ordinary lease or
/// without a request id, a grant heard while the lease is pending, and a grant before
/// the ready report.
#[test]
fn a_whole_host_item_is_acquired_with_a_request_id_and_held_only_after_ready() {
    let mut front = whole_front(3);
    let (heard, calls, bodies) = poll(&mut front, 0, vec![pending("1-1")]);
    assert!(heard.is_empty());
    assert_eq!(calls, ["POST /v1/leases"]);
    assert_eq!(bodies[0]["class"], "whole-node");
    assert_eq!(bodies[0]["request_id"], format!("{REQUEST_PREFIX}3"));
    let still = listed(json!([{ "id": "1-1", "state": "pending" }]));
    let (heard, calls, _) = poll(&mut front, 5_000, vec![still]);
    assert!(heard.is_empty());
    assert_eq!(calls, ["GET /v1/leases"]);
    let held = listed(json!([{ "id": "1-1", "state": "held", "host": "h000" }]));
    let queued = (202, json!({ "run_id": "run-1", "state": "queued" }));
    let (heard, _, _) = poll(
        &mut front,
        10_000,
        vec![held, queued, (200, json!({ "state": "queued" }))],
    );
    let granted = [
        ClientEvent::Queued {
            lease: LeaseId::new(1, 1),
            run: js_sim_cell::RunId::new("run-1"),
        },
        ClientEvent::Placed {
            host: HostId::new("h000").expect("a host id"),
        },
    ];
    assert_eq!(heard, granted);

    let dir = scratch("whole-front-real");
    let mut cell = Cell::new(Setup::new(3), whole_pool(1), dir, Faults::default()).expect("starts");
    cell.add_agent().expect("an agent name is a host id");
    let delay = secs(20);
    cell.set_prepare(0, Prepare::Ready { delay });
    let item = cell.submit(&Work::whole_host(secs(10)));
    cell.run_until(ms(120_000)).expect("no invariant fails");
    let heard = &cell.observations().items[&item].heard;
    let lease = heard.iter().find_map(|heard| match heard.event {
        ClientEvent::Queued { lease, .. } => Some(lease),
        _ => None,
    });
    let lease = lease.expect("the lease was granted");
    let placed = heard
        .iter()
        .find(|h| matches!(h.event, ClientEvent::Placed { .. }));
    let placed_at = placed.expect("the grant was heard").at;
    let ready = cell.ledger().ready();
    let first = ready.first().expect("the agent reported ready");
    assert_eq!(first.lease, lease.to_string());
    assert!(first.at >= ms(20_000), "ready at {:?}", first.at);
    assert!(
        first.at <= placed_at,
        "granted at {placed_at:?} before {:?}",
        first.at
    );
}

/// Asserts the whole-host acquire's refusals: a 409 `no-capacity` (the wait list is
/// full), `lease-ended` or `no-capable-host` (no host has said hello yet) is acquired
/// again at the next poll under the same request id, with no event; any other 409
/// (`exclusive-not-ready`, `no-fit`) ends the item `Failed` with nothing to release; and
/// a pending lease the list no longer shows (its prepare failed) is acquired again at
/// once, under the same request id. Catches a whole-host item that gives up on a wait
/// the server asked for, one that waits forever on a refusal that is final, and a retry
/// under another request id (a second request).
#[test]
fn a_whole_host_acquire_waits_on_a_refusal_to_wait_and_fails_on_any_other() {
    for code in ["no-capacity", "lease-ended", "no-capable-host"] {
        let mut front = whole_front(0);
        let (heard, _, first) = poll(&mut front, 0, vec![refused(code)]);
        assert!(heard.is_empty(), "{code}");
        assert_eq!(front.next_poll(), Some(ms(5_000)), "{code}");
        let (heard, _, again) = poll(&mut front, 5_000, vec![pending("1-1")]);
        assert!(heard.is_empty(), "{code}");
        assert_eq!(again[0]["request_id"], first[0]["request_id"], "{code}");
    }
    for code in ["exclusive-not-ready", "no-fit"] {
        let mut front = whole_front(0);
        let (heard, calls, _) = poll(&mut front, 0, vec![refused(code)]);
        let failed = ClientEvent::Terminal {
            how: HowEnded::Failed,
        };
        assert_eq!(heard, [failed], "{code}");
        assert_eq!(calls, ["POST /v1/leases"], "{code}");
        assert_eq!(front.next_poll(), None, "{code}");
    }
    let mut front = whole_front(7);
    poll(&mut front, 0, vec![pending("1-1")]);
    let gone = listed(json!([]));
    let (heard, calls, bodies) = poll(&mut front, 5_000, vec![gone, pending("1-2")]);
    assert!(heard.is_empty());
    assert_eq!(calls, ["GET /v1/leases", "POST /v1/leases"]);
    assert_eq!(bodies[1]["request_id"], format!("{REQUEST_PREFIX}7"));
}

/// Asserts the prepare actions on the real server: both agents of a two-host pool are
/// made silent, so the one whole-host request's first host never reports ready and the
/// server's prepare deadline (600 s) fails it; at 700 s the agents (and an index past the
/// last, which names no agent) are made to report ready at once, so the request's next
/// try is held, runs and is released, and both hosts are serving at the end. Catches a
/// silence switch that still reports, a delay switch that never reports, a switch kept
/// in the process (lost at a change of switch), and a cell that fails on an agent index
/// it does not have.
#[test]
fn silent_agents_fail_a_prepare_and_agents_made_ready_take_the_next_try() {
    Scenario::new("silent_agents_fail_a_prepare_and_agents_made_ready_take_the_next_try")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(whole_pool(2))
        .hosts("burst", 2)
        .at(Duration::ZERO, agent_silent_on_prepare(agents(0..2)))
        .work(1, Work::whole_host(secs(10)))
        .at(secs(700), agent_prepare_delay(agents(0..3), Duration::ZERO))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Succeeded))
        .expect(Expect::held_and_released(1))
        .expect(Expect::every_host_serving())
        .settle_within(secs(1_200))
        .run_seeds(1);
}

/// Asserts `agent_refuses_prepare` and `Expect::every_host_serving` failing: the one
/// host's agent refuses every prepare as `host-unsafe`, so the server taints the host,
/// the request can never be held and is given up, and the expectation that every host
/// is serving at the end fails naming what it wanted and what it got. Catches a refusal
/// the server never hears (the request would be held), and an expectation that does
/// not read the hosts' state.
#[test]
fn a_host_that_refuses_as_unsafe_is_not_serving_at_the_end() {
    let failure = Scenario::new("a_host_that_refuses_as_unsafe_is_not_serving_at_the_end")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(whole_pool(1))
        .hosts("burst", 1)
        .at(
            Duration::ZERO,
            agent_refuses_prepare(agents(0..1), RefusalReason::HostUnsafe),
        )
        .work(1, Work::whole_host(secs(10)).give_up_after(secs(120)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::GaveUp))
        .expect(Expect::every_host_serving())
        .settle_within(secs(600))
        .try_run_seeds(1);
    match failure {
        Err(ScenarioFailure::Unmet { detail, .. }) => {
            assert_eq!(detail, "every-host-serving: wanted 1, got 0");
        }
        other => panic!("not an unmet expectation: {other:?}"),
    }
}

/// Asserts `request_whole_host` and `Expect::held_and_released` failing: two whole-host
/// requests on one host are each held in turn, run and released (every item succeeds),
/// and an expectation of three held and released fails naming what it wanted and what it
/// got. Catches an action that asks for anything but a whole host (the second request
/// would not wait for the first), and an expectation that counts items that were never
/// held.
#[test]
fn requested_whole_hosts_are_held_in_turn_and_a_wrong_count_fails() {
    let failure = Scenario::new("requested_whole_hosts_are_held_in_turn_and_a_wrong_count_fails")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(whole_pool(1))
        .hosts("burst", 1)
        .at(Duration::ZERO, request_whole_host(2, secs(600)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(2, HowEnded::Succeeded))
        .expect(Expect::held_and_released(3))
        .settle_within(secs(600))
        .try_run_seeds(1);
    match failure {
        Err(ScenarioFailure::Unmet { detail, .. }) => {
            assert_eq!(detail, "held-and-released: wanted 3, got 2");
        }
        other => panic!("not an unmet expectation: {other:?}"),
    }
}

/// Asserts what the server spec adds: a whole-node pool's file names the class
/// `whole-node`, every placement is passed as `--placement` with its own spelling, the
/// default is `registry`, and a placement gives a shape with no architecture `arch =
/// x86_64` while keeping one it has; and the virtual time of a wall time, from the cell's
/// epoch and floored at zero. Catches a placement that never reaches the server, a pool
/// `js-sched` refuses, an architecture written over, and a tail end off by the epoch.
#[test]
fn the_server_spec_passes_the_placement_the_class_and_an_arch() {
    let flag = |spec: &ServerSpec, name: &str| {
        let dir = scratch(name);
        let args = spec.args(dir.path()).expect("the files are written");
        let at = args.iter().position(|arg| arg == "--placement");
        args[at.expect("the flag is passed") + 1].clone()
    };
    for placement in Placement::value_variants() {
        let spec = whole_pool(1).placement(*placement);
        let name = format!("spec-{}", spelled(*placement));
        assert_eq!(flag(&spec, &name), OsString::from(spelled(*placement)));
        assert!(spec.pool_file().contains("class = \"whole-node\""));
        let arch = spec.shape().capabilities.get(ARCH_KEY);
        assert_eq!(arch.map(String::as_str), Some(ARCH));
    }
    let plain = ServerSpec::registered_pool("burst", Shape::gpu_host(), 1);
    assert_eq!(flag(&plain, "spec-default"), OsString::from("registry"));
    assert!(plain.pool_file().contains("class = \"gpu-host\""));
    assert_eq!(plain.shape().capabilities.get(ARCH_KEY), None);
    let arm = Shape {
        capabilities: BTreeMap::from([(ARCH_KEY.to_owned(), "arm64".to_owned())]),
        ..Shape::gpu_host()
    };
    let kept = ServerSpec::registered_pool("burst", arm, 1).placement(Placement::default());
    let arch = kept.shape().capabilities.get(ARCH_KEY);
    assert_eq!(arch.map(String::as_str), Some("arm64"));
    assert_eq!(virtual_time(EPOCH_MS + 5_000), ms(5_000));
    assert_eq!(virtual_time(0), ms(0));
}

/// Asserts what the server's view (`ServerNode::view`) takes from the registry's
/// reservations, on the real server: two whole-host requests on one host, so the second
/// waits while the first holds the host and is set aside the host once the first is
/// released. Every tenth of a second the view lists each reserved host with the request
/// it is set aside for and the end of its tail on the cell's virtual clock, exactly as
/// `Registry::reservations` says at that moment, and the second request is seen set
/// aside at least once. Catches a view that drops the request or the tail, or converts
/// the tail from the wall clock off by the cell's epoch.
#[test]
fn the_server_view_lists_each_reservation_with_its_request_and_tail() {
    let dir = scratch("whole-view");
    let mut cell = Cell::new(Setup::new(5), whole_pool(1), dir, Faults::default()).expect("starts");
    cell.add_agent().expect("an agent name is a host id");
    cell.submit(&Work::whole_host(secs(10)));
    let second = cell.submit(&Work::whole_host(secs(10)));
    let mut seen = Vec::new();
    for tenth in 1..=1_800 {
        cell.run_until(ms(tenth * 100)).expect("no invariant fails");
        let server = cell.server().expect("the cell has a server");
        let view = server.view();
        let registry = server
            .registry()
            .expect("the cell's server registers hosts");
        for reservation in registry.reservations() {
            let listed = &view.hosts[&reservation.host];
            let request = reservation.request.to_string();
            assert_eq!(listed.reserved_for.as_deref(), Some(request.as_str()));
            assert_eq!(listed.tail_end, reservation.tail_end.map(virtual_time));
            seen.push(request);
        }
    }
    let waited = format!("{REQUEST_PREFIX}{}", second.0);
    assert!(seen.contains(&waited), "set aside: {seen:?}");
}

/// Asserts that `Cell::cancel_item` records, in the observations, the item and the
/// virtual time the client first asked to cancel it: a second ask later keeps the first
/// time. Catches a cancel the cell never records (every `drain-kills-nothing` check would
/// then read the client as never having asked) and one recorded at its latest ask.
#[test]
fn a_client_cancel_is_recorded_at_the_time_it_was_first_asked() {
    let dir = scratch("cancel-record");
    let mut cell = Cell::new(Setup::new(2), whole_pool(1), dir, Faults::default()).expect("starts");
    cell.add_agent().expect("an agent name is a host id");
    let item = cell.submit(&Work::whole_host(secs(600)));
    assert!(cell.observations().cancels.is_empty());
    cell.run_until(ms(3_000)).expect("no invariant fails");
    cell.cancel_item(item);
    cell.run_until(ms(7_000)).expect("no invariant fails");
    cell.cancel_item(item);
    let cancels: Vec<(ItemId, FarmTime)> = cell
        .observations()
        .cancels
        .iter()
        .map(|(item, at)| (*item, *at))
        .collect();
    assert_eq!(cancels, [(item, ms(3_000))]);
}
