//! What the poller does with an answer its worker has no room for: it holds the messages,
//! posts nothing while the worker's queue is full, hands them over when the worker makes room
//! and acknowledges them in the poll it posts right after; and what it counts and logs. The
//! worker's queue is filled behind a start held inside the fake runtime's create, and the
//! clock is the fake machine's; nothing waits for a time.

use std::time::Duration;

use js_agent::dispatch::{Counts, Next, Poller};
use js_agent::{Delivery, QUEUE, Snapshot, Worker};
use js_agent_wire::Dispatched;
use serde_json::json;

use super::super::fake::FakeMachine;
use super::super::{rescue, start, starts};
use super::{acked, handed, polls, quiet, route, spec};
use crate::support::{Lowest, Scratch, Script, refused, reply};

/// Holds the worker inside the create of a start and fills its queue behind it, so the next
/// delivery finds it full.
pub(super) fn fill(worker: &Worker, machine: &FakeMachine) {
    machine.world.hold_creates();
    assert_eq!(worker.try_deliver(starts(&["r0"])), Delivery::Delivered);
    machine.world.wait_entered(1);
    for _ in 0..QUEUE {
        assert_eq!(worker.try_deliver(starts(&["r0"])), Delivery::Delivered);
    }
}

/// The ids of the jobs the worker holds.
fn jobs(worker: &Worker) -> Vec<String> {
    let jobs = worker.snapshot().jobs.into_iter().map(|job| job.job_id);
    jobs.collect()
}

/// Asserts that a poll whose answer hands out two starts while the worker's queue is full
/// acknowledges neither and posts no further poll while the queue stays full; that once the
/// worker makes room both reach it, and the next poll (posted at once) acknowledges both; and
/// that the poll after that carries none. Catches the poller that dropped what the worker had
/// no room for and polled on without acknowledging it, which left each start to the server's
/// ten-second claim before it was handed out again.
#[test]
fn starts_the_worker_had_no_room_for_are_held_handed_over_at_its_room_then_acknowledged() {
    let dir = Scratch::new("jobs-dispatch-held");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    fill(&worker, &machine);
    let two = vec![Dispatched::start(spec("j1")), Dispatched::start(spec("j2"))];
    let (mut script, sent) = Script::new(vec![handed(two), quiet(), quiet()]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    let full = poller.poll(&mut script, &route(1));
    let still = poller.poll(&mut script, &route(1));
    assert_eq!(
        polls(&sent).len(),
        1,
        "nothing posted while the queue is full"
    );
    assert!(matches!(full, Next::Room(_)), "{full:?}");
    assert_eq!(full, still);
    machine.world.release();
    assert!(worker.handle().settle());
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert!(worker.handle().settle());
    // `r0` is the start that held the worker inside its create while the queue was full.
    assert_eq!(jobs(&worker), ["r0", "j1", "j2"]);
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);

    let bodies = polls(&sent);
    assert_eq!(bodies.len(), 3);
    assert_eq!(acked(&bodies[0]), Vec::<String>::new());
    assert_eq!(acked(&bodies[1]), ["start:alloc-j1", "start:alloc-j2"]);
    assert_eq!(acked(&bodies[2]), Vec::<String>::new());
}

/// Asserts that the count `Next::Room` carries is the worker's count of orders taken from
/// before the try that found the queue full, so a wait for room from it ends once the worker
/// takes one order; and that a hand-over that finds the queue still full keeps the messages
/// and carries the count read before that try. Catches a count read after the refusal (an
/// order taken in between would be missed and the messages held until the wait's bound).
#[test]
fn the_room_count_is_the_one_read_before_the_refused_try() {
    let dir = Scratch::new("jobs-dispatch-room-count");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    fill(&worker, &machine);
    let taken = worker.taken();
    let (mut script, _) = Script::new(vec![handed(vec![Dispatched::start(spec("j1"))])]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    assert_eq!(poller.poll(&mut script, &route(1)), Next::Room(taken));
    assert_eq!(poller.hand_over(), Some(Next::Room(taken)));
    assert!(!worker.wait_room(taken, Duration::ZERO), "still full");
    machine.world.release();
    assert!(worker.wait_room(taken, super::GUARD), "an order was taken");
    assert!(worker.handle().settle());
    assert_eq!(poller.hand_over(), None, "handed over");
    assert_eq!(poller.hand_over(), None, "nothing held");
}

/// Asserts what the poller counts and logs, each count rising once for each event: an answer
/// the worker has no room for counts one undelivered answer however often the held messages
/// are tried again, and its hand-over logs how long after their arrival the worker took them
/// (on the lifecycle's clock); each failed poll counts one, whatever failed (no answer, a
/// status but 200, an answer that is not a usable reply, no transport), and neither count
/// moves the other. The lines are compared whole. Catches a held answer counted again at each
/// try, a failure that is not counted or counted as undelivered, and lines that do not say
/// which happened.
#[test]
fn each_count_rises_once_for_each_event_and_each_event_is_one_line() {
    let dir = Scratch::new("jobs-dispatch-counts");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    fill(&worker, &machine);
    let two = vec![
        Dispatched::start(spec("j1")),
        Dispatched::stop("j9".to_owned(), "alloc-j9".to_owned()),
    ];
    let (mut script, _) = Script::new(vec![
        handed(two),
        Err("the connection was lost".to_owned()),
        refused(503, "unavailable"),
        reply(200, json!({ "messages": "none" })),
        quiet(),
    ]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));
    let counts = |undelivered, failed| Counts {
        undelivered,
        failed,
    };

    assert_eq!(poller.counts(), counts(0, 0));
    assert!(matches!(poller.poll(&mut script, &route(1)), Next::Room(_)));
    assert!(matches!(poller.poll(&mut script, &route(1)), Next::Room(_)));
    assert_eq!(poller.counts(), counts(1, 0), "one answer, tried twice");
    assert_eq!(
        poller.take_lines(),
        ["dispatch: the job worker has no room for 2 message(s) (undelivered answers: 1)"]
    );
    machine.clock.advance(700);
    machine.world.release();
    assert!(worker.handle().settle());
    assert_eq!(
        poller.poll(&mut script, &route(1)),
        Next::After(Duration::from_millis(500))
    );
    assert_eq!(
        poller.poll(&mut script, &route(1)),
        Next::After(Duration::from_secs(1))
    );
    assert_eq!(
        poller.poll(&mut script, &route(1)),
        Next::After(Duration::from_secs(2))
    );
    assert_eq!(poller.failed(None), Next::After(Duration::from_secs(4)));
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert_eq!(poller.counts(), counts(1, 4));
    assert_eq!(
        poller.take_lines(),
        [
            "dispatch: the job worker took 2 held message(s) 700 ms after they arrived",
            "dispatch: a poll failed (it got no answer); the next in 500 ms (failed polls: 1)",
            "dispatch: a poll failed (it was answered 503); the next in 1000 ms (failed polls: 2)",
            "dispatch: a poll failed (its answer was not a usable reply); the next in 2000 ms \
             (failed polls: 3)",
            "dispatch: a poll failed (no transport could be made); the next in 4000 ms \
             (failed polls: 4)",
        ]
    );
    assert_eq!(poller.take_lines(), Vec::<String>::new(), "taken once");
}

