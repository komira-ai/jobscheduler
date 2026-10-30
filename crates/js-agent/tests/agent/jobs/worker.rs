//! The worker on its own: what it holds after an order, what it does with a reply that
//! waited, and how it starts, steps, closes and ends.

use std::time::Duration;

use js_agent::{Delivery, LINES_CAP, QUEUE};
use js_agent_wire::{Command, HeartbeatReply, HeldState, Outcome};

use super::fake::{Build, FakeMachine, HANG_GUARD};
use super::{STEP_NEVER, acks, plan, start, starts, stops};
use crate::lifecycle::job;
use crate::support::Scratch;

/// Asserts the path of a result through the worker: a start runs the container and the run
/// is held as running; when the container exits the result is published and the run is
/// listed as exited, and the container and its scratch are gone; the result stays in the
/// snapshot until an acknowledgement names it, and then the run is not held. It catches a
/// result that is dropped before the server acknowledged it, and one that is never
/// published.
#[test]
fn a_result_stays_published_until_it_is_acknowledged() {
    let dir = Scratch::new("jobs-worker-result");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);

    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert_eq!(machine.world.created(), ["r1"]);
    let snapshot = worker.snapshot();
    assert_eq!(snapshot.held.len(), 1);
    assert_eq!(snapshot.held[0].state, HeldState::Running);
    assert!(snapshot.results.is_empty());

    machine.world.exit("r1", 0);
    assert!(worker.handle().settle());
    let snapshot = worker.snapshot();
    assert_eq!(snapshot.held[0].state, HeldState::ExitedUnacked);
    assert_eq!(snapshot.results.len(), 1);
    assert_eq!(snapshot.results[0].outcome, Outcome::Succeeded);
    assert_eq!(machine.world.alive(), 0, "the container is removed");

    assert!(worker.handle().settle());
    assert_eq!(worker.snapshot().results.len(), 1, "no ack, no drop");
    assert_eq!(
        worker.try_deliver(acks("r1", "ACCEPTED")),
        Delivery::Delivered
    );
    assert!(worker.handle().settle());
    let snapshot = worker.snapshot();
    assert!(snapshot.held.is_empty() && snapshot.results.is_empty());
}

/// Asserts that a reply that waits in the queue for a worker busy in a slow runtime call
/// keeps the time it arrived: a start with a validity of one second that arrived while the
/// worker was inside a call, and was applied five seconds later on the agent's clock, is
/// stale and creates nothing. It catches a worker that counts `valid_for_ms` from the
/// moment it gets to the reply, so a start that outlived its lease runs.
#[test]
fn a_start_that_waited_for_the_worker_is_stale() {
    let dir = Scratch::new("jobs-worker-stale");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);

    machine.world.hold_creates();
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    machine.world.wait_entered(1);
    let mut late = starts(&["r2"]);
    if let Command::Start(spec) = &mut late.commands[0] {
        spec.valid_for_ms = 1000;
    }
    assert_eq!(worker.try_deliver(late), Delivery::Delivered);
    machine.clock.advance(5000);
    machine.world.release();
    assert!(worker.handle().settle());

    assert_eq!(machine.world.created(), ["r1"], "r2 was stale on arrival");
    let held: Vec<String> = worker
        .snapshot()
        .held
        .into_iter()
        .map(|r| r.run_id)
        .collect();
    assert_eq!(held, ["r1"]);
}

/// Asserts that a worker whose queue is full drops the next reply and says so (`false`)
/// instead of waiting: with the worker inside a call that does not return, exactly
/// [`QUEUE`] replies are taken and the next is refused; and once the call returns they are
/// all dealt with. It catches a hand-over that blocks the session on a busy worker.
#[test]
fn a_full_queue_refuses_the_next_reply_without_waiting() {
    let dir = Scratch::new("jobs-worker-full");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);

    machine.world.hold_creates();
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    machine.world.wait_entered(1);
    for n in 0..QUEUE {
        assert_eq!(
            worker.try_deliver(starts(&["r1"])),
            Delivery::Delivered,
            "reply {n} has room"
        );
    }
    assert_eq!(
        worker.try_deliver(starts(&["r1"])),
        Delivery::Full,
        "the queue is full"
    );

    machine.world.release();
    assert!(worker.handle().settle());
    assert_eq!(machine.world.created(), ["r1"], "a start runs once");
}

