//! The parts of the whole-host machinery, each shown to work and to fail when it should:
//! the lease front's whole-host path (its acquire under a request id, under a given one or
//! under none, its wait and its refusals, driven through a scripted client port, the
//! answers it keeps, and its grant after the agent's ready report on the real server), the
//! actions that set how agents answer a prepare and that request whole hosts, the
//! whole-host expectations (those on a raw answer and on the server as it stands when they
//! are judged, at the end or at a virtual time), and what the server spec adds for them
//! (the whole-node class and keys, the placement and its architecture).
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
use js_registry::Exclusion;
use js_sim::Faults;
use js_sim_cell::actions::{
    agent_prepare_delay, agent_refuses_prepare, agent_silent_on_prepare, agents,
    request_whole_host, submit_work,
};
use js_sim_cell::server::{ARCH, ARCH_KEY, virtual_time};
use js_sim_cell::{
    Booked, Call, CallKind, Cell, ClientEvent, EPOCH_MS, Expect, Front, HostRef, HowEnded, ItemId,
    LeaseFront, Listed, Prepare, REQUEST_PREFIX, RecordFact, Scenario, ScenarioFailure, ServerSpec,
    Setup, Shape, Work,
};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;
use js_types::{HostId, LeaseId, RequestId};
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

/// Asserts the lease front's other two ways of asking for a whole host, through a scripted
/// port: an item of `without_request_id` asks for `whole-node` with no `request_id`, and
/// waits on a 409 `no-capacity` (acquired again at the next poll) but fails on
/// `exclusive-not-ready`; one of `with_request_id` asks under the id it was given; an
/// ordinary item asks for the pool's class with no id whatever its work says. Every answer
/// is kept, as it came, with its item, its kind and its time. Catches an id-less request
/// sent as an ordinary lease or under an id (the server would queue it instead of
/// refusing it), a given id replaced by the item's own, and an answer kept under the
/// wrong item or call.
#[test]
fn a_whole_host_item_is_asked_for_under_no_request_id_or_a_given_one() {
    let mut front = LeaseFront::new(&whole_pool(1));
    let start = FarmTime::default();
    let unnamed = Work::whole_host(secs(10)).without_request_id();
    front.submit(ItemId(0), &unnamed, start);
    let named = Work::whole_host(secs(10)).with_request_id("nightly-42");
    front.submit(ItemId(1), &named, start);
    let ordinary = Work::run_for(secs(10)).with_request_id("not-sent");
    front.submit(ItemId(2), &ordinary, start);
    let answers = vec![
        refused("no-capacity"),
        pending("1-1"),
        refused("no-capacity"),
    ];
    let (heard, _, bodies) = poll(&mut front, 0, answers);
    assert!(heard.is_empty());
    assert_eq!(bodies[0]["class"], "whole-node");
    assert_eq!(bodies[0].get("request_id"), None);
    assert_eq!(bodies[1]["class"], "whole-node");
    assert_eq!(bodies[1]["request_id"], "nightly-42");
    assert_eq!(bodies[2]["class"], "gpu-host");
    assert_eq!(bodies[2].get("request_id"), None);
    let still = listed(json!([{ "id": "1-1", "state": "pending" }]));
    let answers = vec![
        refused("exclusive-not-ready"),
        still,
        refused("no-capacity"),
    ];
    let (heard, calls, _) = poll(&mut front, 5_000, answers);
    let failed = ClientEvent::Terminal {
        how: HowEnded::Failed,
    };
    assert_eq!(heard, [failed]);
    assert_eq!(
        calls,
        ["POST /v1/leases", "GET /v1/leases", "POST /v1/leases"]
    );
    let kept: Vec<(u32, CallKind, FarmTime, u16)> = front
        .answers()
        .iter()
        .map(|answer| (answer.item.0, answer.kind, answer.at, answer.status))
        .collect();
    let acquire = CallKind::Acquire;
    let wanted = [
        (0, acquire, ms(0), 409),
        (1, acquire, ms(0), 202),
        (2, acquire, ms(0), 409),
        (0, acquire, ms(5_000), 409),
        (1, CallKind::List, ms(5_000), 200),
        (2, acquire, ms(5_000), 409),
    ];
    assert_eq!(kept, wanted);
    assert_eq!(front.answers()[0].body, "{\"error\":\"no-capacity\"}");
    let kinds = [
        acquire,
        CallKind::List,
        CallKind::Queue,
        CallKind::Read,
        CallKind::Cancel,
        CallKind::Release,
    ];
    let names: Vec<&str> = kinds.into_iter().map(CallKind::name).collect();
    assert_eq!(
        names,
        ["acquire", "list", "queue", "read", "cancel", "release"]
    );
}

