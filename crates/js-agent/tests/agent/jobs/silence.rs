//! The silence bound through the session and the worker: the session stamps every answer
//! the server wrote to a hello or a heartbeat, whatever its status, on the worker's clock
//! (a reply the worker had no room for too); once the server has not been heard for the
//! bound the worker's lifecycle refuses new starts and keeps its running jobs, whose real
//! results reach the server when it answers again; and a welcome whose grace the bound
//! does not outlast ends the session. The clock is the test's; every wait on the worker is
//! for an order done.

use std::time::Duration;

use js_agent::{Delivery, Drain, Note, QUEUE, Session, Step, Worker};
use js_agent_wire::{Heartbeat, HeldState, Outcome, RunResult};
use serde_json::Value;

use super::fake::FakeMachine;
use super::{STEP_NEVER, plan, start_answer};
use crate::lifecycle::silence::long_run;
use crate::support::{
    Reply, Scratch, Script, Sent, beat_ok, hello, lowest, refused, reply, welcome, welcome_with,
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

/// The detail of a start the agent did not make for a silence.
const SILENCE_STOP: &str = "silence-stop";

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

/// A worker that stops starting work after `bound` of silence (stop grace 1 s), with `runs`
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

/// Hands the worker a start of `run` straight, as the dispatch poller does (no answer of
/// the server with it, so no contact), and says whether the worker took it: `false` when it
/// refused it `silence-stop`.
fn takes_a_start(worker: &Worker, run: &str) -> bool {
    assert_eq!(worker.try_deliver(starts(&[run])), Delivery::Delivered);
    assert!(worker.handle().settle());
    let results = worker.snapshot().results;
    let refused = results.iter().find(|result| result.run_id == run);
    match refused {
        None => true,
        Some(result) => {
            let not_started = (Outcome::FailedToStart, Some(SILENCE_STOP));
            assert_eq!((result.outcome, result.detail.as_deref()), not_started);
            false
        }
    }
}

/// `n` answers of `answer`, each a fresh copy.
fn times(n: usize, answer: impl Fn() -> Reply) -> Vec<Reply> {
    (0..n).map(|_| answer()).collect()
}

/// The last heartbeat sent.
fn last_beat(sent: &Sent) -> Heartbeat {
    let sent = sent.borrow();
    let beats = sent
        .iter()
        .filter(|(path, _)| path == js_agent::HEARTBEAT_PATH);
    let (_, body) = beats.last().expect("a heartbeat was sent");
    serde_json::from_value(body.clone()).expect("a heartbeat")
}

/// The results the last heartbeat sent carried.
fn last_results(sent: &Sent) -> Vec<RunResult> {
    last_beat(sent).results
}

/// Ticks the session once per `every_ms` for `answers` ticks, settling the worker after
/// each, and asserts the runtime was asked to stop nothing.
fn tick_through(
    session: &mut Session<'_>,
    machine: &FakeMachine,
    worker: &Worker,
    answers: usize,
    every_ms: u64,
) {
    for n in 0..answers {
        machine.clock.advance(every_ms);
        tick(session);
        assert!(worker.handle().settle());
        assert!(
            stopped(machine).is_empty(),
            "tick {n}: {:?}",
            stopped(machine)
        );
    }
}

/// Asserts that a welcome is contact and that the silence after it is counted from it: the
/// worker started 100 s before the welcome, the heartbeats after it fail, a start one
/// millisecond short of the welcome plus the bound is taken and one at it is refused, and
/// the running job is never stopped. It catches a silence counted from the start of the
/// process (it would refuse 100 s early), a welcome that is not stamped as contact, and a
/// silence that stops a running job.
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
    assert!(takes_a_start(&worker, "p1"));
    machine.clock.advance(1);
    assert!(!takes_a_start(&worker, "p2"));
    assert!(stopped(&machine).is_empty());
    assert!(worker.snapshot().running());
}

