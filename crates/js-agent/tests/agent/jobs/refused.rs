//! What the answers of a server do to the jobs a host runs: an answer no operator has to act
//! on (404, 400 `invalid-heartbeat`) keeps the session and the jobs; a refusal only the
//! operator can resolve ends the agent with its status and leaves every job running for the
//! next process of the agent, which adopts it. The session is given the state root to hand
//! over in, as every agent that runs jobs is. The worker is a real thread that the sleeper
//! settles; nothing waits for a time.

use std::path::PathBuf;
use std::time::Duration;

use js_agent::{Delivery, Drain, Exit, Handle, Session, Sleeper, Wake, Worker, drive};
use js_agent_wire::HeldState;
use serde_json::Value;

use super::fake::FakeMachine;
use super::{start, starts};
use crate::bounds::{Capped, WakeBudget};
use crate::lifecycle::job;
use crate::support::{Reply, Scratch, Script, beat_ok, hello, lowest, refused, reply, welcome};

/// What the sleeper does at its `n`th wait (counting from 0): the wake it reports.
type Act<'a> = Box<dyn FnMut(usize) -> Wake + 'a>;

/// A sleeper that settles the worker around each action.
struct Settling<'a> {
    handle: Handle,
    calls: usize,
    act: Act<'a>,
    budget: WakeBudget,
}

impl Sleeper for Settling<'_> {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        self.budget.spend();
        self.handle.settle();
        let n = self.calls;
        self.calls += 1;
        let wake = (self.act)(n);
        self.handle.settle();
        wake
    }
}

const LONG: Drain = Drain {
    wait: Duration::from_secs(3600),
    settle: Duration::from_secs(30),
};

/// The state root the jobs of `dir` keep their records in (see `super::plan`).
fn state(dir: &Scratch) -> PathBuf {
    dir.path("state")
}

/// A worker over `machine` running `runs`.
fn running(dir: &Scratch, machine: &FakeMachine, runs: &[&str]) -> Worker {
    let worker = start(dir, machine);
    assert_eq!(worker.try_deliver(starts(runs)), Delivery::Delivered);
    assert!(worker.handle().settle());
    worker
}

/// Drives a session of `worker` over `replies`, handing over in the state root of `dir`,
/// with a sleeper that settles the worker around `act`; returns how it ended, what it logged
/// and what it sent.
fn driven(
    dir: &Scratch,
    worker: &Worker,
    replies: Vec<Reply>,
    drain: Drain,
    act: impl FnMut(usize) -> Wake,
) -> (Exit, String, Vec<(String, Value)>) {
    let (mut script, sent) = Script::new(replies);
    let root = state(dir);
    let mut session = Session::new(hello("h1"), &mut script, lowest())
        .with_jobs(worker, drain)
        .with_handover(Some(&root));
    let mut sleeper = Settling {
        handle: worker.handle(),
        calls: 0,
        act: Box::new(act),
        budget: WakeBudget::new(),
    };
    let mut log = Capped::new();
    let exit = drive(&mut session, &mut sleeper, &mut log);
    let sent = sent.borrow().clone();
    (exit, log.text(), sent)
}

/// The stops the runtime was asked for.
fn stops(machine: &FakeMachine) -> Vec<String> {
    let log = machine.world.log();
    log.into_iter()
        .filter(|call| call.starts_with("stop "))
        .collect()
}

/// The next process of the agent: the first worker is gone (its thread parked, its queue
/// dropped) and a second one is started over the same world and state root, which adopts.
fn next_process(dir: &Scratch, machine: &FakeMachine, first: Worker) -> Worker {
    drop(first);
    start(dir, machine)
}

