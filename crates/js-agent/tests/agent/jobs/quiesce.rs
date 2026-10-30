//! A whole-node lease through the real job worker: the session asks the worker (never
//! blocking) whether the host is quiet and whether it is clean again, the worker answers on
//! its own thread, and the readiness hook runs there. The runtime is in memory, the clock
//! is the test's and does not move, and every wait on the worker is for an event (an order
//! done), never for a time.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};
use std::time::Duration;

use js_agent::lifecycle::{Quiet, Restored};
use js_agent::{
    Delivery, Drain, EXIT_NOT_QUIET, HEARTBEAT_PATH, HookRunner, Note, QUEUE, Quiesce, Ran,
    ReadinessHook, Session, Worker, WorkerHook, WorkerHost, stall_bound,
};
use js_agent_wire::{Command, HeartbeatReply, JobSpec, RefusalReason};
use serde_json::{Value, json};

use super::fake::{FakeMachine, HANG_GUARD};
use super::{STEP_NEVER, plan, start, starts};
use crate::exclusive::{lines, note};
use crate::lifecycle::job;
use crate::support::{Reply, Scratch, Script, Sent, hello, lowest, reply, welcome};

/// How long a test waits for a question that must not block. It is far below
/// [`HANG_GUARD`], which the fixture's own held call waits for: a blocking proxy fails on
/// this guard, never on the fixture giving up first and letting the proxy through.
const ASK_GUARD: Duration = Duration::from_secs(10);

const LEASE: &str = "whole-1";
const OTHER: &str = "whole-2";

/// The drain the sessions of these tests carry; none of them drains.
const DRAIN: Drain = Drain {
    wait: Duration::from_secs(10),
    settle: Duration::from_secs(30),
};

/// A reply body that names `lease` (or none), carries `commands` and acknowledges the
/// results of `acks`.
fn body(lease: Option<&str>, commands: &[Command], acks: &[&str]) -> Value {
    let acks: Vec<Value> = acks
        .iter()
        .map(|run| json!({ "run_id": run, "ack": "ACCEPTED" }))
        .collect();
    json!({ "commands": commands, "result_acks": acks, "exclusive": lease })
}

/// The heartbeat answer with that body.
fn beat(lease: Option<&str>, commands: &[Command], acks: &[&str]) -> Reply {
    reply(200, body(lease, commands, acks))
}

/// The same, as the reply the worker is handed.
fn granting(lease: &str, acks: &[&str]) -> HeartbeatReply {
    serde_json::from_value(body(Some(lease), &[], acks)).unwrap()
}

/// A start for the run `run_id` that belongs to the whole-node lease [`LEASE`].
fn of_lease(run_id: &str) -> Command {
    let spec = JobSpec {
        lease_id: LEASE.to_owned(),
        ..job(run_id)
    };
    Command::Start(Box::new(spec))
}

/// A start for the run `run_id`, which belongs to a lease of its own.
fn of_another(run_id: &str) -> Command {
    Command::Start(Box::new(job(run_id)))
}

/// The heartbeats sent so far.
fn heartbeats(sent: &Sent) -> Vec<Value> {
    let sent = sent.borrow();
    let beats = sent.iter().filter(|(path, _)| path == HEARTBEAT_PATH);
    beats.map(|(_, body)| body.clone()).collect()
}

/// A worker whose readiness hook is `runner`.
fn hooked(
    dir: &Scratch,
    machine: &FakeMachine,
    runner: impl HookRunner + Send + 'static,
) -> Worker {
    let mut made = plan(dir, machine, STEP_NEVER);
    let program = ReadinessHook::new("/bin/sh", &[]).unwrap();
    let bound = Duration::from_secs(1);
    made.readiness = Some(WorkerHook::new(program, Box::new(runner), bound));
    match Worker::start(made) {
        Ok(worker) => worker,
        Err(why) => panic!("the worker did not start: {why}"),
    }
}

/// The threads a hook ran on.
type Threads = Arc<Mutex<Vec<ThreadId>>>;

/// A hook runner that ends with `code` after printing `line`, and counts its runs and the
/// threads they ran on.
struct Counting {
    runs: Arc<AtomicUsize>,
    threads: Threads,
    code: i32,
    line: &'static str,
}

