//! The bell between the job worker and the session: the worker rings it for news, a worker
//! with no room backs the session off (never past the interval), and the whole agent reports a
//! start's `running` at once. Nothing here waits for a time: rings are counted, waits are
//! recorded, and every wait on the worker is for an event with a hang guard.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use js_agent::lifecycle::Wake as Bell;
use js_agent::{
    Delivery, Drain, HEARTBEAT_PATH, Handle, Note, QUEUE, Session, Sleeper, Step, Wake,
};
use js_agent_wire::HeldState;
use serde_json::{Value, json};

use super::drain::container_flags;
use super::fake::{FakeMachine, HANG_GUARD};
use super::{acks, job_start, start, start_answer, starts};
use crate::bounds::WakeBudget;
use crate::support::{
    Highest, Lowest, Scratch, Script, ScriptConnector, beat_ok, flags, hello, linux_host, lowest,
    refused, reply, welcome, welcome_with,
};

/// The drain the sessions of these tests carry; none of them drains.
const DRAIN: Drain = Drain {
    wait: Duration::from_secs(10),
    settle: Duration::from_secs(30),
};

/// A bell that counts its rings.
fn counting() -> (Bell, Arc<AtomicUsize>) {
    let rings = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&rings);
    let bell = Bell::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    });
    (bell, rings)
}

/// Proves the worker rings its bell once when it lists a start as starting (the image is
/// still being pulled), when that run runs and when it ends, and not when the server's
/// acknowledgement removes the result; and that a second bell given to the worker is never
/// rung. Catches a worker that rings for every publish, one that rings only at the end of a
/// run, one that does not ring for a start taken (so no heartbeat is held at the server for a
/// placement meanwhile), a bell that is replaced by a later one, and a ring for a result the
/// server itself acknowledged.
#[test]
fn the_worker_rings_for_a_start_taken_a_run_that_runs_and_ends_not_for_an_ack() {
    let dir = Scratch::new("jobs-bell-worker");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let (bell, rings) = counting();
    let (second, second_rings) = counting();
    worker.ring_on_news(bell);
    worker.ring_on_news(second);
    let running = |s: &js_agent::Snapshot| s.jobs.iter().any(|h| h.state == HeldState::Running);

    let image = job_start("r1").image;
    machine.world.begin_pull(&image);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert_eq!(worker.snapshot().jobs[0].state, HeldState::Starting);
    assert_eq!(rings.load(Ordering::SeqCst), 1, "a start taken is news");
    assert!(worker.handle().settle());
    assert_eq!(rings.load(Ordering::SeqCst), 1, "the same start rings once");

    machine.world.end_pull(&image, true);
    assert!(worker.handle().wait_until(&running, HANG_GUARD));
    assert!(worker.handle().settle());
    assert_eq!(rings.load(Ordering::SeqCst), 2, "the run runs");

    machine.world.exit("r1", 0);
    assert!(worker.handle().settle());
    assert!(worker.snapshot().jobs[0].result.is_some());
    assert_eq!(rings.load(Ordering::SeqCst), 3, "the run ended");

    let ack = acks("r1", "ACCEPTED");
    assert_eq!(worker.try_deliver(ack), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert!(worker.snapshot().jobs.is_empty());
    assert_eq!(
        rings.load(Ordering::SeqCst),
        3,
        "an acknowledgement is none"
    );
    assert_eq!(
        second_rings.load(Ordering::SeqCst),
        0,
        "the first bell stays"
    );
}

/// One tick, as the wait it asks for and the note it makes.
fn step(session: &mut Session<'_>) -> (Duration, Option<Note>) {
    match session.tick() {
        Step::Wait { after, note } => (after, note),
        other => panic!("{other:?}"),
    }
}

/// A reply that starts `run` and asks for the next heartbeat at once.
fn start_and_beat_now(run: &str) -> crate::support::Reply {
    let mut body = start_answer(&[run]);
    body["beat_now"] = Value::Bool(true);
    reply(200, body)
}

/// Proves the wait after a reply the worker had no room for is a backoff, never nothing, and
/// that it grows while the worker stays full: with the lowest jitter the first is 500 ms, the
/// second 1 s, the third 2 s, the third being the reply that also said `beat_now`; that the
/// session is not woken by the bell for it; and that a reply the worker takes ends the run of
/// them, so the wait after it is the interval and the next full worker waits 500 ms again.
/// Catches a busy reply followed by an instant heartbeat (a loop against a full worker), a
/// backoff that never grows, and one that is never reset.
#[test]
fn a_reply_the_worker_has_no_room_for_waits_a_growing_backoff_until_one_is_taken() {
    let dir = Scratch::new("jobs-bell-busy");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    machine.world.hold_creates();
    let filled = |first: &'static str| (0..QUEUE).map(move |_| reply(200, start_answer(&[first])));
    let mut replies = vec![welcome(1), reply(200, start_answer(&["r1"]))];
    replies.extend(filled("r1"));
    replies.extend([
        start_and_beat_now("r1"),
        reply(200, start_answer(&["r1"])),
        reply(200, start_answer(&["r1"])),
        beat_ok(),
        reply(200, start_answer(&["r2"])),
    ]);
    replies.extend(filled("r2"));
    replies.push(reply(200, start_answer(&["r2"])));
    let (mut script, _) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);

    step(&mut session);
    assert_eq!(step(&mut session), (Duration::from_secs(5), None));
    machine.world.wait_entered(1);
    for n in 0..QUEUE {
        assert_eq!(step(&mut session).1, None, "reply {n} has room");
    }
    assert_eq!(
        step(&mut session),
        (Duration::from_millis(500), Some(Note::Busy)),
        "a beat_now does not make it nothing"
    );
    assert!(!session.beats_on_news());
    assert_eq!(
        step(&mut session),
        (Duration::from_secs(1), Some(Note::Busy))
    );
    assert_eq!(
        step(&mut session),
        (Duration::from_secs(2), Some(Note::Busy))
    );
    assert!(!session.beats_on_news());

    machine.world.release();
    assert!(worker.handle().settle());
    assert_eq!(step(&mut session), (Duration::from_secs(5), None));
    assert!(session.beats_on_news(), "an accepted, empty reply");

    machine.world.hold_creates();
    assert_eq!(step(&mut session), (Duration::from_secs(5), None));
    assert!(session.beats_on_news(), "an accepted, delivered reply");
    machine.world.wait_entered(2);
    for n in 0..QUEUE {
        assert_eq!(step(&mut session).1, None, "reply {n} has room");
    }
    assert_eq!(
        step(&mut session),
        (Duration::from_millis(500), Some(Note::Busy)),
        "the delivered reply reset the backoff"
    );
    machine.world.release();
    assert!(worker.handle().settle());
}

