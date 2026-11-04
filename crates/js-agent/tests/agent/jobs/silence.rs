//! The silence bound through the session and the worker: the session stamps every welcome
//! and every accepted reply on the worker's clock (one the worker had no room for too), the
//! worker's lifecycle stops every job once the server has not been heard for the bound, and
//! a welcome whose grace the bound does not outlast ends the session. The clock is the
//! test's; every wait on the worker is for an order done.

use std::time::Duration;

use js_agent::{Delivery, Drain, Note, QUEUE, Session, Step, Worker};

use super::fake::FakeMachine;
use super::{STEP_NEVER, plan, start_answer};
use crate::lifecycle::silence::long_run;
use crate::support::{
    Scratch, Script, beat_ok, hello, lowest, refused, reply, welcome, welcome_with,
};

/// The silence bound of these tests.
const BOUND: Duration = Duration::from_secs(300);

/// The bound in milliseconds of the worker's clock.
const BOUND_MS: u64 = 300_000;

/// The drain the sessions carry; none of them drains.
const DRAIN: Drain = Drain {
    wait: Duration::from_secs(10),
    settle: Duration::from_secs(30),
};

/// A reply that starts these runs, each with a wall limit of a day, which no test reaches.
fn starts(runs: &[&str]) -> js_agent_wire::HeartbeatReply {
    let commands = runs
        .iter()
        .map(|run| js_agent_wire::Command::Start(Box::new(long_run(run))))
        .collect();
    js_agent_wire::HeartbeatReply {
        commands,
        ..js_agent_wire::HeartbeatReply::default()
    }
}

/// A worker that stops its jobs after `bound` of silence (stop grace 1 s), with `runs`
/// running.
fn silenced(dir: &Scratch, machine: &FakeMachine, bound: Duration, runs: &[&str]) -> Worker {
    let mut plan = plan(dir, machine, STEP_NEVER);
    plan.silence_stop = Some(bound);
    let worker = match Worker::start(plan) {
        Ok(worker) => worker,
        Err(why) => panic!("the worker did not start: {why}"),
    };
    if !runs.is_empty() {
        assert_eq!(worker.try_deliver(starts(runs)), Delivery::Delivered);
        assert!(worker.handle().settle());
    }
    worker
}

/// The runs the runtime was asked to stop, in order.
fn stopped(machine: &FakeMachine) -> Vec<String> {
    let log = machine.world.log().into_iter();
    log.filter(|line| line.starts_with("stop ")).collect()
}

/// The note of the next tick, which must be a wait.
fn tick(session: &mut Session<'_>) -> Option<Note> {
    match session.tick() {
        Step::Wait { note, .. } => note,
        other => panic!("{other:?}"),
    }
}

/// Asserts that an agent that never hears a reply after its welcome stops its job at the
/// welcome plus the bound, not earlier: the worker started 100 s before the welcome, the
/// heartbeats after it fail, and a step one millisecond short of the welcome plus the
/// bound stops nothing, while the step at it stops the job once. It catches a silence
/// counted from the start of the process (it would stop 100 s early) and a welcome that is
/// not stamped as contact.
#[test]
fn silence_is_counted_from_welcome_not_from_process_start() {
    let dir = Scratch::new("jobs-silence-welcome");
    let machine = FakeMachine::new();
    let worker = silenced(&dir, &machine, BOUND, &["r1"]);
    machine.clock.advance(100_000);
    let (mut script, _) = Script::new(vec![welcome(1)]);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    assert!(matches!(tick(&mut session), Some(Note::Registered { .. })));
    machine.clock.advance(BOUND_MS - 1);
    tick(&mut session);
    assert!(worker.handle().settle());
    assert!(stopped(&machine).is_empty());
    machine.clock.advance(1);
    assert!(worker.handle().settle());
    assert_eq!(stopped(&machine), ["stop r1"]);
    let results = worker.snapshot().results;
    assert_eq!(results[0].detail.as_deref(), Some("silence-stop"));
}

