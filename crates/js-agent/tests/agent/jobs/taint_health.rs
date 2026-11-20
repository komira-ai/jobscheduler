//! What the session says of the host's taint and what it keeps of the server's: the `tainted`
//! flag of every heartbeat of a session with a worker, and the health state the host's health
//! route answers from (`js_agent::health`).

use std::time::Duration;

use js_agent::health::{Health, Verdict};
use js_agent::{Delivery, Drain, QUEUE, Session, Step};
use serde_json::json;

use super::fake::FakeMachine;
use super::{start, start_answer, starts};
use crate::support::{Scratch, Script, beat_ok, hello, lowest, refused, reply, welcome};

/// The drain the sessions of these tests carry; none of them drains.
const DRAIN: Drain = Drain {
    wait: Duration::from_secs(10),
    settle: Duration::from_secs(30),
};

/// What the health state says of a server taint named `host-job` for the job `j1`.
const SERVER_TAINT: &str = "not placeable: the lease server holds a taint on this host \
                            (reason host-job, run j1)";

/// What the health state says of a taint the agent holds.
const OWN_TAINT: &str = "not placeable: the agent holds a taint on this host";

/// What the health state says when the session holds no welcome.
const NO_SESSION: &str = "not placeable: no session with the lease server";

/// A reply that carries `host_taint` and nothing else.
fn server_holds(reason: &str, run: &str) -> crate::support::Reply {
    reply(
        200,
        json!({ "host_taint": { "reason": reason, "run_id": run } }),
    )
}

/// A reply of 200 acknowledging the taint report.
fn taint_ack() -> crate::support::Reply {
    reply(200, json!({ "commands": [], "taint_ack": true }))
}

/// A worker that holds a taint: a run that exited and whose container cannot be removed.
fn tainted_worker(dir: &Scratch, machine: &FakeMachine) -> js_agent::Worker {
    machine.world.fail("remove", u32::MAX);
    let worker = start(dir, machine);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    machine.world.exit("r1", 0);
    assert!(worker.handle().settle());
    assert!(worker.snapshot().taint.is_some());
    worker
}

fn verdict(health: &Health) -> String {
    health.verdict().to_string()
}