/// Asserts that a registered session with a running job, answered 404 and then 400
/// `invalid-heartbeat` (as a server rolled back or misrouted answers for a while), keeps the
/// job running and the session: through both answers and after them the container is alive
/// and nothing is stopped, every heartbeat names the welcome's epoch and lists the job
/// running, no second hello is sent, and the heartbeat after them is accepted. Catches an
/// agent that ends on such an answer and stops every job of the host with it.
#[test]
fn a_404_then_an_invalid_heartbeat_leaves_the_running_job_running() {
    let dir = Scratch::new("jobs-refused-passing");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1"]);
    let replies = vec![
        welcome(1),
        refused(404, "not-found"),
        refused(400, "invalid-heartbeat"),
        beat_ok(),
        beat_ok(),
    ];
    let world = machine.world.clone();
    let mut alive = Vec::new();
    let (exit, log, sent) = driven(&dir, &worker, replies, LONG, |n| {
        alive.push(world.alive());
        match n {
            3 | 4 => Wake::Stop,
            _ => Wake::Elapsed,
        }
    });

    assert_eq!(exit, Exit::Stopped, "{log}");
    assert_eq!(alive, [1, 1, 1, 1, 1], "{log}");
    assert_eq!(machine.world.alive(), 1);
    assert!(stops(&machine).is_empty(), "{:?}", machine.world.log());
    let paths: Vec<&str> = sent.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(
        paths,
        [
            js_agent::HELLO_PATH,
            js_agent::HEARTBEAT_PATH,
            js_agent::HEARTBEAT_PATH,
            js_agent::HEARTBEAT_PATH,
            js_agent::HEARTBEAT_PATH,
        ]
    );
    for (_, beat) in &sent[1..] {
        assert_eq!(beat["epoch"], 1, "{beat}");
        assert_eq!(beat["held"][0]["run_id"], "r1", "{beat}");
        assert_eq!(beat["held"][0]["state"], "running", "{beat}");
    }
    assert!(
        log.contains("js-agent: heartbeat failed: status 404 (not-found); next in 500ms\n"),
        "{log}"
    );
    assert!(
        log.contains(
            "js-agent: heartbeat failed: status 400 (invalid-heartbeat); next in 1000ms\n"
        ),
        "{log}"
    );
    assert!(!log.contains("not retrying"), "{log}");
}

/// Asserts that a refusal only the operator can resolve, to a heartbeat (401) and to the
/// hello of a process that already runs a job (403 `unknown-pool`), ends the agent with that
/// refusal and leaves the job running: nothing is stopped, the log says the runs are handed
/// over, and the next process of the agent adopts the job as running, and reports its own
/// result, once, when it ends. Catches an agent that stops every job of the host when its
/// token is revoked or its pool renamed, and a hand-over the next process cannot adopt.
#[test]
fn a_refusal_for_good_leaves_the_job_running_for_the_next_process_to_adopt() {
    for (name, replies, status) in [
        (
            "jobs-refused-beat",
            vec![welcome(1), beat_ok(), refused(401, "unauthorised")],
            401,
        ),
        (
            "jobs-refused-hello",
            vec![refused(403, "unknown-pool")],
            403,
        ),
    ] {
        let dir = Scratch::new(name);
        let machine = FakeMachine::new();
        let worker = running(&dir, &machine, &["r1"]);
        let (exit, log, sent) = driven(&dir, &worker, replies, LONG, |_| Wake::Elapsed);

        let Exit::Fatal(fatal) = exit else {
            panic!("{name}: the server refused the agent for good: {exit:?}");
        };
        assert_eq!(fatal.status, status, "{name}");
        assert_eq!(machine.world.alive(), 1, "{name}");
        assert!(
            stops(&machine).is_empty(),
            "{name}: {:?}",
            machine.world.log()
        );
        assert!(
            log.contains(
                "js-agent: the server refused the agent for good: no new job is taken, and the \
                 runs are left running to the next process once no start, stop or check is \
                 under way"
            ),
            "{name}: {log}"
        );
        assert!(
            log.contains("js-agent: handed over: every run is left as it is to the next process"),
            "{name}: {log}"
        );
        assert_eq!(sent[0].1["held"][0]["run_id"], "r1", "{name}");

        let next = next_process(&dir, &machine, worker);
        let held = next.snapshot().held;
        let states: Vec<(&str, HeldState)> = held
            .iter()
            .map(|run| (run.run_id.as_str(), run.state))
            .collect();
        assert_eq!(states, [("r1", HeldState::Running)], "{name}");
        machine.world.exit("r1", 4);
        assert!(next.handle().settle());
        let results = next.snapshot().results;
        let ends: Vec<(&str, Option<i32>)> = results
            .iter()
            .map(|result| (result.run_id.as_str(), result.exit_code))
            .collect();
        assert_eq!(ends, [("r1", Some(4))], "{name}");
        assert_eq!(
            machine.world.created(),
            ["r1"],
            "{name}: nothing started twice"
        );
    }
}