/// Asserts that a server restart shorter than the bound stops nothing: after a welcome and
/// an accepted heartbeat, four heartbeats are refused with 503 a minute apart and a fifth
/// with `stale-epoch` (the server lost the session), twenty graces of silence in all but
/// one millisecond short of the bound; the second welcome and a heartbeat after it are
/// contact again, and a bound later the jobs still run, never stopped, though far more
/// than a bound has passed since the first contact. It catches a silence counted from the
/// first contact, or one that a refused heartbeat counts as contact for.
#[test]
fn a_server_restart_shorter_than_the_bound_stops_nothing() {
    let dir = Scratch::new("jobs-silence-restart");
    let machine = FakeMachine::new();
    let worker = silenced(&dir, &machine, BOUND, &["r1", "r2"]);
    let mut replies = vec![welcome(1), beat_ok()];
    replies.extend((0..4).map(|_| refused(503, "unavailable")));
    replies.extend([refused(409, "stale-epoch"), welcome(2), beat_ok()]);
    let (mut script, _) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    tick(&mut session);
    machine.clock.advance(5_000);
    tick(&mut session);
    for _ in 0..4 {
        machine.clock.advance(60_000);
        tick(&mut session);
        assert!(worker.handle().settle());
    }
    machine.clock.advance(59_999);
    tick(&mut session);
    assert!(worker.handle().settle());
    assert!(stopped(&machine).is_empty(), "{:?}", stopped(&machine));
    assert!(matches!(tick(&mut session), Some(Note::Registered { .. })));
    machine.clock.advance(BOUND_MS - 1);
    tick(&mut session);
    assert!(worker.handle().settle());
    assert!(stopped(&machine).is_empty(), "{:?}", stopped(&machine));
    assert!(worker.snapshot().running());
    assert_eq!(worker.snapshot().held.len(), 2);
}

/// Asserts that a reply the worker had no room for is contact all the same: with the
/// worker held inside a create, the replies that fill its queue come at 0 and the one
/// dropped as busy 200 s later; once the worker goes on, a step one millisecond short of
/// that dropped reply plus the bound stops nothing, and the step at it stops both jobs.
/// It catches a silence counted from the last reply the worker applied rather than the
/// last the session heard (it would stop both jobs 200 s early).
#[test]
fn contact_from_a_dropped_reply_still_counts() {
    let dir = Scratch::new("jobs-silence-dropped");
    let machine = FakeMachine::new();
    let worker = silenced(&dir, &machine, BOUND, &["r1"]);
    machine.world.hold_creates();
    assert_eq!(worker.try_deliver(starts(&["r2"])), Delivery::Delivered);
    machine.world.wait_entered(2);
    let mut replies = vec![welcome(1)];
    replies.extend((0..=QUEUE).map(|_| reply(200, start_answer(&["r1"]))));
    let (mut script, _) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    tick(&mut session);
    for n in 0..QUEUE {
        assert_eq!(tick(&mut session), None, "reply {n} has room");
    }
    machine.clock.advance(200_000);
    assert_eq!(tick(&mut session), Some(Note::Busy));
    machine.clock.advance(BOUND_MS - 1);
    machine.world.release();
    assert!(worker.handle().settle());
    assert!(stopped(&machine).is_empty(), "{:?}", stopped(&machine));
    machine.clock.advance(1);
    assert!(worker.handle().settle());
    assert_eq!(stopped(&machine), ["stop r1", "stop r2"]);
}

/// Asserts that a welcome whose grace plus the stop grace is not below the bound ends the
/// session with `Failed`, naming `--silence-stop` and the numbers: a bound of 25 s with a
/// stop grace of 1 s against a grace of 24 s (exactly at it) is refused, and against a
/// grace one millisecond shorter is taken. It catches a bound the server's own grace could
/// trip (a host the server still counts as present would stop its jobs), and a check that
/// refuses the bound one millisecond early.
#[test]
fn silence_stop_at_or_below_the_grace_is_refused() {
    let dir = Scratch::new("jobs-silence-grace");
    let machine = FakeMachine::new();
    let worker = silenced(&dir, &machine, Duration::from_secs(25), &[]);
    let (mut script, sent) = Script::new(vec![welcome_with(1, 5_000, 24_000)]);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    let Step::Failed(why) = session.tick() else {
        panic!("a bound at the grace is refused");
    };
    assert_eq!(
        why,
        "--silence-stop: 25000 ms is at or below the server's grace of 24000 ms plus the stop \
         grace of 1000 ms, so a host the server still counts as present could stop its jobs; \
         give a longer bound"
    );
    assert_eq!(sent.borrow().len(), 1, "the hello only");

    let (mut script, _) = Script::new(vec![welcome_with(1, 5_000, 23_999)]);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    assert!(matches!(tick(&mut session), Some(Note::Registered { .. })));
}
