//! The ends of the agent that are not a stop request, with jobs running: a refusal for good
//! and the worker's end. In both the agent stops the containers it holds before it exits
//! (through the worker after a refusal, through a runtime of its own after the worker's
//! end), and its exit status is the one the end always had. The runtime is in memory; the
//! worker is a real thread that the sleeper of these tests settles; every wait is for an
//! event with a hang guard, and the bound of a rescue is a test's own and short.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::Duration;

use js_agent::lifecycle::Capacity;
use js_agent::{Delivery, Drain, Exit, Handle, Session, Sleeper, Snapshot, Wake, Worker, drive};
use js_agent_wire::{HeldRun, HeldState};

use super::drain::container_flags;
use super::fake::{Build, FakeMachine, HANG_GUARD, World};
use super::{STEP_NEVER, plan, rescue, start, start_answer, starts};
use crate::bounds::{Capped, WakeBudget};
use crate::support::{
    CountingSleeper, Lowest, Ran, Scratch, Script, ScriptConnector, beat_ok, code, flags, hello,
    linux_host, lowest, refused, reply, welcome,
};

/// What a [`Settler`] does at its `n`th wait (counting from 0), before it settles the worker
/// for the last time.
type Act<'a> = Box<dyn Fn(usize) + 'a>;

/// A sleeper that settles the worker (taken from the hook) around an action of the test.
struct Settler<'a> {
    seen: &'a RefCell<Option<Handle>>,
    act: Act<'a>,
    calls: usize,
    budget: WakeBudget,
}

impl Sleeper for Settler<'_> {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        self.budget.spend();
        let handle = self.seen.borrow().clone().expect("the hook was told");
        handle.settle();
        (self.act)(self.calls);
        self.calls += 1;
        handle.settle();
        Wake::Elapsed
    }
}

/// Runs the whole agent with containers over `machine`; the server welcomes it, starts
/// `r1` and `r2` in the answer of its first heartbeat and then says `then`. `act` runs at
/// the waits.
fn agent(
    name: &str,
    machine: &Arc<FakeMachine>,
    then: Vec<crate::support::Reply>,
    act: impl Fn(usize) + 'static,
) -> Ran {
    let dir = Scratch::new(name);
    let owned = container_flags(&dir);
    let extra: Vec<&str> = owned.iter().map(String::as_str).collect();
    let args = flags(&extra);
    let mut replies = vec![welcome(1), reply(200, start_answer(&["r1", "r2"]))];
    replies.extend(then);
    let (script, _) = Script::new(replies);
    let connector = ScriptConnector::of(script);
    let seen: RefCell<Option<Handle>> = RefCell::new(None);
    let hook = |handle: Handle| *seen.borrow_mut() = Some(handle);
    let mut sleeper = Settler {
        seen: &seen,
        act: Box::new(act),
        calls: 0,
        budget: WakeBudget::new(),
    };
    let jobs: crate::support::Jobs<'_> = (machine.clone(), Some(&hook));
    crate::support::run_on(
        &args,
        &linux_host(),
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
    )
}

/// Asserts that a server that refuses the agent for good while two runs are running ends
/// it with the refusal's status (2) and the refusal on stderr, and that both containers
/// were stopped before the agent exited. It catches an agent that leaves a container
/// running, with no wall limit enforced, when its token is revoked.
#[test]
fn a_refusal_for_good_stops_both_runs_and_keeps_its_status() {
    let machine = Arc::new(FakeMachine::new());
    let world = machine.world.clone();
    let then = vec![refused(401, "unauthorised")];
    let ran = agent("jobs-ends-fatal", &machine, then, |_| {});
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(ran.err.contains("401"), "{}", ran.err);
    let log = world.log();
    assert_eq!(world.created(), ["r1", "r2"]);
    assert!(log.contains(&"stop r1".to_owned()), "{log:?}");
    assert!(log.contains(&"stop r2".to_owned()), "{log:?}");
}