/// Asserts that a server that hears every heartbeat but answers it 503 with its own
/// `error` code for 301 s, and once 409 `stale-epoch` (it lost the session), is heard
/// throughout: the running job is never stopped, a start after those 301 s is taken, and
/// once the heartbeats are answered 200 again the job ends as its container does and the
/// next heartbeat carries its real result (`succeeded`, exit 0). It catches a refusal with
/// the server's body counted as silence, which stops every job on every host while a
/// server's store fails or a deploy is bad.
#[test]
fn a_server_answering_503_for_longer_than_the_bound_keeps_the_job() {
    let dir = Scratch::new("jobs-silence-503");
    let machine = FakeMachine::new();
    let worker = silenced(&dir, &machine, BOUND, &["r1"]);
    let mut replies = vec![welcome(1), beat_ok()];
    replies.extend(times(6, || refused(503, "state-write-failed")));
    replies.extend([refused(409, "stale-epoch"), welcome(2)]);
    replies.extend(times(3, beat_ok));
    let (mut script, sent) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    tick(&mut session);
    tick_through(&mut session, &machine, &worker, 1, 5_000);
    // Six 503s and the 409, 43 s apart: 301 s with no 200.
    tick_through(&mut session, &machine, &worker, 7, 43_000);
    assert!(takes_a_start(&worker, "p1"), "the 503s were contact");
    assert!(matches!(tick(&mut session), Some(Note::Registered { .. })));
    tick_through(&mut session, &machine, &worker, 1, 5_000);

    machine.world.exit("r1", 0);
    assert!(worker.handle().settle());
    tick_through(&mut session, &machine, &worker, 1, 5_000);
    let results = last_results(&sent);
    let r1: Vec<_> = results.iter().filter(|r| r.run_id == "r1").collect();
    assert_eq!(r1.len(), 1, "{results:?}");
    assert_eq!(
        (r1[0].outcome, r1[0].exit_code, r1[0].detail.as_deref()),
        (Outcome::Succeeded, Some(0), None)
    );
}

/// Asserts that answers that never arrive in time (each reply later than the transport's
/// request limit, which the transport reports as an error, 11 s apart for 301 s) are
/// silence and still stop no running job: a start past the bound is refused `silence-stop`,
/// the job runs on and ends as its container does during the silence, and the first
/// heartbeat the server answers after it carries the job's real result (`failed`, exit 3)
/// beside the refusal. It catches a silence that stops a running job, and a result made
/// during a silence that is lost or rewritten.
#[test]
fn replies_later_than_the_limit_for_longer_than_the_bound_keep_the_job() {
    let dir = Scratch::new("jobs-silence-late");
    let machine = FakeMachine::new();
    let worker = silenced(&dir, &machine, BOUND, &["r1"]);
    let late =
        || Err::<js_agent::Answer, String>("cannot talk to the server: timed out".to_owned());
    let mut replies = vec![welcome(1), beat_ok()];
    replies.extend(times(28, late));
    replies.extend(times(2, beat_ok));
    let (mut script, sent) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    tick(&mut session);
    tick_through(&mut session, &machine, &worker, 1, 5_000);
    // 28 heartbeats, each answered too late, 11 s apart: 308 s unheard.
    tick_through(&mut session, &machine, &worker, 28, 11_000);
    assert!(!takes_a_start(&worker, "p1"), "late answers are silence");
    assert!(worker.snapshot().running());
    machine.world.exit("r1", 3);
    assert!(worker.handle().settle());
    tick_through(&mut session, &machine, &worker, 1, 11_000);
    let results = last_results(&sent);
    let mut seen: Vec<_> = results
        .iter()
        .map(|r| {
            (
                r.run_id.as_str(),
                r.outcome,
                r.exit_code,
                r.detail.as_deref(),
            )
        })
        .collect();
    seen.sort_by_key(|(id, ..)| *id);
    assert_eq!(
        seen,
        [
            ("p1", Outcome::FailedToStart, None, Some(SILENCE_STOP)),
            ("r1", Outcome::Failed, Some(3), None),
        ]
    );
    assert!(takes_a_start(&worker, "p2"), "the 200 ended the silence");
}