/// How many replies in a row the worker has no room for in the tests of the busy cap: past the
/// sixth, an uncapped backoff from 1 s doubling would wait 32 s or more.
const FULL_FOR: usize = 10;

/// A session whose worker is full: it is welcomed with `interval_ms` (grace three intervals and
/// a millisecond), takes `r1`, whose create is held, fills the worker's queue with [`QUEUE`]
/// replies, and then has `then` left to answer. Returns the waits of the ticks after the queue is
/// full, with their notes, in order.
fn waits_of_a_full_worker(
    interval_ms: u64,
    jitter: &'static mut dyn js_agent::Jitter,
    then: Vec<crate::support::Reply>,
) -> Vec<(Duration, Option<Note>)> {
    let dir = Scratch::new("jobs-bell-busy-cap");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    machine.world.hold_creates();
    let ticks = then.len();
    let mut replies = vec![
        welcome_with(1, interval_ms, interval_ms * 3 + 1),
        reply(200, start_answer(&["r1"])),
    ];
    replies.extend((0..QUEUE).map(|_| reply(200, start_answer(&["r1"]))));
    replies.extend(then);
    let (mut script, _) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, jitter).with_jobs(&worker, DRAIN);
    step(&mut session);
    step(&mut session);
    machine.world.wait_entered(1);
    for n in 0..QUEUE {
        assert_eq!(step(&mut session).1, None, "reply {n} has room");
    }
    let waits = (0..ticks).map(|_| step(&mut session)).collect();
    machine.world.release();
    assert!(worker.handle().settle());
    waits
}

/// Proves a worker that stays full for ten replies in a row never stretches the wait before
/// the next heartbeat past the welcome's interval: the busy backoff grows from half its first
/// ceiling and is capped at the interval, so the host keeps beating inside its grace. Cases: the
/// default 5 s interval with the lowest jitter (0.5, 1, 2 and 4 s, then the interval) and with
/// the highest (1, 2 and 4 s, then the interval, the cap met from above by an 8 s ceiling), and a
/// 1 s interval with both (the cap met at the first or second reply). Catches a busy reply that
/// waits the uncapped failure backoff, whose sixth and later waits are 16 s to 60 s.
#[test]
fn a_worker_full_for_ten_replies_never_waits_past_the_interval() {
    let ms = Duration::from_millis;
    let cases: [(u64, &'static mut dyn js_agent::Jitter, [u64; 4]); 4] = [
        (5_000, lowest(), [500, 1_000, 2_000, 4_000]),
        (
            5_000,
            Box::leak(Box::new(Highest)),
            [1_000, 2_000, 4_000, 5_000],
        ),
        (1_000, lowest(), [500, 1_000, 1_000, 1_000]),
        (
            1_000,
            Box::leak(Box::new(Highest)),
            [1_000, 1_000, 1_000, 1_000],
        ),
    ];
    for (interval_ms, jitter, first) in cases {
        let busy = (0..FULL_FOR)
            .map(|_| reply(200, start_answer(&["r1"])))
            .collect();
        let waits = waits_of_a_full_worker(interval_ms, jitter, busy);
        let mut expected: Vec<_> = first
            .iter()
            .map(|&wait| (ms(wait), Some(Note::Busy)))
            .collect();
        expected.resize(FULL_FOR, (ms(interval_ms), Some(Note::Busy)));
        assert_eq!(waits, expected, "interval {interval_ms} ms");
    }
}

/// Proves a reply the worker had no room for still ends a run of failed exchanges, as every
/// accepted heartbeat does: after ten such replies a heartbeat answered 503 waits the first
/// failure's backoff (500 ms at the lowest jitter), and the busy reply after it is still at
/// the cap, the interval, since the failure did not empty the worker. Catches busy replies
/// counted as failures, which would make that 503 wait 30 s, past the 15 s grace.
#[test]
fn busy_replies_do_not_lengthen_the_backoff_of_a_failure_after_them() {
    let ms = Duration::from_millis;
    let mut then: Vec<_> = (0..FULL_FOR)
        .map(|_| reply(200, start_answer(&["r1"])))
        .collect();
    then.push(refused(503, "unavailable"));
    then.push(reply(200, start_answer(&["r1"])));
    let waits = waits_of_a_full_worker(5_000, lowest(), then);
    let (failed, after) = (&waits[FULL_FOR], &waits[FULL_FOR + 1]);
    assert_eq!(failed.0, ms(500), "{failed:?}");
    assert!(
        matches!(
            failed.1,
            Some(Note::Retrying {
                what: "heartbeat",
                ..
            })
        ),
        "{failed:?}"
    );
    assert_eq!(
        after,
        &(ms(5_000), Some(Note::Busy)),
        "still full, at the cap"
    );
}

/// A sleeper for the whole agent: it rings a flag as its bell, settles the worker at a wait
/// that may end on the bell (so the worker has published by then), ends that wait with
/// [`Wake::Bell`] when the flag was rung and with [`Wake::Elapsed`] when it was not, and
/// records each wait as the method asked and what it returned.
struct Ringing<'a> {
    seen: &'a RefCell<Option<Handle>>,
    rung: Arc<AtomicBool>,
    waits: &'a RefCell<Vec<(&'static str, Wake)>>,
    budget: WakeBudget,
}