impl Counting {
    fn new(code: i32, line: &'static str) -> (Self, Arc<AtomicUsize>, Threads) {
        let runs = Arc::new(AtomicUsize::new(0));
        let threads = Arc::new(Mutex::new(Vec::new()));
        let runner = Self {
            runs: Arc::clone(&runs),
            threads: Arc::clone(&threads),
            code,
            line,
        };
        (runner, runs, threads)
    }
}

impl HookRunner for Counting {
    fn execute(&self, _program: &str, _args: &[&str], _env: &[(&str, &str)]) -> Ran {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.threads.lock().unwrap().push(thread::current().id());
        Ran::Exited {
            code: Some(self.code),
            text: format!("exit status: {}", self.code),
            stdout: format!("{}\n", self.line),
        }
    }
}

/// Asserts the whole lease through the real worker, session and lifecycle. A run the
/// scheduler did not know of is running when the lease is granted. Then: the first beat after
/// the grant says `preparing`, and no beat says ready while that run is held (running, or
/// exited with its result owed); once its result is acknowledged and the worker found the
/// host quiet, the next beat says `serving` with `exclusive_ready`, and the one after
/// repeats it; a start for the lease then runs and a start for another lease is refused
/// (`no-capacity`); the reply that stops naming the lease makes the beat after say
/// `restoring` until the worker has cleaned up, and then `serving` with nothing reported.
/// It catches a session that never asks the worker, a ready reported before the worker
/// answered or while a run is held, a gate that stays shut after readiness (or open to
/// another lease), and a restore that never ends or ends early.
#[test]
fn a_whole_node_lease_is_followed_through_the_real_worker() {
    let dir = Scratch::new("jobs-quiesce-sequence");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let handle = worker.handle();
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(handle.settle());

    let replies = vec![
        welcome(1),
        beat(Some(LEASE), &[], &[]),
        beat(Some(LEASE), &[], &[]),
        beat(Some(LEASE), &[], &[]),
        beat(Some(LEASE), &[], &["r1"]),
        beat(Some(LEASE), &[], &[]),
        beat(Some(LEASE), &[of_lease("r2"), of_another("r3")], &[]),
        beat(None, &[], &[]),
        beat(None, &[], &["r2", "r3"]),
        beat(None, &[], &[]),
        beat(None, &[], &[]),
    ];
    let (mut script, sent) = Script::new(replies);
    let mut host = WorkerHost::new(handle.clone());
    let clock = worker.clock();
    let mut session = Session::new(hello("h1"), &mut script, lowest())
        .with_jobs(&worker, DRAIN)
        .with_default_quiesce(&mut host, &clock);

    session.tick(); // hello
    for _ in 0..3 {
        session.tick(); // serving; preparing (announced); preparing (asked: r1 runs)
        assert!(handle.settle());
    }
    machine.world.exit("r1", 0);
    assert!(handle.settle());
    for _ in 0..3 {
        session.tick(); // asked: r1's result is owed; asked: acknowledged; ready
        assert!(handle.settle());
    }
    session.tick(); // ready resent; the reply stops naming the lease
    assert!(handle.settle());
    machine.world.exit("r2", 0);
    assert!(handle.settle());
    for _ in 0..3 {
        session.tick(); // restoring (announced); restoring (asked: busy); asked: done
        assert!(handle.settle());
    }

    assert_eq!(
        lines(&sent),
        [
            "serving",
            "preparing",
            "preparing",
            "preparing",
            "preparing",
            "serving ready=whole-1",
            "serving ready=whole-1",
            "restoring",
            "restoring",
            "serving",
        ]
    );
    assert_eq!(machine.world.created(), ["r1", "r2"], "r3 ran nothing");
    let beats = heartbeats(&sent);
    assert_eq!(
        beats[6]["results"],
        json!([{
            "run_id": "r3",
            "lease_id": "lease-r3",
            "outcome": "failed_to_start",
            "detail": "no-capacity",
        }])
    );
}

