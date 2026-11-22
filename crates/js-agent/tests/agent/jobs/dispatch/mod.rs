//! The agent's dispatch poll: what a poll carries, what an answer does to the worker, what is
//! acknowledged and when, how a failure and a refusal end a poll, how the poller follows the
//! session's welcomes, and the whole agent polling through a connector of its own. Nothing
//! waits for a time: the fake transports answer when the test says so, and every wait on
//! another thread is for an event with a hang guard. The poller's thread, the session's cell
//! and the whole agent are in `threads`.

mod live;
mod threads;

use std::time::Duration;

use js_agent::dispatch::{Cell, Next, Poller, Route};
use js_agent::lifecycle::Quiet;
use js_agent::{CredentialState, Delivery, QUEUE, Quiesce, WorkerHost};
use js_agent_wire::limits::MAX_COMMANDS;
use js_agent_wire::{
    BookedResources, DISPATCH_PATH, DispatchReply, Dispatched, JobStartSpec, Welcome,
};
use serde_json::{Value, json};

use super::fake::FakeMachine;
use super::{start, starts};
use crate::support::{Lowest, Reply, SECRET, Scratch, Script, limited, refused, reply, welcome};

/// How long a test waits for another thread before it gives up: a guard against a hang, not
/// a time any test expects to pass.
pub(super) const GUARD: Duration = Duration::from_secs(20);