/// Asserts that a worker that dies with two runs running ends the agent with status 1 and
/// that both containers are stopped through a runtime of the agent's own, which lists them
/// by label first. It catches an agent that leaves the containers running when its worker
/// is gone.
#[test]
fn a_dead_worker_has_its_runs_stopped_without_it() {
    let machine = Arc::new(FakeMachine::new());
    let world = machine.world.clone();
    let on = world.clone();
    let act = move |n: usize| {
        if n == 1 {
            on.panic_at("poll");
        }
    };
    let then = vec![beat_ok(), beat_ok()];
    let ran = agent("jobs-ends-dead", &machine, then, act);
    assert_eq!(ran.status(), code(1), "{}", ran.err);
    assert!(ran.err.contains("job worker stopped"), "{}", ran.err);
    let log = world.log();
    let lists = log.iter().filter(|call| *call == "list").count();
    assert_eq!(lists, 2, "the adoption and the rescue each list: {log:?}");
    assert!(log.contains(&"stop r1".to_owned()), "{log:?}");
    assert!(log.contains(&"stop r2".to_owned()), "{log:?}");
    assert!(!ran.err.contains("rescue:"), "{}", ran.err);
}

/// Asserts that a stop that fails while the worker's end is rescued is logged by the
/// container, that the other container is still stopped, and that the agent still exits
/// with status 1. It catches a rescue that gives up at the first error and one that hides
/// what it could not stop.
#[test]
fn a_stop_that_fails_is_logged_and_the_agent_still_ends() {
    let machine = Arc::new(FakeMachine::new());
    let world = machine.world.clone();
    let on = world.clone();
    let act = move |n: usize| {
        if n == 1 {
            on.panic_at("poll");
            on.fail("stop", 1);
        }
    };
    let then = vec![beat_ok(), beat_ok()];
    let ran = agent("jobs-ends-stop", &machine, then, act);
    assert_eq!(ran.status(), code(1), "{}", ran.err);
    assert!(ran.err.contains("rescue: cannot stop c"), "{}", ran.err);
    assert!(ran.err.contains("stop failed"), "{}", ran.err);
    let stops = stops_in(&world);
    assert_eq!(stops, 2, "both were tried");
}

/// Asserts that an agent without `--container-bin` that is refused for good ends as it
/// always did: status 2, the one line on stderr, and no call to a runtime. It catches a
/// refusal path that reaches for a runtime the agent does not have.
#[test]
fn without_containers_a_refusal_is_as_before() {
    let machine = Arc::new(FakeMachine::new());
    let (script, _) = Script::new(vec![refused(403, "host-bound")]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = crate::support::CountingSleeper::stopping_at(1);
    let jobs: crate::support::Jobs<'_> = (machine.clone(), None);
    let args = flags(&[]);
    let host = linux_host();
    let ran = crate::support::run_on(&args, &host, &connector, &mut sleeper, &mut Lowest, jobs);
    assert_eq!(ran.status(), code(2));
    assert_eq!(
        ran.err,
        "js-agent: the server refused the hello with status 403 (host-bound); not retrying\n"
    );
    assert!(machine.world.log().is_empty());
}

/// A running run `r1` in a world, from a worker that has since been left behind: the
/// machine to rescue it over.
fn world_with_r1(name: &str) -> (World, Worker, Scratch) {
    let dir = Scratch::new(name);
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    (machine.world.clone(), worker, dir)
}

/// How many stops the runtime was asked for.
fn stops_in(world: &World) -> usize {
    let log = world.log();
    log.iter().filter(|call| call.starts_with("stop ")).count()
}

fn running(run: &str) -> Snapshot {
    Snapshot {
        held: vec![HeldRun {
            run_id: run.to_owned(),
            state: HeldState::Running,
        }],
        ..Snapshot::default()
    }
}

/// Asserts that the rescue says why when its own runtime cannot be made and when it cannot
/// list, and stops nothing then. It catches a rescue that fails silently.
#[test]
fn a_rescue_that_cannot_reach_the_engine_says_so() {
    let dir = Scratch::new("jobs-ends-noengine");
    let broken = FakeMachine::failing(Build::RuntimeFails);
    let rescuing = rescue(&dir, &broken);
    let worker = Worker::orphaned(broken.clock.clone(), running("r1"), rescuing);
    let lines = worker.rescue();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("no runtime"), "{lines:?}");

    let (world, _held, dir) = world_with_r1("jobs-ends-w1");
    let machine = FakeMachine::of(&world);
    let rescuing = rescue(&dir, &machine);
    let worker = Worker::orphaned(machine.clock.clone(), running("r1"), rescuing);
    world.fail("list", 1);
    let lines = worker.rescue();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("cannot list"), "{lines:?}");
    assert!(!world.log().contains(&"stop r1".to_owned()));
}

