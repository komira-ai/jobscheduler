//! The session carrying a worker: what the hello and the heartbeat say of its jobs, what
//! the session does with a reply, and what it does when the worker is busy or gone.

use std::time::Duration;

use js_agent::{
    Answer, Drain, HEARTBEAT_PATH, HELLO_PATH, Note, QUEUE, Session, Step, Transport, Worker,
};
use serde_json::{Value, json};

use super::fake::FakeMachine;
use super::{start, start_answer, starts};
use crate::support::{Scratch, Script, beat_ok, hello, lowest, refused, reply, welcome};

/// The drain the sessions of these tests carry; none of them drains.
const DRAIN: Drain = Drain {
    wait: Duration::from_secs(10),
    settle: Duration::from_secs(30),
};

/// A reply of 200 with these acknowledgements and taint flag.
fn answer(acks: &[(&str, &str)], taint_ack: bool) -> crate::support::Reply {
    let acks: Vec<Value> = acks
        .iter()
        .map(|(run, ack)| json!({ "run_id": run, "ack": ack }))
        .collect();
    reply(
        200,
        json!({ "commands": [], "result_acks": acks, "taint_ack": taint_ack }),
    )
}

/// A worker with `run` running, or exited with its result held.
fn with_run(dir: &Scratch, machine: &FakeMachine, run: &str, exited: bool) -> Worker {
    let worker = start(dir, machine);
    assert!(worker.try_deliver(starts(&[run])));
    assert!(worker.handle().settle());
    if exited {
        machine.world.exit(run, 0);
        assert!(worker.handle().settle());
    }
    worker
}

/// Asserts what the session says of the jobs: the hello lists the run the worker holds, the
/// next heartbeat lists it as exited and carries its result, an acknowledgement in the
/// reply makes the worker drop the result, and the heartbeat after that lists nothing. It
/// catches a heartbeat that leaves out the held runs (the server would run the work
/// twice), one that never carries the result, and an acknowledgement that goes nowhere.
#[test]
fn hello_and_heartbeat_carry_the_workers_runs_and_results() {
    let dir = Scratch::new("jobs-session-carry");
    let machine = FakeMachine::new();
    let worker = with_run(&dir, &machine, "r1", true);
    let replies = vec![welcome(1), answer(&[("r1", "ACCEPTED")], false), beat_ok()];
    let (mut script, sent) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);

    session.tick();
    session.tick();
    assert!(worker.handle().settle());
    session.tick();

    let sent = sent.borrow();
    let paths: Vec<&str> = sent.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(paths, [HELLO_PATH, HEARTBEAT_PATH, HEARTBEAT_PATH]);
    assert_eq!(
        sent[0].1["held"],
        json!([{ "run_id": "r1", "state": "exited_unacked" }])
    );
    assert_eq!(sent[1].1["held"], sent[0].1["held"]);
    assert_eq!(sent[1].1["results"][0]["run_id"], "r1");
    assert_eq!(sent[1].1["results"][0]["outcome"], "succeeded");
    assert_eq!(sent[2].1["held"], json!([]));
    assert_eq!(sent[2].1["results"], json!([]));
}

/// Asserts that a new epoch keeps the worker and what it holds: after the server forgets
/// the session (a stale epoch) the second hello lists the run again, and the run was
/// created once. It catches a session that rebuilds the job half when it registers again,
/// which would start the work twice or forget it.
#[test]
fn a_new_epoch_keeps_the_worker_and_lists_its_runs_in_the_hello() {
    let dir = Scratch::new("jobs-session-epoch");
    let machine = FakeMachine::new();
    let worker = with_run(&dir, &machine, "r1", false);
    let replies = vec![
        welcome(1),
        refused(409, "stale-epoch"),
        welcome(2),
        beat_ok(),
    ];
    let (mut script, sent) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    for _ in 0..4 {
        session.tick();
    }
    let sent = sent.borrow();
    let paths: Vec<&str> = sent.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(
        paths,
        [HELLO_PATH, HEARTBEAT_PATH, HELLO_PATH, HEARTBEAT_PATH]
    );
    let running = json!([{ "run_id": "r1", "state": "running" }]);
    assert_eq!(sent[0].1["held"], running);
    assert_eq!(sent[2].1["held"], running);
    assert_eq!(machine.world.created(), ["r1"]);
}

/// Asserts that the commands of an accepted reply reach the worker in the order the reply
/// lists them, and that what the worker did is printable by the session thread. It catches
/// a reply that is not handed over, and one handed over out of order.
#[test]
fn the_commands_of_a_reply_reach_the_worker_in_order() {
    let dir = Scratch::new("jobs-session-commands");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let replies = vec![welcome(1), reply(200, start_answer(&["r2", "r1"]))];
    let (mut script, _) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    session.tick();
    let Step::Wait { note, .. } = session.tick() else {
        panic!("the heartbeat is answered");
    };
    assert_eq!(note, None);
    assert!(worker.handle().settle());

    assert_eq!(machine.world.created(), ["r2", "r1"]);
    let lines = session.job_lines();
    assert!(
        lines
            .iter()
            .any(|line| line == "job: Started { run_id: \"r1\" }")
    );
    assert!(session.job_lines().is_empty(), "the lines are taken once");
}

