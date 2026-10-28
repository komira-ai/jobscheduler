//! The loop around a session that carries jobs: the drain after a stop request, and the
//! whole agent over a machine of the test's own. The worker is a real thread; the sleeper
//! of these tests settles it before and after each action, so what the next tick sees does
//! not depend on how fast the thread is.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use js_agent::{Drain, Exit, Handle, Session, Sleeper, Wake, Worker, drive};
use serde_json::{Value, json};

use super::fake::{Build, FakeMachine};
use super::{private, start, start_answer, starts, user};
use crate::support::{
    CountingSleeper, Jobs, Lowest, Reply, Scratch, Script, ScriptConnector, beat_ok, code, flags,
    hello, linux_host, lowest, refused, reply, run_on, welcome,
};

/// What the sleeper does at its `n`th wait (counting from 0): the wake it reports.
type Act<'a> = Box<dyn FnMut(usize) -> Wake + 'a>;

/// A sleeper that settles the worker around each action.
struct Settling<'a> {
    handle: Handle,
    calls: usize,
    act: Act<'a>,
}

impl Sleeper for Settling<'_> {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        self.handle.settle();
        let n = self.calls;
        self.calls += 1;
        let wake = (self.act)(n);
        self.handle.settle();
        wake
    }
}

fn settling<'a>(worker: &Worker, act: impl FnMut(usize) -> Wake + 'a) -> Settling<'a> {
    Settling {
        handle: worker.handle(),
        calls: 0,
        act: Box::new(act),
    }
}

/// The reply that acknowledges the result of `run`.
fn ack_of(run: &str) -> Reply {
    reply(
        200,
        json!({ "commands": [], "result_acks": [{ "run_id": run, "ack": "ACCEPTED" }] }),
    )
}

/// Drives a session over `replies` with the sleeper `act`; returns how it ended, what it
/// logged and what it sent.
fn drained(
    worker: &Worker,
    drain: Drain,
    replies: Vec<Reply>,
    act: impl FnMut(usize) -> Wake,
) -> (Exit, String, Vec<(String, Value)>) {
    let (mut script, sent) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(worker, drain);
    let mut sleeper = settling(worker, act);
    let mut log = Vec::new();
    let exit = drive(&mut session, &mut sleeper, &mut log);
    let sent = sent.borrow().clone();
    (exit, String::from_utf8(log).unwrap(), sent)
}

const LONG: Drain = Drain {
    wait: Duration::from_secs(3600),
    settle: Duration::from_secs(30),
};

fn phases(sent: &[(String, Value)]) -> Vec<String> {
    let phase = |(_, body): &(String, Value)| body["phase"].as_str().unwrap().to_owned();
    sent.iter().map(phase).collect()
}

/// Asserts the drain of an agent whose job ends while it drains: after the stop request
/// every heartbeat says `draining` and lists the run; the start the server sends meanwhile
/// is not taken; the job exits, its result rides the next heartbeat and is acknowledged;
/// and the agent ends, with a heartbeat that lists nothing, without having stopped the job.
/// It catches an agent that leaves at the signal with a job running and a result unsent,
/// and one that takes work while it drains.
#[test]
fn a_job_that_ends_during_the_drain_is_reported_before_the_agent_leaves() {
    let dir = Scratch::new("jobs-drain-finish");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert!(worker.try_deliver(starts(&["r1"])));
    assert!(worker.handle().settle());
    let replies = vec![
        welcome(1),
        reply(200, start_answer(&["r2"])),
        ack_of("r1"),
        beat_ok(),
    ];
    let world = machine.world.clone();
    let act = move |n: usize| match n {
        0 => Wake::Stop,
        1 => {
            world.exit("r1", 0);
            Wake::Elapsed
        }
        _ => Wake::Elapsed,
    };
    let (exit, log, sent) = drained(&worker, LONG, replies, act);

    assert_eq!(exit, Exit::Stopped);
    assert_eq!(
        phases(&sent),
        ["serving", "draining", "draining", "draining"]
    );
    assert_eq!(sent[1].1["held"][0]["state"], "running");
    assert_eq!(sent[2].1["results"][0]["run_id"], "r1");
    assert_eq!(sent[2].1["results"][0]["outcome"], "succeeded");
    assert_eq!(sent[3].1["held"], json!([]));
    assert_eq!(machine.world.created(), ["r1"], "r2 is not taken");
    assert!(
        !machine.world.log().contains(&"stop r1".to_owned()),
        "the job ended on its own"
    );
    assert!(
        log.contains("draining: the start of r2 is not taken"),
        "{log}"
    );
    assert!(!log.contains("a run or a result is left"), "{log}");
}