/// Asserts that a rescue stops at most its limit of containers and names the ones it did
/// not try. It catches a rescue with no bound on its calls.
#[test]
fn a_rescue_stops_no_more_than_its_limit() {
    let dir = Scratch::new("jobs-ends-limit");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let both = starts(&["r1", "r2"]);
    assert_eq!(worker.try_deliver(both), Delivery::Delivered);
    assert!(worker.handle().settle());
    let limited = rescue(&dir, &machine).limited(1, Duration::from_secs(60));
    let gone = Worker::orphaned(machine.clock.clone(), running("r1"), limited);
    let lines = gone.rescue();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("not tried"), "{lines:?}");
    assert_eq!(stops_in(&machine.world), 1);
}

/// Asserts that a rescue whose runtime does not answer returns when its bound passes, and
/// says that it did not finish. It catches an exit that waits for a stuck engine.
#[test]
fn a_rescue_that_does_not_finish_returns_at_its_bound() {
    let (world, _held, dir) = world_with_r1("jobs-ends-w2");
    let machine = FakeMachine::of(&world);
    let limited = rescue(&dir, &machine).limited(4, Duration::from_millis(50));
    let gone = Worker::orphaned(machine.clock.clone(), running("r1"), limited);
    world.hold_lists();
    let lines = gone.rescue();
    world.release();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("did not finish"), "{lines:?}");
}

/// Asserts that a container the rescue cannot read is still given a stop, with a line
/// that names it, and that the other two are stopped as before. It catches a rescue that
/// treats one unreadable container as a listing it cannot make and stops nothing.
#[test]
fn a_container_that_cannot_be_read_is_still_stopped_by_the_rescue() {
    let dir = Scratch::new("jobs-ends-unread");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let three = starts(&["r1", "r2", "r3"]);
    assert_eq!(worker.try_deliver(three), Delivery::Delivered);
    assert!(worker.handle().settle());
    let world = machine.world.clone();
    let limited = rescue(&dir, &machine).limited(4, Duration::from_secs(60));
    let gone = Worker::orphaned(machine.clock.clone(), running("r1"), limited);
    world.fail("poll", 1);
    let lines = gone.rescue();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("cannot read c1"), "{lines:?}");
    assert!(lines[0].contains("poll failed"), "{lines:?}");
    assert!(lines[0].contains("stopping it anyway"), "{lines:?}");
    assert_eq!(stops_in(&world), 3, "{:?}", world.log());
}

/// Asserts that the rescue of an agent never stops a container that another agent (a
/// different host id) made on the same runtime. It catches a rescue that stops every
/// container of the runtime.
#[test]
fn a_rescue_never_stops_another_agents_container() {
    let dir = Scratch::new("jobs-ends-other");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    let world = machine.world.clone();
    world.leak("theirs", "h2");
    assert_eq!(world.alive(), 2);
    let limited = rescue(&dir, &machine).limited(4, Duration::from_secs(60));
    let gone = Worker::orphaned(machine.clock.clone(), running("r1"), limited);
    let lines = gone.rescue();
    assert!(lines.is_empty(), "{lines:?}");
    let log = world.log();
    assert!(log.contains(&"stop r1".to_owned()), "{log:?}");
    assert!(!log.contains(&"stop theirs".to_owned()), "{log:?}");
    assert_eq!(stops_in(&world), 1, "{log:?}");
}

/// Asserts that a refusal for good, with a worker that does not answer, stops the runs
/// through the rescue after the bound: the worker is asked, waited for, and then bypassed.
/// It catches a refusal path that trusts a worker it cannot reach.
#[test]
fn a_worker_that_does_not_answer_a_refusal_is_bypassed() {
    let (world, _held, dir) = world_with_r1("jobs-ends-w3");
    let machine = FakeMachine::of(&world);
    let limited = rescue(&dir, &machine).limited(4, Duration::from_secs(5));
    let stuck = Worker::orphaned(machine.clock.clone(), running("r1"), limited);
    let lines = stuck.wind_down();
    assert!(lines.is_empty(), "{lines:?}");
    let log = world.log();
    assert!(log.contains(&"stop r1".to_owned()), "{log:?}");
}

/// Asserts that what a refusal's rescue could not stop is written to the log before the
/// agent ends with the refusal. It catches a refusal path that hides a container it left
/// running.
#[test]
fn a_refusal_logs_what_it_could_not_stop() {
    let (world, _held, dir) = world_with_r1("jobs-ends-w4");
    let machine = FakeMachine::of(&world);
    let limited = rescue(&dir, &machine).limited(4, Duration::from_secs(5));
    let stuck = Worker::orphaned(machine.clock.clone(), running("r1"), limited);
    world.fail("list", 1);
    let drain = Drain {
        wait: Duration::from_secs(10),
        settle: Duration::from_secs(30),
    };
    let (mut script, _) = Script::new(vec![refused(401, "unauthorised")]);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&stuck, drain);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let mut log = Capped::new();
    let exit = drive(&mut session, &mut sleeper, &mut log);
    assert!(matches!(exit, Exit::Fatal(_)), "{exit:?}");
    let text = log.text();
    assert!(text.contains("rescue: cannot list"), "{text}");
}

