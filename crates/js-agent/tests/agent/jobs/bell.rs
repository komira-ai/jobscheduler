//! The bell between the job worker and the session: the worker rings it for news, a worker
//! with no room backs the session off, and the whole agent reports a start's `running` at once.
//! Nothing here waits for a time: rings are counted, waits are recorded, and every wait on the
//! worker is for an event with a hang guard.

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
use super::{acks, start, start_answer, starts};
use crate::bounds::WakeBudget;
use crate::lifecycle::job;
use crate::support::{
    Lowest, Scratch, Script, ScriptConnector, beat_ok, flags, hello, linux_host, lowest, reply,
    welcome,
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

/// Proves the worker rings its bell when a run it started runs and when that run ends, not
/// when it lists the start as starting (the image is still being pulled), and not when the
/// server's acknowledgement removes the result; and that a second bell given to the worker
/// is never rung. Catches a worker that rings for every publish, one that rings only at the
/// end of a run, a bell that is replaced by a later one, and a ring for a result the server
/// itself acknowledged.
#[test]
fn the_worker_rings_for_a_run_that_runs_and_ends_not_for_a_start_taken_or_an_ack() {
    let dir = Scratch::new("jobs-bell-worker");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let (bell, rings) = counting();
    let (second, second_rings) = counting();
    worker.ring_on_news(bell);
    worker.ring_on_news(second);
    let running = |s: &js_agent::Snapshot| s.held.iter().any(|h| h.state == HeldState::Running);

    let image = job("r1").image;
    machine.world.begin_pull(&image);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert_eq!(worker.snapshot().held[0].state, HeldState::Starting);
    assert_eq!(rings.load(Ordering::SeqCst), 0, "a start taken is no news");

    machine.world.end_pull(&image, true);
    assert!(worker.handle().wait_until(&running, HANG_GUARD));
    assert!(worker.handle().settle());
    assert_eq!(rings.load(Ordering::SeqCst), 1, "the run runs");

    machine.world.exit("r1", 0);
    assert!(worker.handle().settle());
    assert_eq!(worker.snapshot().results.len(), 1);
    assert_eq!(rings.load(Ordering::SeqCst), 2, "the run ended");

    let ack = acks("r1", "ACCEPTED");
    assert_eq!(worker.try_deliver(ack), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert!(worker.snapshot().results.is_empty());
    assert_eq!(
        rings.load(Ordering::SeqCst),
        2,
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
    let held = |n: usize| sent[n].1["held"].clone();
    assert_eq!(sent[1].0, HEARTBEAT_PATH);
    assert_eq!(held(1), json!([]));
    assert_eq!(held(2), json!([{ "run_id": "r1", "state": "running" }]));
    assert_eq!(machine.world.created(), ["r1"]);
}