/// Asserts that a job still running when the wait is over is stopped, its result (killed) is
/// sent and acknowledged, and the agent then ends: the first two heartbeats of the drain
/// list the run running and the third, after the stop, carries the result. It catches a
/// drain that waits for ever for a job that does not end, and one that stops a job before
/// its wait is over.
#[test]
fn a_job_still_running_after_the_wait_is_stopped_and_reported() {
    let dir = Scratch::new("jobs-drain-stop");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert!(worker.try_deliver(starts(&["r1"])));
    assert!(worker.handle().settle());
    let drain = Drain {
        wait: Duration::from_secs(10),
        settle: Duration::from_secs(30),
    };
    let replies = vec![welcome(1), beat_ok(), beat_ok(), ack_of("r1"), beat_ok()];
    let log_of = machine.world.clone();
    let before_stop = RefCell::new(Vec::new());
    let act = |n: usize| {
        before_stop
            .borrow_mut()
            .push(log_of.log().contains(&"stop r1".to_owned()));
        if n == 0 { Wake::Stop } else { Wake::Elapsed }
    };
    let (exit, _, sent) = drained(&worker, drain, replies, act);

    assert_eq!(exit, Exit::Stopped);
    assert_eq!(sent[1].1["held"][0]["state"], "running");
    assert_eq!(sent[3].1["results"][0]["outcome"], "killed");
    assert_eq!(sent[4].1["held"], json!([]));
    assert_eq!(
        *before_stop.borrow(),
        [false, false, true, true],
        "stopped at the third heartbeat of the drain, when ten seconds had passed"
    );
}

/// Asserts that a result the server never acknowledges is not dropped when the drain's time
/// is over: the agent ends, says that something is left, and the worker still holds the run
/// and its result for the next process. It catches an agent that discards what it could not
/// report.
#[test]
fn a_result_nobody_acknowledges_is_kept_when_the_drain_gives_up() {
    let dir = Scratch::new("jobs-drain-giveup");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert!(worker.try_deliver(starts(&["r1"])));
    assert!(worker.handle().settle());
    let drain = Drain {
        wait: Duration::from_secs(5),
        settle: Duration::from_secs(5),
    };
    let replies = vec![welcome(1), beat_ok(), beat_ok(), beat_ok()];
    let act = |n: usize| if n == 0 { Wake::Stop } else { Wake::Elapsed };
    let (exit, log, _) = drained(&worker, drain, replies, act);

    assert_eq!(exit, Exit::Stopped);
    assert!(log.contains("draining: a run or a result is left"), "{log}");
    let snapshot = worker.snapshot();
    assert_eq!(snapshot.results.len(), 1);
    assert_eq!(snapshot.held.len(), 1);
}

/// Asserts that a second stop request ends the drain at once: the job is untouched, still
/// running, and the agent says it left something. It catches an agent that cannot be made
/// to leave.
#[test]
fn a_second_stop_request_ends_the_drain_at_once() {
    let dir = Scratch::new("jobs-drain-twice");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert!(worker.try_deliver(starts(&["r1"])));
    assert!(worker.handle().settle());
    let replies = vec![welcome(1), beat_ok()];
    let (exit, log, sent) = drained(&worker, LONG, replies, |_| Wake::Stop);

    assert_eq!(exit, Exit::Stopped);
    assert_eq!(sent.len(), 2);
    assert_eq!(machine.world.alive(), 1);
    assert!(log.contains("draining: a run or a result is left"), "{log}");
}

/// Asserts that an agent that holds nothing drains with one heartbeat of `draining`, and
/// says nothing is left. It catches a drain that waits for a job that is not there.
#[test]
fn an_agent_with_no_jobs_drains_with_one_heartbeat() {
    let dir = Scratch::new("jobs-drain-idle");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let replies = vec![welcome(1), beat_ok()];
    let (exit, log, sent) = drained(&worker, LONG, replies, |_| Wake::Stop);

    assert_eq!(exit, Exit::Stopped);
    assert_eq!(phases(&sent), ["serving", "draining"]);
    assert!(!log.contains("is left"), "{log}");
}