/// Asserts a tainted host is refused through the worker: a container the agent could not
/// remove has tainted the host, so the beat that gets the worker's answer carries
/// `exclusive_refused` with the code `host-unsafe` and a detail that says the host is
/// tainted, the phase stays `preparing`, the refusal is resent, and no beat says ready. It
/// catches a worker answer that is lost on its way to the heartbeat and a ready reported
/// for a host that is not safe to hand over.
#[test]
fn a_tainted_host_is_refused_through_the_worker() {
    let dir = Scratch::new("jobs-quiesce-taint");
    let machine = FakeMachine::new();
    machine.world.fail("remove", u32::MAX);
    let worker = start(&dir, &machine);
    let handle = worker.handle();
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(handle.settle());
    machine.world.exit("r1", 0);
    assert!(handle.settle());
    assert!(worker.snapshot().taint.is_some());

    let replies = std::iter::once(welcome(1))
        .chain((0..5).map(|_| beat(Some(LEASE), &[], &[])))
        .collect();
    let (mut script, sent) = Script::new(replies);
    let mut host = WorkerHost::new(handle.clone());
    let clock = worker.clock();
    let mut session = Session::new(hello("h1"), &mut script, lowest())
        .with_jobs(&worker, DRAIN)
        .with_default_quiesce(&mut host, &clock);
    session.tick(); // hello
    for _ in 0..5 {
        session.tick();
        assert!(handle.settle());
    }

    let beats = heartbeats(&sent);
    let phases: Vec<&str> = beats.iter().map(|b| b["phase"].as_str().unwrap()).collect();
    assert_eq!(
        phases,
        [
            "serving",
            "preparing",
            "preparing",
            "preparing",
            "preparing"
        ]
    );
    assert!(beats.iter().all(|b| b.get("exclusive_ready").is_none()));
    for refused in &beats[3..] {
        let refusal = &refused["exclusive_refused"];
        assert_eq!(refusal["lease_id"], LEASE, "{refused}");
        assert_eq!(refusal["code"], "host-unsafe", "{refused}");
        let detail = refusal["detail"].as_str().unwrap();
        assert!(detail.starts_with("this host is tainted"), "{detail}");
    }
    assert!(
        beats[..3]
            .iter()
            .all(|b| b.get("exclusive_refused").is_none())
    );
}

/// Asserts the host never hands back an answer that is about another lease: a ready the
/// worker published for one lease is busy when the other is asked, and the worker's answer
/// for the held lease is not taken for the lease that replaced it. It catches a proxy that
/// returns whatever was published last, which would report a new lease ready on the old
/// lease's verdict.
#[test]
fn the_host_never_returns_an_answer_for_another_lease() {
    let dir = Scratch::new("jobs-quiesce-tags");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());

    assert_eq!(
        worker.try_deliver(granting(LEASE, &[])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Busy, "nothing published yet");
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Ready);

    assert_eq!(
        host.prepare(OTHER),
        Quiet::Busy,
        "the ready on the page is for the other lease"
    );
    assert!(handle.settle());
    assert_eq!(
        host.prepare(OTHER),
        Quiet::Busy,
        "the worker follows only the first lease"
    );
    assert_eq!(host.prepare(LEASE), Quiet::Busy, "the worker asked afresh");
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Ready);

    assert_eq!(
        worker.try_deliver(granting(OTHER, &[])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Busy, "the lease was replaced");
    assert!(handle.settle());
    assert_eq!(host.prepare(OTHER), Quiet::Busy, "asked afresh");
    assert!(handle.settle());
    assert_eq!(host.prepare(OTHER), Quiet::Ready);
    assert_eq!(
        host.prepare(LEASE),
        Quiet::Busy,
        "the ready on the page is for the new lease"
    );
}