/// Asserts the taint's path through the session: the heartbeat after a host was tainted
/// (a container that cannot be removed) carries the reason and the run; the reply's
/// `taint_ack` goes to the worker, which stops reporting; the next heartbeat carries no
/// taint. It catches a taint that is never reported and one reported for ever.
#[test]
fn a_taint_is_reported_until_the_server_acknowledges_it() {
    let dir = Scratch::new("jobs-session-taint");
    let machine = FakeMachine::new();
    machine.world.fail("remove", u32::MAX);
    let worker = with_run(&dir, &machine, "r1", true);
    assert!(worker.snapshot().taint.is_some());
    let replies = vec![welcome(1), answer(&[], true), beat_ok()];
    let (mut script, sent) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    session.tick();
    session.tick();
    assert!(worker.handle().settle());
    session.tick();

    let sent = sent.borrow();
    assert_eq!(sent[1].1["taint"]["reason"], "cleanup-failed");
    assert_eq!(sent[1].1["taint"]["run_id"], "r1");
    assert!(sent[2].1.get("taint").is_none(), "{}", sent[2].1);
    assert!(worker.snapshot().taint.is_none());
}

/// A transport that runs `during` once, while the first heartbeat is on its way to the
/// server and after its body was made.
struct Hooked<'a> {
    inner: Script,
    during: Option<Box<dyn FnOnce() + 'a>>,
}

impl Transport for Hooked<'_> {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        let answered = self.inner.post(path, body);
        if path == HEARTBEAT_PATH {
            self.during.take().into_iter().for_each(|during| during());
        }
        answered
    }
}

/// Asserts that a `taint_ack` answers only a heartbeat that carried the taint: the host is
/// tainted while the first heartbeat (which carried none) is in flight, the reply says
/// `taint_ack`, and the worker still reports the taint in the next heartbeat. It catches a
/// session that passes any acknowledgement on, so that a taint the server was never told of
/// is marked as told and the host goes on taking work.
#[test]
fn an_acknowledgement_of_a_heartbeat_with_no_taint_counts_for_nothing() {
    let dir = Scratch::new("jobs-session-ack-carried");
    let machine = FakeMachine::new();
    let worker = with_run(&dir, &machine, "r1", false);
    let (script, sent) = Script::new(vec![welcome(1), answer(&[], true), beat_ok()]);
    let handle = worker.handle();
    let world = machine.world.clone();
    let taint_now = move || {
        world.fail("remove", u32::MAX);
        world.exit("r1", 0);
        assert!(handle.settle());
    };
    let mut transport = Hooked {
        inner: script,
        during: Some(Box::new(taint_now)),
    };
    let mut session = Session::new(hello("h1"), &mut transport, lowest()).with_jobs(&worker, DRAIN);
    session.tick();
    session.tick();
    assert!(worker.handle().settle());
    assert!(worker.snapshot().taint.is_some(), "the ack was not for it");
    session.tick();

    let sent = sent.borrow();
    assert!(sent[1].1.get("taint").is_none());
    assert_eq!(sent[2].1["taint"]["reason"], "cleanup-failed");
}

/// Asserts that a reply the busy worker has no room for is dropped and noted (`Busy`) and
/// that the session does not wait: with the worker inside a call that does not return, the
/// replies fill the queue and the next one is noted, in the same tick that took it. It
/// catches a hand-over that blocks the heartbeat on the engine.
#[test]
fn a_reply_the_busy_worker_cannot_take_is_noted_and_the_tick_returns() {
    let dir = Scratch::new("jobs-session-busy");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    machine.world.hold_creates();
    let mut replies = vec![welcome(1)];
    replies.extend((0..QUEUE + 2).map(|_| reply(200, start_answer(&["r1"]))));
    let (mut script, _) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    let note = |session: &mut Session<'_>| match session.tick() {
        Step::Wait { note, .. } => note,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        note(&mut session),
        Some(Note::Registered {
            epoch: 1,
            interval: Duration::from_secs(5),
        })
    );
    assert_eq!(note(&mut session), None);
    machine.world.wait_entered(1);
    for n in 0..QUEUE {
        assert_eq!(note(&mut session), None, "reply {n} has room");
    }
    assert_eq!(note(&mut session), Some(Note::Busy));
    machine.world.release();
    assert!(worker.handle().settle());
}

/// Asserts that a session whose worker has ended ends with `Failed`, and that a worker that
/// is alive does not. It catches an agent that heartbeats on with jobs nobody supervises.
#[test]
fn a_worker_that_ended_ends_the_session() {
    let dir = Scratch::new("jobs-session-ended");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let (mut script, _) = Script::new(vec![welcome(1), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    assert!(matches!(session.tick(), Step::Wait { .. }));

    machine.world.panic_at("create");
    assert!(worker.try_deliver(starts(&["r1"])));
    assert!(!worker.handle().settle());
    let Step::Failed(why) = session.tick() else {
        panic!("the worker is gone");
    };
    assert!(why.contains("job worker stopped"), "{why}");
}