/// A cell of one host of a whole-node pool of `spec`, whose agent answers a prepare as
/// `prepare` says, with one item of `work` submitted at the server's first tick and the
/// cell run to `until` ms.
fn one_item(name: &str, spec: ServerSpec, prepare: Prepare, work: &Work, until: u64) -> Cell {
    let mut cell =
        Cell::new(Setup::new(4), spec, scratch(name), Faults::default()).expect("starts");
    cell.add_agent().expect("an agent name is a host id");
    cell.set_prepare(0, prepare);
    cell.run_until(ms(5_000)).expect("no invariant fails");
    cell.submit(work);
    cell.run_until(ms(until)).expect("no invariant fails");
    cell
}

/// What `expect` finds unmet in `cell` now, or the empty text when it is met.
fn unmet(expect: &Expect, cell: &mut Cell) -> String {
    expect.unmet(cell).unwrap_or_default()
}

/// Asserts `Expect::answer` on the real server: the whole-host acquire made at 5 s is
/// kept as answered (item 0, an acquire, at 5 s, 202 `pending`), and the expectation is
/// met by its status alone and by its status and its exact body, and fails, naming the
/// call, what it wanted and what it got, on another status, on another body, and on a
/// call not made (`no-answer`). Catches an expectation that compares the status when a
/// body is wanted, one that compares the text of a body and not its JSON, and one that
/// reads a call that was not made as met.
#[test]
fn an_answer_expectation_compares_the_status_and_the_body_the_client_kept() {
    let work = Work::whole_host(secs(10));
    let mut cell = one_item(
        "answer-expect",
        whole_pool(1),
        Prepare::default(),
        &work,
        5_500,
    );
    let first = cell.answers()[0].clone();
    let kept = (first.item, first.kind, first.at, first.status);
    assert_eq!(kept, (ItemId(0), CallKind::Acquire, ms(5_000), 202));
    let body: Value = serde_json::from_str(&first.body).expect("a JSON body");
    assert_eq!(body["state"], "pending");
    let ask = Call::acquire(ItemId(0), 1);
    assert_eq!(unmet(&Expect::answer(ask, 202, None), &mut cell), "");
    assert_eq!(
        unmet(&Expect::answer(ask, 202, Some(body.clone())), &mut cell),
        ""
    );
    let status = Expect::answer(ask, 201, None);
    let wanted = "answer(item0 acquire 1): wanted 201, got 202";
    assert_eq!(unmet(&status, &mut cell), wanted);
    let other = Expect::answer(ask, 202, Some(json!({ "error": "no-capacity" })));
    let wanted = format!(
        "answer(item0 acquire 1): wanted 202 {{\"error\":\"no-capacity\"}}, got 202 {body}"
    );
    assert_eq!(unmet(&other, &mut cell), wanted);
    let second = Expect::answer(Call::acquire(ItemId(0), 2), 202, None);
    let wanted = "answer(item0 acquire 2): wanted 202, got no-answer";
    assert_eq!(unmet(&second, &mut cell), wanted);
    let release = Expect::answer(Call::release(ItemId(0), 1), 200, None);
    let wanted = "answer(item0 release 1): wanted 200, got no-answer";
    assert_eq!(unmet(&release, &mut cell), wanted);
    let made = Call::new(ItemId(0), CallKind::Acquire, 1);
    assert_eq!(made, ask);
}