/// Asserts the same for the clean-up after a lease: the worker's `Done` for one lease is
/// busy when asked about another, and nothing is done while the worker still follows the
/// lease. It catches a restore that ends early on a reply the worker has not applied, and
/// one answered with another lease's `Done`.
#[test]
fn a_restored_answer_is_only_for_its_lease() {
    let dir = Scratch::new("jobs-quiesce-restore");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());

    assert_eq!(
        worker.try_deliver(granting(LEASE, &[])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    assert_eq!(host.restore(LEASE), Restored::Busy);
    assert!(handle.settle());
    assert_eq!(
        host.restore(LEASE),
        Restored::Busy,
        "the worker still follows the lease"
    );

    let ended = HeartbeatReply::default();
    assert_eq!(worker.try_deliver(ended), Delivery::Delivered);
    assert!(handle.settle());
    assert_eq!(host.restore(LEASE), Restored::Busy, "asked afresh");
    assert!(handle.settle());
    assert_eq!(host.restore(LEASE), Restored::Done);
    assert_eq!(host.restore(OTHER), Restored::Busy, "the done is for LEASE");
}

/// Asserts the readiness hook runs on the worker's thread and once for each lease: asked
/// to prepare the same lease over and over (three questions before the worker answers, and
/// more after), the hook is started once, on a thread other than the caller's, and its
/// verdict is what is returned every time. It catches a hook run from the heartbeat loop
/// and one re-run by every repeated question.
#[test]
fn the_hook_runs_once_per_lease_on_the_worker() {
    let dir = Scratch::new("jobs-quiesce-hook");
    let machine = FakeMachine::new();
    let (runner, runs, threads) = Counting::new(0, "");
    let worker = hooked(&dir, &machine, runner);
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());

    assert_eq!(
        worker.try_deliver(granting(LEASE, &[])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    assert_eq!(
        host.prepare(LEASE),
        Quiet::Busy,
        "no answer is published yet"
    );
    for _ in 0..2 {
        // The worker may or may not have answered by now; the hook runs once either way.
        let _ = host.prepare(LEASE);
    }
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Ready);
    assert_eq!(host.prepare(LEASE), Quiet::Ready);
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Ready);

    assert_eq!(runs.load(Ordering::SeqCst), 1, "one run for the lease");
    let threads = threads.lock().unwrap();
    assert_eq!(threads.len(), 1);
    assert_ne!(
        threads[0],
        thread::current().id(),
        "not the caller's thread"
    );
}

/// Asserts a refusing hook through the whole session: the hook (exit status
/// [`EXIT_NOT_QUIET`], a first line) runs once the worker found the host quiet, the beat
/// that gets the answer carries `exclusive_refused` with the hook's code and first line,
/// the session notes the refusal once, the refusal is resent without running the hook again,
/// and no beat says ready. It catches a hook verdict lost between the worker and the
/// heartbeat, a refusal that is reported once and forgotten, and a hook that runs again for
/// every resend.
#[test]
fn a_refusing_hook_refuses_the_lease_with_its_code() {
    let dir = Scratch::new("jobs-quiesce-hook-refuses");
    let machine = FakeMachine::new();
    let (runner, runs, _) = Counting::new(EXIT_NOT_QUIET, "noisy neighbour");
    let worker = hooked(&dir, &machine, runner);
    let handle = worker.handle();

    let replies = std::iter::once(welcome(1))
        .chain((0..5).map(|_| beat(Some(LEASE), &[], &[])))
        .collect();
    let (mut script, sent) = Script::new(replies);
    let mut host = WorkerHost::new(handle.clone());
    let clock = worker.clock();
    let mut session = Session::new(hello("h1"), &mut script, lowest())
        .with_jobs(&worker, DRAIN)
        .with_default_quiesce(&mut host, &clock);
    let mut steps = vec![session.tick()]; // hello
    for _ in 0..5 {
        steps.push(session.tick());
        assert!(handle.settle());
    }

    let refused = "preparing refused=not-quiet:noisy neighbour";
    assert_eq!(
        lines(&sent),
        ["serving", "preparing", "preparing", refused, refused]
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let notes: Vec<Option<Note>> = steps.iter().map(note).collect();
    let refusals = notes
        .iter()
        .filter(|n| matches!(n, Some(Note::Refused { .. })));
    assert_eq!(refusals.count(), 1);
    assert_eq!(
        notes[4],
        Some(Note::Refused {
            code: RefusalReason::NotQuiet
        })
    );
}

/// Asserts that a full worker queue makes the question busy at once instead of waiting: with
/// the worker inside a runtime call that does not return and [`QUEUE`] orders waiting, the
/// question comes back `Busy`, and once the call returns and the run has ended the same
/// question is answered. The asking runs on a thread the test waits for under a guard, so a
/// proxy that blocks fails the test instead of hanging it. It catches a proxy that blocks
/// the heartbeat loop on a busy worker, and one that gives up for good when its order was
/// dropped.
#[test]
fn a_full_queue_makes_the_question_busy_without_waiting() {
    let dir = Scratch::new("jobs-quiesce-full");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());

    machine.world.hold_creates();
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    machine.world.wait_entered(1);
    for n in 0..QUEUE {
        assert_eq!(
            worker.try_deliver(granting(LEASE, &[])),
            Delivery::Delivered,
            "reply {n} has room"
        );
    }
    let (answered, answers) = channel();
    let asking = thread::spawn(move || {
        let answer = (host.prepare(LEASE), host.restore(LEASE));
        let _ = answered.send(answer);
        host
    });
    let answer = answers
        .recv_timeout(ASK_GUARD)
        .expect("the questions blocked on a full queue");
    assert_eq!(
        answer,
        (Quiet::Busy, Restored::Busy),
        "the orders were dropped"
    );
    let mut host = asking.join().unwrap();

    machine.world.release();
    assert!(handle.settle());
    machine.world.exit("r1", 0);
    assert!(handle.settle());
    assert_eq!(
        worker.try_deliver(granting(LEASE, &["r1"])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Busy, "asked afresh");
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Ready);
}