/// Asserts the end of the hand-over after a refusal for good while a start is under way (its
/// image pull never ends, so the worker never parks): with a drain wait of two asks the agent
/// asks, waits, asks again and then leaves with the refusal, saying that what was under way
/// is left; a stop request during it ends it the same way at the first wait. Either way
/// nothing is stopped and the running job is still alive. Catches a refusal that waits for
/// ever on a worker that cannot park, one that ignores a stop request, and one that falls
/// back to stopping the jobs.
#[test]
fn a_refusal_while_a_start_is_under_way_leaves_at_the_drain_wait_or_a_stop() {
    for (name, drain, wake, waits) in [
        (
            "jobs-refused-bound",
            Drain {
                wait: Duration::from_secs(10),
                settle: Duration::from_secs(30),
            },
            Wake::Elapsed,
            2,
        ),
        ("jobs-refused-stopped", LONG, Wake::Stop, 2),
    ] {
        let dir = Scratch::new(name);
        let machine = FakeMachine::new();
        let pulled = job("r2").image;
        let worker = running(&dir, &machine, &["r1"]);
        machine.world.begin_pull(&pulled);
        assert_eq!(worker.try_deliver(starts(&["r2"])), Delivery::Delivered);
        assert!(worker.handle().settle());
        let replies = vec![welcome(1), refused(401, "unauthorised")];
        let mut calls = 0;
        let (exit, log, _) = driven(&dir, &worker, replies, drain, |n| {
            calls += 1;
            match n {
                0 => Wake::Elapsed,
                _ => wake,
            }
        });

        assert!(matches!(exit, Exit::Fatal(_)), "{name}: {exit:?}");
        assert_eq!(
            calls, waits,
            "{name}: the waits of the loop and the hand-over"
        );
        assert!(
            log.contains(
                "js-agent: handing over: a start, a stop or a check is still under way and is \
                 left to the next process"
            ),
            "{name}: {log}"
        );
        assert!(!log.contains("js-agent: handed over"), "{name}: {log}");
        assert_eq!(machine.world.alive(), 1, "{name}");
        assert!(
            stops(&machine).is_empty(),
            "{name}: {:?}",
            machine.world.log()
        );
    }
}

/// Asserts that the answers which leave the jobs alone are not taken for a refusal by a
/// server that answers with a body the agent cannot read: a 404 with no body, then a 413
/// `body-too-large`, then a 403 with a code the agent does not know, each keeps the job
/// running and is followed by a heartbeat of the same session. Catches an agent that reads
/// any 4xx it does not know as the end.
#[test]
fn unknown_and_unreadable_4xx_leave_the_running_job_running() {
    let dir = Scratch::new("jobs-refused-unknown");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1"]);
    let replies = vec![
        welcome(1),
        reply(404, Value::Null),
        refused(413, "body-too-large"),
        refused(403, "forbidden"),
        beat_ok(),
        beat_ok(),
    ];
    let (exit, log, sent) = driven(&dir, &worker, replies, LONG, |n| match n {
        4 | 5 => Wake::Stop,
        _ => Wake::Elapsed,
    });
    assert_eq!(exit, Exit::Stopped, "{log}");
    assert_eq!(machine.world.alive(), 1);
    assert!(stops(&machine).is_empty(), "{:?}", machine.world.log());
    let hellos = sent
        .iter()
        .filter(|(path, _)| path == js_agent::HELLO_PATH)
        .count();
    assert_eq!(hellos, 1, "{sent:?}");
    assert_eq!(sent.len(), 6, "{sent:?}");
}

/// Asserts that a process refused at its first hello, holding a run past its wall limit (as a
/// process that adopted it after an earlier refusal does), stops that run before it hands
/// over: the worker takes a step before it answers the park, the run's container is given its
/// stop, once, and the agent still ends with the refusal. Catches a refusal that lasts leaving a run
/// running past its wall limit for ever, because each restarted process parks before its
/// worker has stepped once.
#[test]
fn a_run_past_its_wall_limit_is_stopped_before_a_refusal_hands_over() {
    let dir = Scratch::new("jobs-refused-overdue");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1"]);
    // `job` gives every run a minute.
    machine.clock.advance(61_000);
    let replies = vec![refused(403, "unknown-pool")];
    let (exit, log, _) = driven(&dir, &worker, replies, LONG, |_| Wake::Elapsed);

    let Exit::Fatal(fatal) = exit else {
        panic!("the server refused the agent for good: {exit:?}");
    };
    assert_eq!(fatal.status, 403);
    assert_eq!(stops(&machine), ["stop r1"], "{log}");
}