/// Asserts the ways a worker fails to start: a runtime that cannot be made, a runtime that
/// panics while it is made, a host the agent cannot see, and an engine that cannot list the
/// containers of an earlier run. Each is an error with its reason, none a hang, and none
/// leaves a worker. It catches an agent that registers with a held list it does not know.
#[test]
fn a_worker_that_cannot_start_says_why() {
    let dir = Scratch::new("jobs-worker-fail");
    let cases = [
        (Build::RuntimeFails, "the engine does not answer"),
        (Build::Panics, "stopped while it started"),
        (Build::HostNotSeen, "pid namespace of its own"),
    ];
    for (build, text) in cases {
        let machine = FakeMachine::failing(build);
        let why = js_agent::Worker::start(plan(&dir, &machine, STEP_NEVER)).err();
        let why = why.expect("the worker must not start");
        assert!(why.contains(text), "{text}: {why}");
    }
    let machine = FakeMachine::new();
    machine.world.fail("list", 1);
    let why = js_agent::Worker::start(plan(&dir, &machine, STEP_NEVER)).err();
    let why = why.expect("the worker must not start");
    assert!(why.contains("cannot list the containers"), "{why}");

    // The egress check at start: a worker over a network that is not limited as the flags
    // say does not start. It catches an agent that starts and refuses every job instead.
    let machine = FakeMachine::new();
    machine.world.fail("egress", 1);
    let why = js_agent::Worker::start(plan(&dir, &machine, STEP_NEVER)).err();
    let why = why.expect("the worker must not start");
    assert!(why.contains("egress failed"), "{why}");
}

/// Asserts that a worker started over an engine that holds a container of an earlier
/// process lists that run first, running, before any order: the hello of the new process can
/// carry it. It catches a restart that forgets a job it is still running.
#[test]
fn the_containers_of_an_earlier_process_are_held_at_start() {
    let dir = Scratch::new("jobs-worker-adopt");
    let machine = FakeMachine::new();
    let first = start(&dir, &machine);
    assert_eq!(first.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(first.handle().settle());

    let second = start(&dir, &FakeMachine::of(&machine.world));
    let held = second.snapshot().held;
    assert_eq!(held.len(), 1);
    assert_eq!(
        (held[0].run_id.as_str(), held[0].state),
        ("r1", HeldState::Running)
    );
    let lines = second.take_lines();
    assert!(
        lines.iter().any(|line| line.contains("Adopted")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("the fake engine is ready")),
        "the runtime's own words are kept: {lines:?}"
    );
}

/// Asserts that a worker steps by itself when it gets no order: with a step interval of a
/// millisecond a container that exits is found, its result published, with nothing sent to
/// the worker. It catches a worker that only moves when the server speaks, so a job that
/// ends while the server is quiet is never reported. The test waits for the event, with a
/// guard against a hang, and not for a time.
#[test]
fn a_worker_finds_an_exit_by_itself() {
    let dir = Scratch::new("jobs-worker-steps");
    let machine = FakeMachine::new();
    let worker = js_agent::Worker::start(plan(&dir, &machine, Duration::from_millis(1)))
        .expect("the worker starts");
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    let handle = worker.handle();
    assert!(handle.wait_until(&|s| !s.held.is_empty(), HANG_GUARD));
    assert!(!handle.wait_until(&|s| !s.results.is_empty(), Duration::from_millis(1)));
    machine.world.exit("r1", 3);
    assert!(handle.wait_until(&|s| !s.results.is_empty(), HANG_GUARD));
    let result = &worker.snapshot().results[0];
    assert_eq!(
        (result.outcome, result.exit_code),
        (Outcome::Failed, Some(3))
    );
}

/// Asserts that a closed worker takes no start but still obeys a stop: the start of a
/// second run is not acted on (and a line says so), the first run is stopped and reported
/// killed. It catches a drain that goes on taking work, and one that stops obeying.
#[test]
fn a_closed_worker_takes_no_start_and_still_stops() {
    let dir = Scratch::new("jobs-worker-closed");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());

    worker.close();
    let mut both = starts(&["r2"]);
    both.commands.push(Command::Stop {
        run_id: "r1".to_owned(),
    });
    assert_eq!(worker.try_deliver(both), Delivery::Delivered);
    assert!(worker.handle().settle());

    assert_eq!(machine.world.created(), ["r1"]);
    let results = worker.snapshot().results;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, Outcome::Killed);
    let lines = worker.take_lines();
    assert!(
        lines.contains(&"draining: the start of r2 is not taken".to_owned()),
        "{lines:?}"
    );
}