/// Asserts that questions cannot fill the queue: with the worker inside a runtime call,
/// twenty prepare questions and twenty restore questions leave exactly one of each in the
/// queue, so `QUEUE - 2` replies are still taken and only the next is `Full` (a question per
/// heartbeat would have filled the queue and dropped replies earlier). Once the worker has
/// taken them, a restore asked again is answered `Done` after the lease has ended, which a
/// flag that is never cleared would stop. It catches a proxy that queues a question on every
/// heartbeat while the worker is busy, and one that never asks again.
#[test]
fn questions_never_crowd_replies_out_of_the_queue() {
    let dir = Scratch::new("jobs-quiesce-crowd");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let mut host = WorkerHost::new(worker.handle());

    machine.world.hold_creates();
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    machine.world.wait_entered(1);
    for _ in 0..20 {
        assert_eq!(host.prepare(LEASE), Quiet::Busy);
        assert_eq!(host.restore(LEASE), Restored::Busy);
    }
    for n in 0..QUEUE - 2 {
        assert_eq!(
            worker.try_deliver(granting(LEASE, &[])),
            Delivery::Delivered,
            "reply {n} has room"
        );
    }
    assert_eq!(worker.try_deliver(granting(LEASE, &[])), Delivery::Full);

    machine.world.release();
    let handle = worker.handle();
    assert!(handle.settle());
    machine.world.exit("r1", 0);
    assert!(handle.settle());
    let ended: HeartbeatReply = serde_json::from_value(body(None, &[], &["r1"])).unwrap();
    assert_eq!(worker.try_deliver(ended), Delivery::Delivered);
    assert!(handle.settle());
    // This question is answered Busy whether or not the flag was cleared (the published
    // answer is Busy until the worker has taken the order); the Done after the settle is
    // the check that the flag was cleared and a second order was sent.
    let _ = host.restore(LEASE);
    assert!(handle.settle());
    assert_eq!(host.restore(LEASE), Restored::Done);
}

/// A hook runner that tells the test when it has started and then waits to be released, and
/// counts its runs.
struct Gated {
    runs: Arc<AtomicUsize>,
    started: Sender<()>,
    release: Receiver<()>,
}

impl HookRunner for Gated {
    fn execute(&self, _program: &str, _args: &[&str], _env: &[(&str, &str)]) -> Ran {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let _ = self.started.send(());
        let _ = self.release.recv_timeout(HANG_GUARD);
        Ran::Exited {
            code: Some(0),
            text: "exit status: 0".to_owned(),
            stdout: String::new(),
        }
    }
}