/// Asserts that a server that refuses the agent for good during the drain ends it with that
/// refusal, and that a worker that ends during the drain, or during the ordinary loop, ends
/// it as failed. It catches a drain that goes on against a server that will not have the
/// host, and one that waits for the jobs of a dead worker.
#[test]
fn a_refusal_or_a_dead_worker_ends_the_loop() {
    let dir = Scratch::new("jobs-drain-ends");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert!(worker.try_deliver(starts(&["r1"])));
    assert!(worker.handle().settle());
    let replies = vec![welcome(1), refused(401, "unauthorised")];
    let (exit, _, _) = drained(&worker, LONG, replies, |_| Wake::Stop);
    let Exit::Fatal(fatal) = exit else {
        panic!("the server refused the agent: {exit:?}");
    };
    assert_eq!(fatal.status, 401);

    let dir = Scratch::new("jobs-drain-dies");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert!(worker.try_deliver(starts(&["r1"])));
    assert!(worker.handle().settle());
    let world = machine.world.clone();
    let none = Drain {
        wait: Duration::ZERO,
        settle: Duration::from_secs(30),
    };
    let replies = vec![welcome(1), beat_ok(), beat_ok()];
    let (exit, _, _) = drained(&worker, none, replies, |n| {
        if n == 0 {
            world.panic_at("stop");
            Wake::Stop
        } else {
            Wake::Elapsed
        }
    });
    assert!(matches!(exit, Exit::Failed(_)), "{exit:?}");

    let dir = Scratch::new("jobs-loop-dies");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let world = machine.world.clone();
    let replies = vec![welcome(1), beat_ok(), beat_ok()];
    let (exit, _, _) = drained(&worker, LONG, replies, |_| {
        world.panic_at("create");
        assert!(worker.try_deliver(starts(&["r1"])));
        Wake::Elapsed
    });
    assert!(matches!(exit, Exit::Failed(_)), "{exit:?}");
}

/// The flags that start an agent with containers over `dir`.
pub fn container_flags(dir: &Scratch) -> Vec<String> {
    let exec = dir.write("docker", "#!/bin/sh\n", 0o755);
    let (user, _, _) = user();
    vec![
        "--container-bin".to_owned(),
        exec.display().to_string(),
        "--container-user".to_owned(),
        user,
        "--state-root".to_owned(),
        private(dir, "state").display().to_string(),
        "--scratch-root".to_owned(),
        private(dir, "run-scratch").display().to_string(),
    ]
}

/// Asserts that the whole agent, given a machine, starts its worker before it says hello
/// and hands the worker to the hook; that an engine it cannot use ends it with status 2 and
/// the reason before it registers; and that a worker that dies ends it with status 1 and
/// says so. It catches a start that registers before the jobs are ready, and an agent that
/// goes on after its worker died.
#[test]
fn the_whole_agent_starts_its_worker_and_ends_when_it_dies() {
    let dir = Scratch::new("jobs-agent");
    let owned = container_flags(&dir);
    let extra: Vec<&str> = owned.iter().map(String::as_str).collect();
    let args_with = flags(&extra);
    let host = linux_host();

    let broken = Arc::new(FakeMachine::failing(Build::RuntimeFails));
    let (script, sent) = Script::new(vec![welcome(1)]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let ran = run_on(
        &args_with,
        &host,
        &connector,
        &mut sleeper,
        &mut Lowest,
        (broken, None),
    );
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err.contains("--container-bin:") && ran.err.contains("the engine does not answer"),
        "{}",
        ran.err
    );
    assert!(
        sent.borrow().is_empty(),
        "nothing is said before the jobs are ready"
    );

    let machine = Arc::new(FakeMachine::new());
    let (script, _) = Script::new(vec![welcome(1), reply(200, start_answer(&["r1"]))]);
    let connector = ScriptConnector::of(script);
    let seen: RefCell<Option<Handle>> = RefCell::new(None);
    let hook = |handle: Handle| *seen.borrow_mut() = Some(handle);
    let world = machine.world.clone();
    let mut sleeper = Settling2 {
        seen: &seen,
        world: &world,
        calls: 0,
    };
    let jobs: Jobs<'_> = (machine, Some(&hook));
    let ran = run_on(
        &args_with,
        &host,
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
    );
    assert_eq!(ran.status(), code(1), "{}", ran.err);
    assert!(ran.err.contains("job worker stopped"), "{}", ran.err);
    assert!(ran.err.contains("the fake engine is ready"), "{}", ran.err);
}

/// A sleeper that makes the worker's runtime panic when the first start arrives, and
/// waits until the worker is gone.
struct Settling2<'a> {
    seen: &'a RefCell<Option<Handle>>,
    world: &'a super::fake::World,
    calls: usize,
}

impl Sleeper for Settling2<'_> {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        let handle = self.seen.borrow().clone().expect("the hook was told");
        self.calls += 1;
        if self.calls == 1 {
            self.world.panic_at("create");
        }
        handle.settle();
        Wake::Elapsed
    }
}