/// Asserts that `stop_all` stops every run the worker holds, each reported killed, and
/// stops them once. It catches a drain that leaves a job running past its wait.
#[test]
fn stop_all_stops_every_run() {
    let dir = Scratch::new("jobs-worker-stop-all");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert_eq!(
        worker.try_deliver(starts(&["r1", "r2"])),
        Delivery::Delivered
    );
    assert!(worker.handle().settle());

    worker.stop_all();
    assert!(worker.handle().settle());
    let log = machine.world.log();
    assert!(log.contains(&"stop r1".to_owned()) && log.contains(&"stop r2".to_owned()));
    let outcomes: Vec<Outcome> = worker
        .snapshot()
        .results
        .iter()
        .map(|r| r.outcome)
        .collect();
    assert_eq!(outcomes, [Outcome::Killed, Outcome::Killed]);
    assert!(worker.handle().settle());
    let stops = machine
        .world
        .log()
        .iter()
        .filter(|c| c.starts_with("stop"))
        .count();
    assert_eq!(stops, 2, "one stop each: {:?}", machine.world.log());
}

/// Asserts that a command the lifecycle refuses is logged by its place in the reply, and
/// the commands beside it still count: a stop at 0 is acted on and a start with no run id
/// at 1 is refused with its index. It catches a reply thrown away for one bad command, and a
/// refusal nobody can find in the log.
#[test]
fn a_refused_command_is_logged_by_its_place() {
    let dir = Scratch::new("jobs-worker-invalid");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    let mut bad = job("r2");
    bad.run_id = String::new();
    let reply = HeartbeatReply {
        commands: vec![
            Command::Stop {
                run_id: "r1".to_owned(),
            },
            Command::Start(Box::new(bad)),
        ],
        ..HeartbeatReply::default()
    };
    assert_eq!(worker.try_deliver(reply), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert_eq!(worker.snapshot().results[0].outcome, Outcome::Killed);
    let lines = worker.take_lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Invalid { index: 1,")),
        "{lines:?}"
    );
}

/// Asserts that a worker keeps at most [`LINES_CAP`] lines for the session to print and
/// counts the rest: three hundred refused starts in one reply leave the cap's worth and one
/// line saying how many were dropped, and the counter starts again. It catches a log that
/// grows for ever while the session is busy, and one that drops lines without saying.
#[test]
fn lines_over_the_cap_are_counted() {
    let dir = Scratch::new("jobs-worker-lines");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let _ = worker.take_lines();
    let mut many = HeartbeatReply::default();
    for n in 0..LINES_CAP + 44 {
        let mut huge = job(&format!("r{n}"));
        huge.limits.cpu_millis = 9000;
        many.commands.push(Command::Start(Box::new(huge)));
    }
    assert_eq!(worker.try_deliver(many), Delivery::Delivered);
    assert!(worker.handle().settle());
    let lines = worker.take_lines();
    assert_eq!(lines.len(), LINES_CAP + 1);
    assert_eq!(lines[0], "44 log line(s) were dropped");
    assert!(worker.take_lines().is_empty());
}

/// Asserts that a worker whose runtime panics is seen to have ended, by the handle and by
/// the worker, and that a settle on it returns false and does not hang. It catches an agent
/// that goes on listing jobs nobody supervises.
#[test]
fn a_worker_that_panics_is_seen_to_have_ended() {
    let dir = Scratch::new("jobs-worker-panic");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let handle = worker.handle();
    assert!(!worker.ended() && !handle.ended());
    machine.world.panic_at("create");
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(!handle.settle(), "the worker is gone");
    assert!(worker.ended() && handle.ended());
    assert_eq!(
        worker.try_deliver(stops("r1")),
        Delivery::Gone,
        "nobody takes a reply"
    );
}

/// Asserts that the snapshot lists the running leases for the load reader: a started run
/// appears with the lease it belongs to, and is gone once it has exited. It catches a
/// worker that publishes no leases (the heartbeat would carry no per-lease cost) and one
/// that keeps listing a finished run.
#[test]
fn the_snapshot_lists_the_running_leases_and_drops_a_finished_one() {
    let dir = Scratch::new("jobs-worker-leases");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert!(worker.snapshot().leases.is_empty());
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    let leases = worker.snapshot().leases;
    assert_eq!(leases.len(), 1);
    assert!(leases[0].started_usec.is_some());
    assert!(!leases[0].lease_id.is_empty());
    machine.world.exit("r1", 0);
    assert!(worker.handle().settle());
    assert!(worker.snapshot().leases.is_empty());
}