/// Asserts that a server that cannot be reached at all past the bound stops the starts and
/// not the work: a start one millisecond short of the bound is taken, one at it is refused,
/// the two running jobs are never stopped (one ends during the silence with exit 0, the
/// other still runs), and when the server is reached again the heartbeat carries the ended
/// job's real result and lists the other as running. It catches a silence that stops a
/// running job, and one that takes new work it could not report.
#[test]
fn true_silence_refuses_new_starts_and_keeps_the_running_jobs() {
    let dir = Scratch::new("jobs-silence-true");
    let machine = FakeMachine::new();
    let worker = silenced(&dir, &machine, BOUND, &["r1", "r2"]);
    let down = || Err::<js_agent::Answer, String>("cannot talk to the server: refused".to_owned());
    let mut replies = vec![welcome(1)];
    replies.extend(times(5, down));
    replies.extend(times(2, beat_ok));
    let (mut script, sent) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    tick(&mut session);
    tick_through(&mut session, &machine, &worker, 5, 59_999);
    assert!(takes_a_start(&worker, "p1"));
    machine.clock.advance(5);
    assert!(!takes_a_start(&worker, "p2"));
    machine.world.exit("r1", 0);
    assert!(worker.handle().settle());
    machine.clock.advance(10 * BOUND_MS);
    assert!(worker.handle().settle());
    assert!(stopped(&machine).is_empty(), "{:?}", stopped(&machine));
    tick_through(&mut session, &machine, &worker, 1, 1_000);
    let results = last_results(&sent);
    let r1 = results
        .iter()
        .find(|r| r.run_id == "r1")
        .expect("r1's result");
    assert_eq!(
        (r1.outcome, r1.exit_code, r1.detail.as_deref()),
        (Outcome::Succeeded, Some(0), None)
    );
    let running: Vec<String> = last_beat(&sent)
        .held
        .into_iter()
        .filter(|run| run.state == HeldState::Running)
        .map(|run| run.run_id)
        .collect();
    assert_eq!(running, ["r2", "p1"]);
}

/// Asserts that an answer whose body the server did not write is not contact: 503s with an
/// empty body (what a proxy in front of a server that is down answers), for longer than the
/// bound, leave a start refused `silence-stop`, while the running job is not stopped. It
/// catches a contact rule that any HTTP status satisfies, which a proxy would keep
/// satisfying for a server that is gone.
#[test]
fn a_proxys_empty_503_is_not_contact() {
    let dir = Scratch::new("jobs-silence-proxy");
    let machine = FakeMachine::new();
    let worker = silenced(&dir, &machine, BOUND, &["r1"]);
    let mut replies = vec![welcome(1)];
    replies.extend(times(6, || reply(503, Value::Null)));
    let (mut script, _) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    tick(&mut session);
    tick_through(&mut session, &machine, &worker, 6, 50_000);
    assert!(!takes_a_start(&worker, "p1"));
    assert!(worker.snapshot().running());
}

/// Asserts that a reply the worker had no room for is contact all the same: with the
/// worker held inside a create, the replies that fill its queue come at 0 and the one
/// dropped as busy 200 s later; once the worker goes on, a start one millisecond short of
/// that dropped reply plus the bound is taken and one at it is refused, and no job is
/// stopped. It catches a silence counted from the last reply the worker applied rather
/// than the last the session heard (it would refuse 200 s early).
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
    assert!(takes_a_start(&worker, "p1"));
    machine.clock.advance(1);
    assert!(!takes_a_start(&worker, "p2"));
    assert!(stopped(&machine).is_empty(), "{:?}", stopped(&machine));
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
         grace of 1000 ms, so a host the server still counts as present could stop starting \
         work; give a longer bound"
    );
    assert_eq!(sent.borrow().len(), 1, "the hello only");

    let (mut script, _) = Script::new(vec![welcome_with(1, 5_000, 23_999)]);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    assert!(matches!(tick(&mut session), Some(Note::Registered { .. })));
}