const IMAGE: &str = "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A valid job start for `job`, whose allocation is `alloc-<job>`.
pub(super) fn spec(job: &str) -> JobStartSpec {
    JobStartSpec {
        job_id: job.to_owned(),
        allocation_id: format!("alloc-{job}"),
        image: IMAGE.to_owned(),
        command: vec!["--go".to_owned()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
        isolation: Default::default(),
        fuse: false,
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
        placed_at_ms: None,
        xcode: None,
        sizing: None,
    }
}

/// The body of an answer that hands out `messages`.
fn handing(messages: Vec<Dispatched>) -> Value {
    serde_json::to_value(DispatchReply { messages }).unwrap()
}

/// A 200 that hands out `messages`.
pub(super) fn handed(messages: Vec<Dispatched>) -> Reply {
    reply(200, handing(messages))
}

/// A 200 that hands out nothing.
pub(super) fn quiet() -> Reply {
    handed(Vec::new())
}

/// The route of host `h1`'s session of `epoch`.
fn route(epoch: u64) -> Route {
    Route {
        host_id: "h1".to_owned(),
        epoch,
        session: SECRET.to_owned(),
        credential: None,
    }
}

/// A welcome of `epoch` and `session`, which says `dispatch_poll` when `polling`, and carries
/// `credential` when one is given.
pub(super) fn welcomed(
    epoch: u64,
    session: &str,
    polling: bool,
    credential: Option<&str>,
) -> Welcome {
    let mut body = json!({
        "epoch": epoch,
        "heartbeat_interval_ms": 5000,
        "grace_ms": 15000,
        "pool": "macs",
        "session": session,
        "dispatch_poll": polling,
    });
    if let Some(credential) = credential {
        body["host_credential"] = json!(credential);
    }
    serde_json::from_value(body).unwrap()
}

/// A welcome answer that says `dispatch_poll`.
pub(super) fn welcome_polling(epoch: u64) -> Reply {
    let mut answer = welcome(epoch).unwrap();
    answer.body["dispatch_poll"] = json!(true);
    Ok(answer)
}

/// The acknowledgements a poll carried, as `kind:allocation` texts in the order sent.
pub(super) fn acked(body: &Value) -> Vec<String> {
    let acks = body["acks"].as_array().cloned().unwrap_or_default();
    let text = |ack: &Value| format!("{}:{}", ack["kind"].as_str().unwrap(), ack["allocation_id"]);
    acks.iter().map(|ack| text(ack).replace('"', "")).collect()
}

/// The bodies of the polls a script was sent, in order.
fn polls(sent: &crate::support::Sent) -> Vec<Value> {
    sent.borrow()
        .iter()
        .inspect(|(path, _)| assert_eq!(path, DISPATCH_PATH))
        .map(|(_, body)| body.clone())
        .collect()
}

/// Asserts that a poll names the host, the epoch and the session of the route it is given
/// and carries no acknowledgement when it holds none; that a start in the answer reaches the
/// worker (the job is held), the poll is answered with `Again`, the next poll carries the
/// start's acknowledgement by kind and allocation id, and the poll after the one that carried
/// it carries none. Catches a poll that names another session, a start that is dropped on
/// the way to the worker, an acknowledgement that names the job instead of the allocation, and
/// one that is sent for ever.
#[test]
fn a_start_in_a_poll_reaches_the_worker_and_the_next_poll_acknowledges_it() {
    let dir = Scratch::new("jobs-dispatch-start");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let (mut script, sent) = Script::new(vec![
        handed(vec![Dispatched::start(spec("j1"))]),
        quiet(),
        quiet(),
    ]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    assert_eq!(poller.poll(&mut script, &route(7)), Next::Again);
    assert!(worker.handle().settle());
    assert_eq!(worker.snapshot().jobs[0].job_id, "j1");
    assert_eq!(poller.poll(&mut script, &route(7)), Next::Again);
    assert_eq!(poller.poll(&mut script, &route(7)), Next::Again);

    let bodies = polls(&sent);
    assert_eq!(
        bodies[0],
        json!({ "host_id": "h1", "epoch": 7, "session": SECRET })
    );
    assert_eq!(acked(&bodies[1]), ["start:alloc-j1"]);
    assert_eq!(acked(&bodies[2]), Vec::<String>::new());
    assert!(bodies[2].get("acks").is_none(), "{}", bodies[2]);
}

/// Asserts that a start handed out by a poll leaves the whole-node lease the host holds as
/// it is: the worker was granted a lease by a heartbeat's reply and found the host quiet for
/// it; after the poll delivers a start the job is held and the worker still answers ready for
/// that lease from what it published. Catches an answer delivered as a heartbeat reply that
/// names no lease, which ends the hold (the published answer for the lease is dropped, and the
/// host would prepare again and refuse the lease's own starts).
#[test]
fn a_poll_that_delivers_a_start_keeps_the_whole_node_lease_ready() {
    let dir = Scratch::new("jobs-dispatch-lease");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());
    let grant = js_agent_wire::HeartbeatReply {
        exclusive: Some("lease-1".to_owned()),
        ..Default::default()
    };
    assert_eq!(worker.try_deliver(grant), Delivery::Delivered);
    assert!(handle.settle());
    assert_eq!(host.prepare("lease-1"), Quiet::Busy, "asked");
    assert!(handle.settle());
    assert_eq!(host.prepare("lease-1"), Quiet::Ready);

    let (mut script, _) = Script::new(vec![handed(vec![Dispatched::start(spec("j1"))])]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert!(handle.settle());

    assert_eq!(worker.snapshot().jobs[0].job_id, "j1");
    assert_eq!(host.prepare("lease-1"), Quiet::Ready, "the hold was kept");
}

/// Asserts that a stop in a poll's answer reaches the worker, which stops the running job,
/// and that the next poll acknowledges it as a stop of the allocation (the kind is part of the
/// acknowledgement, so it never deletes the start of the same allocation). Catches a stop
/// that is acknowledged but never handed on, and an acknowledgement of a stop that says
/// `start`.
#[test]
fn a_stop_in_a_poll_stops_the_job_and_is_acknowledged_as_a_stop() {
    let dir = Scratch::new("jobs-dispatch-stop");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let (mut script, sent) = Script::new(vec![
        handed(vec![Dispatched::start(spec("j1"))]),
        handed(vec![Dispatched::stop(
            "j1".to_owned(),
            "alloc-j1".to_owned(),
        )]),
        quiet(),
    ]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert!(worker.handle().settle());
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert!(worker.handle().settle());
    assert!(machine.world.log().contains(&"stop j1".to_owned()));
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);

    let bodies = polls(&sent);
    assert_eq!(acked(&bodies[1]), ["start:alloc-j1"]);
    assert_eq!(acked(&bodies[2]), ["stop:alloc-j1"]);
}

/// Asserts that the same start handed out twice (the acknowledgement of the first was lost,
/// or the server that handed it out died) starts the job once, and that both deliveries are
/// acknowledged. Catches a second container for one job, and a redelivery that is not
/// acknowledged (the server would hand it out again for ever).
#[test]
fn a_start_handed_out_twice_starts_one_job_and_both_are_acknowledged() {
    let dir = Scratch::new("jobs-dispatch-twice");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let again = Dispatched::start(spec("j1"));
    let (mut script, sent) = Script::new(vec![
        handed(vec![again.clone()]),
        handed(vec![again]),
        quiet(),
        quiet(),
    ]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    for _ in 0..4 {
        assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
        assert!(worker.handle().settle());
    }

    assert_eq!(
        machine.world.created().len(),
        1,
        "{:?}",
        machine.world.log()
    );
    assert_eq!(worker.snapshot().jobs.len(), 1);
    let bodies = polls(&sent);
    assert_eq!(acked(&bodies[1]), ["start:alloc-j1"]);
    assert_eq!(acked(&bodies[2]), ["start:alloc-j1"], "the redelivery");
    assert_eq!(acked(&bodies[3]), Vec::<String>::new());
}

/// Asserts that a poll whose answer is lost (a transport error) waits a backoff (the lowest
/// jitter makes it 500 ms, then 1 s), that the next poll carries the acknowledgements the lost
/// one carried, and that the poll after an answered one carries none. Catches acknowledgements
/// dropped with a poll nobody answered (the server would hand the start out again after its
/// hold, once more than it must), and a backoff that never grows.
#[test]
fn a_poll_whose_answer_is_lost_sends_the_same_acknowledgements_again() {
    let dir = Scratch::new("jobs-dispatch-lost");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let (mut script, sent) = Script::new(vec![
        handed(vec![Dispatched::start(spec("j1"))]),
        Err("the connection was lost".to_owned()),
        Err("the connection was lost".to_owned()),
        quiet(),
        quiet(),
    ]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert_eq!(
        poller.poll(&mut script, &route(1)),
        Next::After(Duration::from_millis(500))
    );
    assert_eq!(
        poller.poll(&mut script, &route(1)),
        Next::After(Duration::from_secs(1))
    );
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);

    let bodies = polls(&sent);
    for (n, body) in bodies[1..=3].iter().enumerate() {
        assert_eq!(acked(body), ["start:alloc-j1"], "poll {}", n + 1);
    }
    assert_eq!(acked(&bodies[4]), Vec::<String>::new());
}

/// Asserts how each status ends a poll. Every status but 200 (404, 408, 429 with a
/// `Retry-After`, 5xx, and the refusals 400, 401, 403 and 409 of an instance that does not
/// hold the session or will not serve it) waits a backoff that doubles while the failures
/// last and starts again after an answered poll; the wait is at least the `Retry-After`; and
/// the acknowledgements are kept for the poll after each. Catches a refusal that stops the
/// polls for the rest of the session (one instance's 409 would leave the host without the
/// starts that the servers leave out of heartbeat replies), and a rate limit that is not
/// obeyed.
#[test]
fn every_refusal_backs_off_and_the_poll_goes_on() {
    let dir = Scratch::new("jobs-dispatch-statuses");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let mut replies = vec![handed(vec![Dispatched::start(spec("j1"))])];
    replies.extend([
        refused(404, "not-found"),
        refused(404, "not-found"),
        refused(503, "unavailable"),
    ]);
    for status in [400, 401, 403, 409] {
        replies.push(refused(status, "session-mismatch"));
    }
    replies.extend([quiet(), refused(408, "timeout"), limited(429, 30), quiet()]);
    let (mut script, sent) = Script::new(replies);
    let mut poller = Poller::new(&worker, Box::new(Lowest));
    let mut decided = || poller.poll(&mut script, &route(1));

    assert_eq!(decided(), Next::Again);
    let ms = Duration::from_millis;
    assert_eq!(decided(), Next::After(ms(500)));
    assert_eq!(decided(), Next::After(ms(1000)));
    assert_eq!(decided(), Next::After(ms(2000)));
    for wait in [4000, 8000, 16000, 30000] {
        assert_eq!(decided(), Next::After(ms(wait)));
    }
    assert_eq!(decided(), Next::Again);
    assert_eq!(
        decided(),
        Next::After(ms(500)),
        "an answered poll ended the run"
    );
    assert_eq!(decided(), Next::After(Duration::from_secs(30)));
    assert_eq!(decided(), Next::Again);

    let bodies = polls(&sent);
    for (n, body) in bodies[1..=8].iter().enumerate() {
        assert_eq!(acked(body), ["start:alloc-j1"], "poll {}", n + 1);
    }
    for (n, body) in bodies[9..=11].iter().enumerate() {
        assert_eq!(acked(body), Vec::<String>::new(), "poll {}", n + 9);
    }
}

/// Asserts that an answer that is not a usable reply (a body that does not decode, one that
/// lists more messages than a reply may) is treated as a failure: a backoff, nothing handed to
/// the worker, and the acknowledgements held kept. Catches a malformed answer taken for an
/// empty one (it would drop the acknowledgements the server still waits for), and a reply of
/// more messages than the wire allows being acted on.
#[test]
fn an_answer_that_is_not_a_usable_reply_is_a_failure_that_keeps_the_acknowledgements() {
    let dir = Scratch::new("jobs-dispatch-unusable");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let too_many = (0..=MAX_COMMANDS)
        .map(|n| Dispatched::stop(format!("j{n}"), format!("alloc-j{n}")))
        .collect();
    let (mut script, sent) = Script::new(vec![
        handed(vec![Dispatched::start(spec("j1"))]),
        reply(200, json!({ "messages": "none" })),
        handed(too_many),
        quiet(),
        quiet(),
    ]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert_eq!(
        poller.poll(&mut script, &route(1)),
        Next::After(Duration::from_millis(500))
    );
    assert_eq!(
        poller.poll(&mut script, &route(1)),
        Next::After(Duration::from_secs(1))
    );
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert!(worker.handle().settle());

    assert!(
        worker.snapshot().retired_jobs.is_empty(),
        "no stop was acted on"
    );
    let bodies = polls(&sent);
    for (n, body) in bodies[1..=3].iter().enumerate() {
        assert_eq!(acked(body), ["start:alloc-j1"], "poll {}", n + 1);
    }
    assert_eq!(acked(&bodies[4]), Vec::<String>::new());
}

/// Asserts that a message that fails its check is neither acted on nor acknowledged while the
/// valid messages beside it are: a start with no start, a start for another allocation than
/// the message names, a stop that carries a start, a kind this agent does not know, and a
/// start that does not decode; beside a good start and a stop. Catches one bad message that
/// costs the reply (the good start would wait for the server's hold), a bad start that is run,
/// and a bad message that is acknowledged (the server would delete what the agent never did).
#[test]
fn an_invalid_message_is_neither_acted_on_nor_acknowledged_but_its_neighbours_are() {
    let dir = Scratch::new("jobs-dispatch-invalid");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let mut bodiless = Dispatched::start(spec("j2"));
    bodiless.start = None;
    let mut elsewhere = Dispatched::start(spec("j3"));
    elsewhere.allocation_id = "alloc-other".to_owned();
    let mut stopping = Dispatched::stop("j4".to_owned(), "alloc-j4".to_owned());
    stopping.start = Some(Box::new(spec("j4")));
    let answer = json!({ "messages": [
        serde_json::to_value(&bodiless).unwrap(),
        serde_json::to_value(Dispatched::start(spec("j1"))).unwrap(),
        serde_json::to_value(&elsewhere).unwrap(),
        serde_json::to_value(&stopping).unwrap(),
        { "kind": "reboot", "job_id": "j5", "allocation_id": "alloc-j5" },
        { "kind": "start", "job_id": "j6", "allocation_id": "alloc-j6", "start": { "image": 5 } },
        serde_json::to_value(Dispatched::stop("j9".to_owned(), "alloc-j9".to_owned())).unwrap(),
    ] });
    let (mut script, sent) = Script::new(vec![reply(200, answer), quiet()]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert!(worker.handle().settle());

    let held: Vec<String> = worker
        .snapshot()
        .jobs
        .into_iter()
        .map(|j| j.job_id)
        .collect();
    assert_eq!(held, ["j1"]);
    assert_eq!(worker.snapshot().retired_jobs, ["j9"]);
    assert_eq!(machine.world.created().len(), 1);
    let bodies = polls(&sent);
    assert_eq!(acked(&bodies[1]), ["start:alloc-j1", "stop:alloc-j9"]);
}

/// Asserts that an answer whose messages are all invalid hands the worker nothing and
/// acknowledges nothing, and that a worker with no room for the answer (its queue is full
/// while it is held inside a call) gets nothing acknowledged either, so the server hands the
/// start out again after its hold; the next poll that finds room is acknowledged. Catches an
/// empty reply sent to the worker for every quiet poll, and a start acknowledged though the
/// worker dropped it.
#[test]
fn nothing_is_acknowledged_that_the_worker_did_not_take() {
    let dir = Scratch::new("jobs-dispatch-full");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    machine.world.hold_creates();
    assert_eq!(worker.try_deliver(starts(&["r0"])), Delivery::Delivered);
    machine.world.wait_entered(1);
    for _ in 0..QUEUE {
        assert_eq!(worker.try_deliver(starts(&["r0"])), Delivery::Delivered);
    }
    let mut bodiless = Dispatched::start(spec("j2"));
    bodiless.start = None;
    let (mut script, sent) = Script::new(vec![
        handed(vec![bodiless]),
        handed(vec![Dispatched::start(spec("j1"))]),
        quiet(),
        handed(vec![Dispatched::start(spec("j1"))]),
        quiet(),
    ]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again, "full");
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    machine.world.release();
    assert!(worker.handle().settle());
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);

    let bodies = polls(&sent);
    for (n, body) in bodies[1..=3].iter().enumerate() {
        assert_eq!(acked(body), Vec::<String>::new(), "poll {}", n + 1);
    }
    assert_eq!(acked(&bodies[4]), ["start:alloc-j1"]);
}

/// Asserts that a route prints without its secrets, and a poll does not carry a credential.
/// Catches a `Debug` of the route that leaks the session secret into a log line or a panic.
#[test]
fn a_route_prints_without_its_secrets() {
    let mut route = route(3);
    route.credential = Some(js_agent_wire::HostCredential::parse("cred-secret-1").unwrap());
    let text = format!("{route:?}");
    assert!(text.contains("h1") && text.contains("3"), "{text}");
    assert!(
        !text.contains(SECRET) && !text.contains("cred-secret-1"),
        "{text}"
    );
}

/// Asserts what the cell holds after welcomes: no route before one; the route of a welcome
/// that says `dispatch_poll`, with its generation; nothing for a later welcome that does not
/// (the poller stops); a later welcome's route replacing the earlier; and nothing once
/// closed. Catches a poller that keeps polling a session the server no longer serves, and one
/// that polls a welcome without the flag.
#[test]
fn the_cell_holds_the_route_of_the_latest_welcome_that_says_dispatch_poll() {
    let cell = Cell::new(None);
    assert!(cell.try_route().is_none());
    cell.publish(
        "h1",
        &welcomed(1, "one", false, None),
        CredentialState::None,
    );
    assert!(cell.try_route().is_none(), "no flag, no route");
    cell.publish("h1", &welcomed(2, "two", true, None), CredentialState::None);
    let (generation, route) = cell.try_route().unwrap();
    assert_eq!(
        (generation, route.epoch, route.session.as_str()),
        (2, 2, "two")
    );
    assert_eq!(route.host_id, "h1");
    cell.publish(
        "h1",
        &welcomed(3, "three", true, None),
        CredentialState::None,
    );
    assert_eq!(cell.try_route().unwrap().1.epoch, 3);
    cell.publish(
        "h1",
        &welcomed(4, "four", false, None),
        CredentialState::None,
    );
    assert!(cell.try_route().is_none(), "the flag was withdrawn");
    cell.publish(
        "h1",
        &welcomed(5, "five", true, None),
        CredentialState::None,
    );
    cell.close();
    assert!(cell.try_route().is_none());
    assert!(cell.wait_route().is_none());
}

/// Asserts which credential the route carries: the one found in the state root at start; the
/// one a welcome carried when the session saved it, or could not save it but uses it; and not
/// one the transport refused, nor one a session with no store ignored. A welcome with none
/// keeps the credential held. Catches a poll that authenticates with a dead token after the
/// host's credential replaced it, and one that adopts a credential the session did not.
#[test]
fn the_route_carries_the_credential_the_session_adopted() {
    let found = js_agent_wire::HostCredential::parse("found-credential").unwrap();
    let cell = Cell::new(Some(found));
    let held = |cell: &Cell| {
        let route = cell.try_route().unwrap().1;
        route.credential.map(|c| c.expose().to_owned())
    };
    let publish = |epoch: u64, credential: Option<&str>, kept: CredentialState| {
        cell.publish("h1", &welcomed(epoch, "s", true, credential), kept);
    };
    publish(1, None, CredentialState::None);
    assert_eq!(held(&cell).as_deref(), Some("found-credential"));
    publish(2, Some("saved-credential"), CredentialState::Saved);
    assert_eq!(held(&cell).as_deref(), Some("saved-credential"));
    publish(3, Some("ignored"), CredentialState::None);
    assert_eq!(held(&cell).as_deref(), Some("saved-credential"));
    publish(4, Some("refused"), CredentialState::Refused);
    assert_eq!(held(&cell).as_deref(), Some("saved-credential"));
    publish(5, Some("unsaved-credential"), CredentialState::Unsaved);
    assert_eq!(held(&cell).as_deref(), Some("unsaved-credential"));
    let bare = Cell::new(None);
    bare.publish("h1", &welcomed(1, "s", true, None), CredentialState::None);
    assert_eq!(held(&bare), None);
}

/// Asserts that a pause is cut short, without waiting, for a generation the cell has left and
/// for a cell that is closed, and that a pause of the current generation of an open cell is
/// not (it runs its whole limit, here zero). Catches a backoff that outlasts a new session or
/// the end of the agent, and one that is cut short with nothing to cut it.
#[test]
fn a_pause_ends_at_the_close_and_at_a_later_welcome() {
    let cell = Cell::new(None);
    cell.publish("h1", &welcomed(1, "s", true, None), CredentialState::None);
    cell.publish("h1", &welcomed(2, "s", true, None), CredentialState::None);
    assert!(
        cell.pause(Duration::from_secs(3600), 1),
        "a later welcome cuts the pause"
    );
    assert!(
        !cell.pause(Duration::ZERO, 2),
        "the current session's pause runs its limit"
    );
    cell.close();
    assert!(
        cell.pause(Duration::from_secs(3600), 2),
        "the close cuts the pause"
    );
}