/// Asserts the expectations that read the server when they are judged, on the real
/// server: one host whose agent never reports ready, a 60 s `prepare_timeout`, and one
/// whole-host item under the request id `probe-1` asked at 5 s. A second later its lease
/// is listed pending, booked on the host, not waiting, with no reservation and no record;
/// at 66 s, after its deadline, the record holds one failure, not terminal, the host left
/// out until the default 300 s backoff after the deadline, and `prepare-timeout`; and
/// with the server down the list is `unread` and the lease booked nowhere. Each one that
/// does not hold names what it wanted and what it got: another state, a call not made
/// (`no-lease`), the leases listed, another host or no host, a place on the wait list, a
/// host not listed, a record absent or present, other exclusions, another code. Catches a
/// statement read from a stale observation instead of the server, one met by a missing
/// lease or record, and one that reports what it got wrongly.
#[test]
fn the_server_expectations_read_the_server_when_they_are_judged() {
    let spec = whole_pool(1).prepare_timeout(secs(60));
    let work = Work::whole_host(Duration::ZERO).with_request_id("probe-1");
    let mut cell = one_item("server-expect", spec, Prepare::Silent, &work, 6_000);
    let ask = Call::acquire(ItemId(0), 1);
    let never = Call::acquire(ItemId(1), 1);
    let body: Value = serde_json::from_str(&cell.answers()[0].body).expect("a JSON body");
    let lease = body["id"].as_str().expect("a lease id").to_owned();
    let request = RequestId::new("probe-1").expect("a request id");
    let met = [
        Expect::listed(ask, Listed::Pending),
        Expect::booked(ask, Booked::On(HostRef::Agent(0))),
        Expect::booked(ask, Booked::Somewhere),
        Expect::booked(ask, Booked::On(HostRef::BookedFor(ask))),
        Expect::booked(never, Booked::Nowhere),
        Expect::waiting("probe-1", None),
        Expect::no_reservations(),
        Expect::request_record(&request, RecordFact::Absent),
    ];
    for expect in &met {
        assert_eq!(unmet(expect, &mut cell), "", "{expect:?}");
    }
    let failed = [
        (
            Expect::listed(ask, Listed::Held),
            "listed(item0 acquire 1): wanted held, got pending".to_owned(),
        ),
        (
            Expect::listed(never, Listed::Absent),
            "listed(item1 acquire 1): wanted absent, got no-lease".to_owned(),
        ),
        (
            Expect::no_leases_listed(),
            format!("no-leases-listed: wanted [], got [{lease}]"),
        ),
        (
            Expect::booked(ask, Booked::Nowhere),
            "booked(item0 acquire 1): wanted nowhere, got h000".to_owned(),
        ),
        (
            Expect::booked(ask, Booked::Elsewhere(HostRef::Agent(0))),
            "booked(item0 acquire 1): wanted not on h000, got h000".to_owned(),
        ),
        (
            Expect::booked(ask, Booked::On(HostRef::BookedFor(never))),
            "booked(item0 acquire 1): wanted on nowhere, got h000".to_owned(),
        ),
        (
            Expect::waiting("probe-1", Some(0)),
            "waiting(probe-1): wanted 0, got none".to_owned(),
        ),
        (
            Expect::set_aside(5, None),
            "set-aside(h005): wanted aside=false for=none, got unlisted".to_owned(),
        ),
        (
            Expect::request_record(&request, RecordFact::PrepareFailures(1)),
            "request-record(probe-1).prepare-failures: wanted 1, got absent".to_owned(),
        ),
    ];
    for (expect, wanted) in &failed {
        assert_eq!(&unmet(expect, &mut cell), wanted);
    }

    cell.run_until(ms(66_000)).expect("no invariant fails");
    let record = |fact: RecordFact| Expect::request_record(&request, fact);
    for fact in [RecordFact::PrepareFailures(1), RecordFact::Terminal(false)] {
        assert_eq!(unmet(&record(fact.clone()), &mut cell), "", "{fact:?}");
    }
    let until = EPOCH_MS + 65_000 + 300_000;
    let failed = [
        (
            record(RecordFact::Absent),
            "request-record(probe-1).absent: wanted absent, got present".to_owned(),
        ),
        (
            record(RecordFact::Exclusions(vec![(
                HostRef::Agent(0),
                Exclusion::ForLife,
            )])),
            format!(
                "request-record(probe-1).exclusions: wanted [h000 for life], got [h000 until {until}]"
            ),
        ),
        (
            record(RecordFact::LastFailure("setup-failed".to_owned())),
            "request-record(probe-1).last-failure: wanted setup-failed, got prepare-timeout"
                .to_owned(),
        ),
    ];
    for (expect, wanted) in &failed {
        assert_eq!(&unmet(expect, &mut cell), wanted);
    }

    let refusing = ServerSpec::registered_pool("burst", Shape::gpu_host(), 0).hello_burst(1);
    cell.restart_server_with(&refusing);
    let down = [
        (
            Expect::listed(ask, Listed::Pending),
            "listed(item0 acquire 1): wanted pending, got unread",
        ),
        (
            Expect::no_leases_listed(),
            "no-leases-listed: wanted [], got unread",
        ),
        (
            Expect::booked(ask, Booked::Somewhere),
            "booked(item0 acquire 1): wanted somewhere, got nowhere",
        ),
    ];
    for (expect, wanted) in &down {
        assert_eq!(unmet(expect, &mut cell), *wanted);
    }
}