/// Asserts what every heartbeat of a session with a worker says of the host: `tainted` is
/// false for a worker that holds no taint, true for one that does, and still true in the
/// heartbeat after the server acknowledged the taint report (the report stops, the taint does
/// not); a session with no worker says nothing. It catches a heartbeat that says nothing, which
/// leaves the server unable to tell a clean agent from an old one, and one that says the host
/// is clean once its report was acknowledged.
#[test]
fn every_heartbeat_says_whether_the_host_is_tainted() {
    let dir = Scratch::new("jobs-taint-flag-clean");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let (mut script, sent) = Script::new(vec![welcome(1), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    session.tick();
    session.tick();
    assert_eq!(sent.borrow()[1].1["tainted"], json!(false));

    let dir = Scratch::new("jobs-taint-flag-tainted");
    let machine = FakeMachine::new();
    let worker = tainted_worker(&dir, &machine);
    let (mut script, sent) = Script::new(vec![welcome(1), taint_ack(), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    session.tick();
    session.tick();
    assert!(worker.handle().settle());
    session.tick();
    let sent = sent.borrow();
    assert_eq!(sent[1].1["tainted"], json!(true));
    assert_eq!(sent[1].1["taint"]["reason"], "cleanup-failed");
    assert_eq!(sent[2].1["tainted"], json!(true));
    assert!(sent[2].1.get("taint").is_none(), "{}", sent[2].1);

    let (mut script, sent) = Script::new(vec![welcome(1), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut script, lowest());
    session.tick();
    session.tick();
    assert!(sent.borrow()[1].1.get("tainted").is_none());
}

/// Asserts that the health state follows the session: not placeable before the welcome,
/// placeable after it, not placeable with the reason, check and run of the taint a reply says
/// the server holds, placeable when a later reply says none, and not placeable again while
/// the session is lost; and that each change of the verdict is one line the session hands to
/// its caller with the worker's. It catches a health route that is never told of the session,
/// one that keeps a taint the server forgot, and a verdict that changes without a line.
#[test]
fn the_health_verdict_follows_the_welcome_the_servers_taint_and_the_loss_of_the_session() {
    let dir = Scratch::new("jobs-taint-health-session");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let (mut script, _) = Script::new(vec![
        welcome(1),
        server_holds("host-job", "j1"),
        beat_ok(),
        refused(409, "stale-epoch"),
        welcome(2),
    ]);
    let health = Health::default();
    let mut session = Session::new(hello("h1"), &mut script, lowest())
        .with_jobs(&worker, DRAIN)
        .with_health(&health);
    assert_eq!(verdict(&health), NO_SESSION);
    assert_eq!(worker.take_lines(), ["the fake engine is ready"]);

    session.tick();
    assert_eq!(health.verdict(), Verdict::Placeable);
    assert_eq!(
        session.job_lines(),
        [
            format!("health: {NO_SESSION}"),
            "health: placeable".to_owned()
        ]
    );

    session.tick();
    assert_eq!(verdict(&health), SERVER_TAINT);
    assert_eq!(session.job_lines(), [format!("health: {SERVER_TAINT}")]);

    session.tick();
    assert_eq!(health.verdict(), Verdict::Placeable);
    assert_eq!(session.job_lines(), ["health: placeable"]);

    session.tick();
    assert_eq!(verdict(&health), NO_SESSION);
    assert_eq!(session.job_lines(), [format!("health: {NO_SESSION}")]);

    session.tick();
    assert_eq!(health.verdict(), Verdict::Placeable);
    assert_eq!(session.job_lines(), ["health: placeable"]);
    assert!(session.job_lines().is_empty(), "each line is taken once");
}

/// Asserts that the taint the agent's worker holds reaches the health state with the
/// heartbeat that carries it, and stays while the host is tainted after the server
/// acknowledged the report. It catches a tainted agent that reports itself placeable.
#[test]
fn the_taint_of_the_worker_makes_the_host_not_placeable() {
    let dir = Scratch::new("jobs-taint-health-own");
    let machine = FakeMachine::new();
    let worker = tainted_worker(&dir, &machine);
    let (mut script, _) = Script::new(vec![welcome(1), taint_ack(), beat_ok()]);
    let health = Health::default();
    let mut session = Session::new(hello("h1"), &mut script, lowest())
        .with_jobs(&worker, DRAIN)
        .with_health(&health);
    session.tick();
    assert_eq!(health.verdict(), Verdict::Placeable);
    session.tick();
    assert_eq!(verdict(&health), OWN_TAINT);
    assert!(worker.handle().settle());
    session.tick();
    assert_eq!(verdict(&health), OWN_TAINT);
}

/// Asserts that a reply whose `host_taint` fails the wire's check (a reason with a control
/// character) is not shown, and that a session with no worker shows the server's taint too
/// and sends no `tainted`. It catches a taint taken on trust into a line and an HTTP body, and
/// a health route that works only beside a worker.
#[test]
fn a_session_with_no_worker_shows_the_servers_taint_and_ignores_an_unusable_one() {
    let (mut script, sent) = Script::new(vec![
        welcome(1),
        server_holds("residue", "r1"),
        server_holds("bad\nreason", "r1"),
    ]);
    let health = Health::default();
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_health(&health);
    session.tick();
    session.tick();
    assert_eq!(
        verdict(&health),
        "not placeable: the lease server holds a taint on this host (reason residue, run r1)"
    );
    session.tick();
    assert_eq!(health.verdict(), Verdict::Placeable);
    assert!(sent.borrow()[1].1.get("tainted").is_none());
}

/// Asserts that a reply that carries only the taint the server holds is not handed to the
/// worker: with the worker's queue full it is not noted `Busy` and the step waits the
/// interval, as a quiet reply does, while the taint reaches the health state. It catches a
/// `host_taint` that makes every reply of a tainted host a delivery the worker must make room
/// for.
#[test]
fn a_reply_that_carries_only_the_servers_taint_is_not_delivered_to_the_worker() {
    let dir = Scratch::new("jobs-taint-health-delivery");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    machine.world.hold_creates();
    let mut replies = vec![welcome(1)];
    replies.extend((0..=QUEUE).map(|_| reply(200, start_answer(&["r1"]))));
    replies.push(server_holds("host-job", "j1"));
    let (mut script, _) = Script::new(replies);
    let health = Health::default();
    let mut session = Session::new(hello("h1"), &mut script, lowest())
        .with_jobs(&worker, DRAIN)
        .with_health(&health);
    let step = |session: &mut Session<'_>| match session.tick() {
        Step::Wait { after, note } => (after, note),
        other => panic!("{other:?}"),
    };
    step(&mut session);
    step(&mut session);
    machine.world.wait_entered(1);
    for n in 0..QUEUE {
        assert_eq!(step(&mut session).1, None, "reply {n} has room");
    }
    assert_eq!(verdict(&health), "placeable");
    assert_eq!(step(&mut session), (Duration::from_secs(5), None));
    assert_eq!(verdict(&health), SERVER_TAINT);
    machine.world.release();
    assert!(worker.handle().settle());
}