impl Sleeper for Ringing<'_> {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        self.budget.spend();
        self.waits.borrow_mut().push(("sleep", Wake::Elapsed));
        Wake::Elapsed
    }

    fn sleep_or_bell(&mut self, _duration: Duration) -> Wake {
        self.budget.spend();
        let handle = self.seen.borrow().clone().expect("the hook was told");
        assert!(handle.settle());
        let woken = match self.rung.swap(false, Ordering::SeqCst) {
            true => Wake::Bell,
            false => Wake::Elapsed,
        };
        self.waits.borrow_mut().push(("sleep_or_bell", woken));
        woken
    }

    fn bell(&self) -> Option<Bell> {
        let rung = Arc::clone(&self.rung);
        Some(Bell::new(move || rung.store(true, Ordering::SeqCst)))
    }
}

/// Proves, through the whole agent, that a start's `running` is reported at once: the server
/// answers the first heartbeat with a start, the worker runs it and rings the sleeper's bell,
/// the wait after that heartbeat (a `sleep_or_bell`, the one after a heartbeat the server
/// accepted) ends on the bell, and the next heartbeat lists the run as running. The wait after
/// the hello is a plain sleep, and the wait after the second heartbeat, which the bell did not
/// ring for, is not ended by it. Catches a bell never given to the worker, a bell that rings
/// nothing awake, and a running run reported only at the interval.
#[test]
fn a_start_is_reported_running_by_the_next_heartbeat_when_the_bell_ends_the_wait() {
    let dir = Scratch::new("jobs-bell-agent");
    let owned = container_flags(&dir);
    let extra: Vec<&str> = owned.iter().map(String::as_str).collect();
    let args = flags(&extra);
    let replies = vec![
        welcome(1),
        reply(200, start_answer(&["r1"])),
        beat_ok(),
        crate::support::refused(401, "unauthorised"),
    ];
    let (script, sent) = Script::new(replies);
    let connector = ScriptConnector::of(script);
    let seen: RefCell<Option<Handle>> = RefCell::new(None);
    let hook = |handle: Handle| *seen.borrow_mut() = Some(handle);
    let waits = RefCell::new(Vec::new());
    let mut sleeper = Ringing {
        seen: &seen,
        rung: Arc::default(),
        waits: &waits,
        budget: WakeBudget::new(),
    };
    let machine = Arc::new(FakeMachine::new());
    let jobs: crate::support::Jobs<'_> = (machine.clone(), Some(&hook));
    let ran = crate::support::run_on(
        &args,
        &linux_host(),
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
    );
    assert_eq!(ran.status(), crate::support::code(2), "{}", ran.err);

    assert_eq!(
        *waits.borrow(),
        [
            ("sleep", Wake::Elapsed),
            ("sleep_or_bell", Wake::Bell),
            ("sleep_or_bell", Wake::Elapsed),
        ]
    );
    let sent = sent.borrow();
    let jobs = |n: usize| sent[n].1["jobs"].clone();
    assert_eq!(sent[1].0, HEARTBEAT_PATH);
    assert_eq!(sent[1].1["held"], json!([]));
    assert_eq!(sent[2].1["held"], json!([]));
    assert_eq!(jobs(1), Value::Null);
    assert_eq!(jobs(2), json!([{ "job_id": "r1", "state": "running" }]));
    assert_eq!(machine.world.created(), ["r1"]);
}