/// Runs `work` on a thread of its own and waits for its answer for at most the hang guard.
/// A call that waits for a time no test expects ends the test with a message; the thread
/// is left behind (the process ends anyway).
fn promptly<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, answer) = sync_channel(1);
    thread::spawn(move || {
        let _ = done.send(work());
    });
    answer
        .recv_timeout(HANG_GUARD)
        .expect("the call waited for a time it should not wait for")
}

/// A worker over `machine` whose stop grace (and so its wind-down wait and rescue bound)
/// is an hour: a wait that is not cut short outlasts the hang guard.
fn slow_to_give_up(dir: &Scratch, machine: &FakeMachine) -> Worker {
    let mut plan = plan(dir, machine, STEP_NEVER);
    plan.config.stop_grace = Duration::from_secs(3600);
    plan.rescue_bound = None;
    Worker::start(plan).unwrap_or_else(|why| panic!("the worker did not start: {why}"))
}

/// Asserts that a worker that dies while it is being wound down is rescued at once: the
/// worker holds a running run, its runtime panics in the stop it is asked for, and
/// `wind_down` returns without waiting out the hour it would otherwise wait, with the run
/// stopped through a runtime of the agent's own after a second listing. It catches a wait
/// that does not look at the worker's end and so holds the agent for hours.
#[test]
fn a_worker_that_ends_while_winding_down_is_rescued_at_once() {
    let dir = Scratch::new("jobs-ends-w5");
    let machine = FakeMachine::new();
    let worker = slow_to_give_up(&dir, &machine);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    machine.world.panic_at("stop");
    let lines = promptly(move || worker.wind_down());
    assert!(lines.is_empty(), "{lines:?}");
    let log = machine.world.log();
    let lists = log.iter().filter(|call| *call == "list").count();
    assert_eq!(lists, 2, "the adoption and the rescue each list: {log:?}");
    assert_eq!(
        stops_in(&machine.world),
        2,
        "the worker's, then the rescue's"
    );
}

/// Asserts that a worker that ended while it was inside a start, so that no snapshot ever
/// showed the run, is still rescued by `wind_down`: the rescue lists the containers again.
/// It catches a wind-down that rescues only what the last snapshot shows running.
#[test]
fn a_worker_that_ended_inside_a_start_is_rescued() {
    let dir = Scratch::new("jobs-ends-w6");
    let machine = FakeMachine::new();
    let worker = slow_to_give_up(&dir, &machine);
    machine.world.panic_at("create");
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(!worker.handle().settle(), "the worker ended");
    assert!(!worker.snapshot().running());
    let lines = promptly(move || worker.wind_down());
    assert!(lines.is_empty(), "{lines:?}");
    let log = machine.world.log();
    let lists = log.iter().filter(|call| *call == "list").count();
    assert_eq!(lists, 2, "the adoption and the rescue each list: {log:?}");
}

/// Asserts that a worker that is alive but never answers is waited for only for the time
/// the rescue gives it, not for the whole bound of the rescue: with a wait of 50 ms and a
/// bound of an hour, `wind_down` returns and the run is stopped by the rescue. It catches
/// a wind-down that waits out the bound of the rescue for a stalled worker.
#[test]
fn a_stalled_worker_is_waited_for_only_the_wind_down_wait() {
    let (world, _held, dir) = world_with_r1("jobs-ends-w7");
    let machine = FakeMachine::of(&world);
    let hour = Duration::from_secs(3600);
    let quick = rescue(&dir, &machine)
        .limited(4, hour)
        .winding(Duration::from_millis(50));
    let stuck = Worker::orphaned(machine.clock.clone(), running("r1"), quick);
    let lines = promptly(move || stuck.wind_down());
    assert!(lines.is_empty(), "{lines:?}");
    assert!(world.log().contains(&"stop r1".to_owned()));
}