/// Asserts the hook is not run for a host the agent's own checks found busy: with a run
/// held the question is busy and the hook has run zero times however often it is asked; once
/// the run's result is acknowledged the host is found quiet, the hook runs once and the
/// answer is ready. It catches a hook run before the checks passed, which would measure a
/// host that is not quiet.
#[test]
fn the_hook_waits_for_the_hosts_own_checks() {
    let dir = Scratch::new("jobs-quiesce-hook-busy");
    let machine = FakeMachine::new();
    let (runner, runs, _) = Counting::new(0, "");
    let worker = hooked(&dir, &machine, runner);
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());

    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert_eq!(
        worker.try_deliver(granting(LEASE, &[])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    for _ in 0..2 {
        assert_eq!(host.prepare(LEASE), Quiet::Busy);
        assert!(handle.settle());
    }
    assert_eq!(runs.load(Ordering::SeqCst), 0, "a run is held");

    machine.world.exit("r1", 0);
    assert!(handle.settle());
    assert_eq!(
        worker.try_deliver(granting(LEASE, &["r1"])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Busy, "asked afresh");
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Ready);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

/// Asserts the hook runs once for a lease even when a second question is queued while the
/// first is inside the hook: the test holds the hook until the second question is queued,
/// then releases it; the second order finds the final answer published and does nothing.
/// It catches a worker that runs the hook again for every order it takes.
#[test]
fn a_second_question_taken_after_the_answer_does_not_rerun_the_hook() {
    let dir = Scratch::new("jobs-quiesce-hook-race");
    let machine = FakeMachine::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let (started, started_rx) = channel();
    let (release_tx, release) = channel();
    let runner = Gated {
        runs: Arc::clone(&runs),
        started,
        release,
    };
    let worker = hooked(&dir, &machine, runner);
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());

    assert_eq!(
        worker.try_deliver(granting(LEASE, &[])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Busy);
    started_rx
        .recv_timeout(HANG_GUARD)
        .expect("the hook started");
    assert_eq!(host.prepare(LEASE), Quiet::Busy, "queues a second order");
    release_tx.send(()).unwrap();
    assert!(handle.settle());

    assert_eq!(host.prepare(LEASE), Quiet::Ready);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the hook ran once");
}

/// Asserts that a question asked about one lease and then about another, before the worker
/// has taken the first, is answered for the second only: the worker is held in the hook of an
/// earlier lease, the host asks about `LEASE` and then about `OTHER` (one order queues), the
/// worker follows `OTHER`, and when it is released the single order answers `OTHER` at once
/// (the hook runs for the earlier lease and for `OTHER`, never for `LEASE`). It catches an
/// order that answers for the lease first asked, which leaves the current lease unanswered.
#[test]
fn a_question_is_answered_for_the_lease_asked_last() {
    let dir = Scratch::new("jobs-quiesce-latest");
    let machine = FakeMachine::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let (started, started_rx) = channel();
    let (release_tx, release) = channel();
    let runner = Gated {
        runs: Arc::clone(&runs),
        started,
        release,
    };
    let worker = hooked(&dir, &machine, runner);
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());

    let earlier = "whole-0";
    assert_eq!(
        worker.try_deliver(granting(earlier, &[])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    assert_eq!(host.prepare(earlier), Quiet::Busy);
    started_rx
        .recv_timeout(HANG_GUARD)
        .expect("the hook started");
    assert_eq!(
        worker.try_deliver(granting(OTHER, &[])),
        Delivery::Delivered
    );
    assert_eq!(host.prepare(LEASE), Quiet::Busy);
    assert_eq!(
        host.prepare(OTHER),
        Quiet::Busy,
        "the same order, now for OTHER"
    );
    release_tx.send(()).unwrap();
    release_tx.send(()).unwrap();
    assert!(handle.settle());

    assert_eq!(host.prepare(OTHER), Quiet::Ready);
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the earlier lease and OTHER"
    );
}

/// Asserts the worker's watchdog allows a hook its whole bound and no more: with the hook
/// running and the manual clock moved past the watchdog's own limit but not past the hook's
/// bound, the worker is not stalled; moved past the bound it is. It catches a hook that is
/// mistaken for a stuck worker (the agent would end), and a bound that is not counted.
#[test]
fn a_hook_may_take_its_bound_without_the_worker_being_stalled() {
    let dir = Scratch::new("jobs-quiesce-stall");
    let machine = FakeMachine::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let (started, started_rx) = channel();
    let (release_tx, release) = channel();
    let runner = Gated {
        runs: Arc::clone(&runs),
        started,
        release,
    };
    let worker = hooked(&dir, &machine, runner);
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());

    assert_eq!(
        worker.try_deliver(granting(LEASE, &[])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Busy);
    started_rx
        .recv_timeout(HANG_GUARD)
        .expect("the hook started");
    let own = stall_bound(Duration::from_secs(1), 1);
    machine
        .clock
        .advance(u64::try_from(own.as_millis()).unwrap() + 500);
    assert!(!worker.stalled(), "inside the hook's bound");
    machine.clock.advance(1000);
    assert!(worker.stalled(), "past the hook's bound");
    release_tx.send(()).unwrap();
    assert!(handle.settle());
}

/// Asserts the watchdog's allowance is not given before the hook starts: with the worker
/// inside the post-job checks of a prepare (held by the test, so the hook has not run), the
/// manual clock moved just past the plain stall bound makes the worker stalled. Released, the
/// hook runs once, the answer is ready and the idle worker is not stalled. It catches an
/// allowance for the hook that is given to the whole prepare order, whatever its answer.
/// (A prepare answered busy, refused or taken over never reaches the hook; the window those
/// leave is pinned in `answers.rs`.)
#[test]
fn the_checks_before_the_hook_have_the_plain_stall_bound() {
    let dir = Scratch::new("jobs-quiesce-stall-checks");
    let machine = FakeMachine::new();
    let (runner, runs, _) = Counting::new(0, "");
    let worker = hooked(&dir, &machine, runner);
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());

    assert_eq!(
        worker.try_deliver(granting(LEASE, &[])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    machine.world.hold_checks();
    assert_eq!(host.prepare(LEASE), Quiet::Busy);
    machine.world.wait_checking(1);
    let own = stall_bound(Duration::from_secs(1), 1);
    assert!(!worker.stalled(), "no time has passed");
    machine
        .clock
        .advance(u64::try_from(own.as_millis()).unwrap() + 1);
    assert!(worker.stalled(), "the hook has not started: no allowance");
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    machine.world.release();
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Ready);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert!(!worker.stalled(), "an idle worker is never stalled");
}

/// Asserts a second restore order, queued while the first is inside its checks, changes
/// nothing once the first has published `Done`: the test holds the checks of the first order,
/// asks again (a second order queues behind it), and releases both; the answer is `Done` and
/// the checks ran once (one listing of the host's processes). It catches a worker that
/// recomputes the answer for every restore order: it would run the checks again (the count
/// of host listings is what bites here; the answer is `Done` either way when no job has
/// started, and `answers.rs` covers the job that starts between the orders).
#[test]
fn a_second_restore_order_through_the_worker_leaves_done() {
    let dir = Scratch::new("jobs-quiesce-restore-twice");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());

    machine.world.hold_checks();
    assert_eq!(host.restore(LEASE), Restored::Busy);
    machine.world.wait_checking(1);
    assert_eq!(
        host.restore(LEASE),
        Restored::Busy,
        "queues a second order: the first was taken"
    );
    machine.world.release();
    assert!(handle.settle());

    assert_eq!(host.restore(LEASE), Restored::Done, "the answer is kept");
    assert_eq!(machine.world.checked(), 1, "the checks ran once");
}

/// Asserts a hook that outlasts the plain stall bound (the clock moved past it while the hook
/// runs, still inside the hook's bound) leaves a worker that is alive, not stalled and with
/// the lease ready once the hook returns: the worker goes on to step and publish. The
/// limit right after the hook returns (the plain limit moved by the hook's run time) is
/// pinned in `answers.rs`; nothing in the real worker can be held between the hook's return
/// and the end of the order, so this test sees the end of the order only.
#[test]
fn a_hook_longer_than_the_plain_stall_bound_leaves_the_worker_sound() {
    let dir = Scratch::new("jobs-quiesce-long-hook");
    let machine = FakeMachine::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let (started, started_rx) = channel();
    let (release_tx, release) = channel();
    let worker = hooked(
        &dir,
        &machine,
        Gated {
            runs: Arc::clone(&runs),
            started,
            release,
        },
    );
    let handle = worker.handle();
    let mut host = WorkerHost::new(handle.clone());

    assert_eq!(
        worker.try_deliver(granting(LEASE, &[])),
        Delivery::Delivered
    );
    assert!(handle.settle());
    assert_eq!(host.prepare(LEASE), Quiet::Busy);
    started_rx
        .recv_timeout(HANG_GUARD)
        .expect("the hook started");
    let own = stall_bound(Duration::from_secs(1), 1);
    machine
        .clock
        .advance(u64::try_from(own.as_millis()).unwrap() + 500);
    assert!(!worker.stalled(), "past the plain bound, inside the hook's");
    release_tx.send(()).unwrap();
    assert!(handle.settle());

    assert!(!worker.ended());
    assert!(!worker.stalled());
    assert_eq!(host.prepare(LEASE), Quiet::Ready);
}