/// Asserts that held messages are stamped with the time their answer arrived, not the time
/// the worker took them: the start's timeline, which the worker logs with the start, counts
/// its `taken` step from the arrival, 700 ms before the hand-over. Catches held starts stamped
/// at the hand-over, which would hide the hold from the start's timeline.
#[test]
fn held_starts_are_stamped_with_the_arrival_of_their_answer() {
    let dir = Scratch::new("jobs-dispatch-stamp");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    fill(&worker, &machine);
    let (mut script, _) = Script::new(vec![handed(vec![Dispatched::start(spec("j1"))])]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    assert!(matches!(poller.poll(&mut script, &route(1)), Next::Room(_)));
    machine.clock.advance(700);
    machine.world.release();
    assert!(worker.handle().settle());
    assert_eq!(poller.hand_over(), None);
    assert!(worker.handle().settle());

    let lines = worker.take_lines();
    let started: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains("\"j1\""))
        .collect();
    assert_eq!(started.len(), 1, "{lines:?}");
    assert!(
        started[0].contains("received_ms: 0,") && started[0].contains("(\"taken\", 700)"),
        "{}",
        started[0]
    );
}

/// Asserts that messages handed to a worker whose queue nobody reads any more are dropped
/// at once: the answer counts as undelivered, the line says they were dropped, the poller
/// polls on at once and acknowledges nothing. Catches a poller that holds messages for a
/// worker that will never take them (it would stop polling for good).
#[test]
fn messages_for_a_worker_that_is_gone_are_dropped_and_not_acknowledged() {
    let dir = Scratch::new("jobs-dispatch-gone");
    let machine = FakeMachine::new();
    let worker = Worker::orphaned(
        machine.clock.clone(),
        Snapshot::default(),
        rescue(&dir, &machine),
    );
    let (mut script, sent) =
        Script::new(vec![handed(vec![Dispatched::start(spec("j1"))]), quiet()]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);

    assert_eq!(poller.counts().undelivered, 1);
    assert_eq!(
        poller.take_lines(),
        ["dispatch: the job worker is gone: dropped 1 message(s) (undelivered answers: 1)"]
    );
    assert_eq!(acked(&polls(&sent)[1]), Vec::<String>::new());
}

/// Asserts that messages held while the worker ends (its runtime panics) are dropped at the
/// next hand-over, with a line, and the poller polls on without acknowledging them. Catches
/// held messages offered for ever to a worker that has ended.
#[test]
fn held_messages_are_dropped_when_the_worker_ends() {
    let dir = Scratch::new("jobs-dispatch-ended");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    fill(&worker, &machine);
    let two = vec![Dispatched::start(spec("j1")), Dispatched::start(spec("j2"))];
    let (mut script, sent) = Script::new(vec![handed(two), quiet()]);
    let mut poller = Poller::new(&worker, Box::new(Lowest));

    assert!(matches!(poller.poll(&mut script, &route(1)), Next::Room(_)));
    machine.world.panic_at("poll");
    machine.world.release();
    assert!(!worker.handle().settle(), "the worker ended");
    assert!(worker.ended());
    assert_eq!(poller.hand_over(), None);
    assert_eq!(poller.poll(&mut script, &route(1)), Next::Again);

    assert_eq!(
        poller.take_lines(),
        [
            "dispatch: the job worker has no room for 2 message(s) (undelivered answers: 1)",
            "dispatch: the job worker ended: 2 held message(s) dropped",
        ]
    );
    assert_eq!(acked(&polls(&sent)[1]), Vec::<String>::new());
}