/// Asserts that rescuing closes the worker: a worker that is inside a start when the
/// rescue runs, with a second start waiting in its queue, makes the first container when
/// it wakes and never the second. It catches a rescue that leaves a stalled worker free to
/// start the runs it has queued, after the rescue listed the containers.
#[test]
fn a_rescue_closes_the_worker_so_a_queued_start_is_not_taken() {
    let dir = Scratch::new("jobs-ends-w8");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    machine.world.hold_creates();
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    machine.world.wait_entered(1);
    assert_eq!(worker.try_deliver(starts(&["r2"])), Delivery::Delivered);
    let lines = worker.rescue();
    assert!(lines.is_empty(), "{lines:?}");
    machine.world.release();
    assert!(worker.handle().settle());
    assert_eq!(
        machine.world.created(),
        ["r1"],
        "r2 was queued behind a stall"
    );
}

/// Asserts that `wind_down` closes the worker itself: with one start in progress and
/// another queued, and nothing yet shown as running (so no rescue follows), the queued
/// start is not taken once the worker goes on. It catches a wind-down that only asks for
/// the stops and lets the worker take the starts that wait.
#[test]
fn a_wind_down_closes_the_worker() {
    let dir = Scratch::new("jobs-ends-w9");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    machine.world.hold_creates();
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    machine.world.wait_entered(1);
    assert_eq!(worker.try_deliver(starts(&["r2"])), Delivery::Delivered);
    assert!(worker.wind_down().is_empty());
    machine.world.release();
    assert!(worker.handle().settle());
    assert_eq!(
        machine.world.created(),
        ["r1"],
        "r2 was queued behind a stall"
    );
}

/// Asserts that exited containers, which the listing holds too, do not use up the limit of
/// the rescue: of three containers, two have exited, and a limit of one still stops the
/// one that runs and no other. It catches a rescue that spends its limit on containers
/// that are already stopped and leaves a running one.
#[test]
fn exited_containers_do_not_use_up_the_rescue_limit() {
    let dir = Scratch::new("jobs-ends-w10");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert_eq!(
        worker.try_deliver(starts(&["r1", "r2", "r3"])),
        Delivery::Delivered
    );
    assert!(worker.handle().settle());
    machine.world.exit("r1", 0);
    machine.world.exit("r2", 0);
    let limited = rescue(&dir, &machine).limited(1, Duration::from_secs(60));
    let gone = Worker::orphaned(machine.clock.clone(), running("r3"), limited);
    let lines = gone.rescue();
    assert!(lines.is_empty(), "{lines:?}");
    let log = machine.world.log();
    assert_eq!(stops_in(&machine.world), 1, "{log:?}");
    assert!(log.contains(&"stop r3".to_owned()), "{log:?}");
}

/// Asserts that a container the rescue cannot read the state of is still stopped: the
/// first poll fails and the run is stopped all the same. It catches a rescue that skips
/// what it cannot tell is running.
#[test]
fn a_container_whose_state_is_unknown_is_stopped() {
    let (world, _held, dir) = world_with_r1("jobs-ends-w11");
    let machine = FakeMachine::of(&world);
    let rescuing = rescue(&dir, &machine);
    let gone = Worker::orphaned(machine.clock.clone(), running("r1"), rescuing);
    world.fail("poll", 1);
    let lines = gone.rescue();
    assert!(lines.is_empty(), "{lines:?}");
    assert!(world.log().contains(&"stop r1".to_owned()));
}

/// Asserts that a host that runs more containers than the old limit of 16 has every one
/// of them stopped by the rescue with its default limit. It catches a limit that leaves
/// running containers behind on a host with many cores.
#[test]
fn every_running_container_is_stopped_beyond_sixteen() {
    let dir = Scratch::new("jobs-ends-w12");
    let machine = FakeMachine::new();
    let mut big = plan(&dir, &machine, STEP_NEVER);
    big.config.capacity = Capacity {
        cpu_millis: 64_000,
        memory_bytes: 1 << 40,
        gpus: Default::default(),
    };
    let worker = Worker::start(big).unwrap_or_else(|why| panic!("no worker: {why}"));
    let names: Vec<String> = (0..20).map(|n| format!("r{n}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    assert_eq!(worker.try_deliver(starts(&names)), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert_eq!(machine.world.created().len(), 20, "all were started");
    let rescuing = rescue(&dir, &machine);
    let gone = Worker::orphaned(machine.clock.clone(), running("r0"), rescuing);
    let lines = gone.rescue();
    assert!(lines.is_empty(), "{lines:?}");
    assert_eq!(stops_in(&machine.world), 20);
}