/// Asserts `Scenario::expect_at` and what a seed reports: a statement judged at 5 s, the
/// virtual time the item is submitted, is judged before the client's call of that time
/// and fails the seed as `at 5000 ms` and the statement; the same statement judged at
/// 6 s is met; and with every timed statement met, a statement judged at the end that is
/// not fails the seed. Catches a timed statement judged at the end, or after the client's
/// calls of its time, and one whose failure is dropped.
#[test]
fn an_expectation_at_a_virtual_time_is_judged_then_and_fails_its_seed() {
    let ask = Call::acquire(ItemId(0), 1);
    let at = |name: &str, when: Duration, last: Expect| {
        Scenario::new(name)
            .scratch(env!("CARGO_TARGET_TMPDIR"))
            .server(whole_pool(1))
            .hosts("burst", 1)
            .at(secs(5), submit_work(1, Work::whole_host(secs(1))))
            .expect_at(when, Expect::answer(ask, 202, None))
            .expect(last)
            .settle_within(secs(120))
            .try_run_seeds(1)
    };
    let met = Expect::all_items_terminal();
    let early = at("expect_at_before_the_call", secs(5), met.clone());
    let wanted = "at 5000 ms answer(item0 acquire 1): wanted 202, got no-answer";
    match early {
        Err(ScenarioFailure::Unmet { detail, seed, .. }) => {
            assert_eq!((detail.as_str(), seed), (wanted, 0));
        }
        other => panic!("not an unmet expectation: {other:?}"),
    }
    assert!(at("expect_at_after_the_call", secs(6), met).is_ok());
    let second = Expect::answer(Call::acquire(ItemId(0), 2), 202, None);
    match at("expect_at_then_at_the_end", secs(6), second) {
        Err(ScenarioFailure::Unmet { detail, .. }) => {
            assert_eq!(detail, "answer(item0 acquire 2): wanted 202, got no-answer");
        }
        other => panic!("not an unmet expectation: {other:?}"),
    }
}

/// Asserts the whole-node keys of the server spec: `prepare_timeout`, `reserve_backoff`
/// and `max_replace` are written into the pool file as the server spells them, ahead of
/// the tables, only when set, and a server of them starts. Catches a key misspelled or
/// written in a table (the server would refuse the file) and a key written when unset.
#[test]
fn the_server_spec_writes_the_whole_node_keys_it_is_given() {
    let spec = whole_pool(1)
        .prepare_timeout(secs(60))
        .reserve_backoff(Duration::from_millis(10_500))
        .max_replace(2);
    let file = spec.pool_file();
    let keys = "max_replace = 2\nprepare_timeout = \"60000ms\"\nreserve_backoff = \"10500ms\"\n";
    let tables = file.find("[shape]").expect("the shape table");
    assert!(file.find(keys).is_some_and(|at| at < tables), "{file}");
    assert!(!whole_pool(1).pool_file().contains("prepare_timeout"));
    let cell = Cell::new(
        Setup::new(1),
        spec,
        scratch("whole-keys"),
        Faults::default(),
    );
    assert!(cell.is_ok(), "the server takes the keys");
}

/// Asserts that `Expect::no_reservations` names the reservation it finds, on the real
/// server: two whole-host items on one host, so the second waits while the first holds
/// the host and is set aside the host once the first is released. At the first tenth of a
/// second the registry holds that reservation, the statement fails naming the host and
/// the request, `[h000 for sim-item-1]`. Catches a statement met while a host is set aside,
/// and one that reports what it got without the host or the request.
#[test]
fn no_reservations_names_the_reservation_it_finds() {
    let dir = scratch("no-reservations");
    let mut cell = Cell::new(Setup::new(5), whole_pool(1), dir, Faults::default()).expect("starts");
    cell.add_agent().expect("an agent name is a host id");
    cell.submit(&Work::whole_host(secs(10)));
    let second = cell.submit(&Work::whole_host(secs(10)));
    let expect = Expect::no_reservations();
    let wanted = format!(
        "no-reservations: wanted [], got [h000 for {REQUEST_PREFIX}{}]",
        second.0
    );
    let mut found = None;
    for tenth in 1..=1_800 {
        cell.run_until(ms(tenth * 100)).expect("no invariant fails");
        let held = cell
            .server()
            .and_then(|s| s.registry())
            .map(|r| r.reservations());
        if held.is_some_and(|held| !held.is_empty()) {
            found = expect.unmet(&mut cell);
            break;
        }
    }
    assert_eq!(found, Some(wanted));
}
