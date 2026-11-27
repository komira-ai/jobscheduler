//! The handover: a stop request that finds the handover file in the state root leaves every
//! run to the next process of the agent instead of draining the host, once no start, stop or
//! post-job check is under way; the next process (a second worker over the same world and
//! state root, as a restarted agent) adopts the runs and reports each result once. The worker
//! is a real thread; the sleepers settle it, or wait for an event, never for a time.

use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::lifecycle::handover;
use js_agent::{Delivery, Drain, Exit, Handle, Session, Sleeper, Wake, Worker, drive};
use js_agent_wire::{HeldJob, HeldState, JobResult};
use serde_json::{Value, json};

use super::fake::FakeMachine;
use super::{job_start, start, starts};
use crate::bounds::{Capped, WakeBudget};
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

/// A sleeper that only acts: what the worker does meanwhile is the test's to wait for.
struct Acting<'a> {
    calls: usize,
    act: Act<'a>,
    budget: WakeBudget,
}

impl Sleeper for Acting<'_> {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        self.budget.spend();
        let n = self.calls;
        self.calls += 1;
        (self.act)(n)
    }
}

/// How long a test waits for the worker's answer to a park: a guard against a hang, never an
/// expectation (a worker that is not stuck answers at once).
const ANSWER: Duration = Duration::from_secs(60);

const LONG: Drain = Drain {
    wait: Duration::from_secs(3600),
    settle: Duration::from_secs(30),
};

/// The state root the jobs of `dir` keep their records in (see `super::plan`).
fn state(dir: &Scratch) -> PathBuf {
    dir.path("state")
}

/// Drives a session over `replies`, looking for a handover in `root` when given one, with
/// `sleeper`; returns how it ended, what it logged and what it sent.
fn driven(
    worker: &Worker,
    root: Option<&Path>,
    replies: Vec<Reply>,
    drain: Drain,
    sleeper: &mut dyn Sleeper,
) -> (Exit, String, Vec<(String, Value)>) {
    let (mut script, sent) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest())
        .with_jobs(worker, drain)
        .with_handover(root);
    let mut log = Capped::new();
    let exit = drive(&mut session, sleeper, &mut log);
    let sent = sent.borrow().clone();
    (exit, log.text(), sent)
}

/// [`driven`] with a sleeper that settles the worker around `act`, and the handover looked
/// for in the state root of `dir`.
fn handed(
    dir: &Scratch,
    worker: &Worker,
    replies: Vec<Reply>,
    drain: Drain,
    act: impl FnMut(usize) -> Wake,
) -> (Exit, String, Vec<(String, Value)>) {
    let mut sleeper = Settling {
        handle: worker.handle(),
        calls: 0,
        act: Box::new(act),
        budget: WakeBudget::new(),
    };
    let root = state(dir);
    driven(worker, Some(&root), replies, drain, &mut sleeper)
}

fn phases(sent: &[(String, Value)]) -> Vec<String> {
    let phase = |(_, body): &(String, Value)| body["phase"].as_str().unwrap().to_owned();
    sent.iter().map(phase).collect()
}

/// The job ids of the results every message in `sent` carried, in order.
fn results(sent: &[(String, Value)]) -> Vec<String> {
    let jobs = sent.iter().flat_map(|(_, body)| {
        let jobs = body["jobs"].as_array().cloned().unwrap_or_default();
        jobs.into_iter()
            .filter(|job| job.get("result").is_some())
            .map(|job| job["job_id"].as_str().unwrap().to_owned())
    });
    jobs.collect()
}

/// The result of the job at `at` in `jobs`, which the test knows to have ended.
fn result_of(jobs: &[HeldJob], at: usize) -> &JobResult {
    jobs[at].result.as_ref().expect("the job's result")
}

/// A worker over `machine` running `runs`, with the handover file written in its state root.
fn running(dir: &Scratch, machine: &FakeMachine, runs: &[&str]) -> Worker {
    let worker = start(dir, machine);
    assert_eq!(worker.try_deliver(starts(runs)), Delivery::Delivered);
    assert!(worker.handle().settle());
    handover::write(&state(dir)).unwrap();
    worker
}

/// The next process of the agent: the first worker is gone (its thread parked, its queue
/// dropped) and a second one is started over the same world and state root, which adopts.
fn next_process(dir: &Scratch, machine: &FakeMachine, first: Worker) -> Worker {
    drop(first);
    start(dir, machine)
}

/// Whether the state root of `dir` holds the record of owed checks of any run.
fn owed(dir: &Scratch) -> Vec<String> {
    let names = std::fs::read_dir(state(dir)).unwrap();
    let names = names.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned());
    names.filter(|name| name.starts_with("owed-")).collect()
}

/// Asserts the handover of a busy host: at the stop request the agent says `draining` once,
/// stops nothing and ends with status 0, both jobs still running; the next process adopts
/// both as running, and when each ends it reports that job's own result, once, with its own
/// code. Catches an update stop that drains or stops a running job, one that waits for the
/// jobs to end, a next process that loses a job, and a result that is sent twice or under
/// another job's name.
#[test]
fn a_stop_asked_to_hand_over_leaves_the_running_jobs_to_the_next_process_which_reports_each_once() {
    let dir = Scratch::new("jobs-handover-busy");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1", "r2"]);
    let replies = vec![welcome(1), beat_ok()];
    let (exit, log, sent) = handed(&dir, &worker, replies, LONG, |_| Wake::Stop);

    assert_eq!(exit, Exit::Stopped);
    assert_eq!(phases(&sent), ["serving", "draining"]);
    assert_eq!(sent[1].1["jobs"][0]["state"], "running");
    assert_eq!(sent[1].1["jobs"][1]["state"], "running");
    assert!(results(&sent).is_empty());
    assert_eq!(machine.world.alive(), 2);
    assert!(
        !machine
            .world
            .log()
            .iter()
            .any(|call| call.starts_with("stop ")),
        "{:?}",
        machine.world.log()
    );
    assert!(
        log.contains("js-agent: handing over: no new job is taken"),
        "{log}"
    );
    assert!(
        log.contains("js-agent: handed over: every run is left as it is to the next process"),
        "{log}"
    );
    assert!(
        !log.contains("draining: a run or a result is left"),
        "{log}"
    );

    let next = next_process(&dir, &machine, worker);
    let held = next.snapshot().jobs;
    let states: Vec<(&str, HeldState)> = held
        .iter()
        .map(|job| (job.job_id.as_str(), job.state))
        .collect();
    assert_eq!(
        states,
        [("r1", HeldState::Running), ("r2", HeldState::Running)]
    );
    machine.world.exit("r2", 3);
    assert!(next.handle().settle());
    machine.world.exit("r1", 0);
    assert!(next.handle().settle());
    let jobs = next.snapshot().jobs;
    let mut ends: Vec<(&str, Option<i32>)> = jobs
        .iter()
        .filter_map(|job| job.result.as_ref())
        .map(|result| (result.job_id.as_str(), result.exit_code))
        .collect();
    ends.sort_unstable();
    assert_eq!(ends, [("r1", Some(0)), ("r2", Some(3))]);
    assert_eq!(
        machine.world.created(),
        ["r1", "r2"],
        "nothing is started twice"
    );
}

/// Asserts a job that ends while no agent runs: after the handover the job exits; the next
/// process finds it ended, holds its result with its exit code as the only result, and sends
/// it. Catches a job whose end in the restart's gap is lost, or reported as killed.
#[test]
fn a_job_that_ends_during_the_restart_is_found_and_reported_by_the_next_process() {
    let dir = Scratch::new("jobs-handover-gap");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1"]);
    let replies = vec![welcome(1), beat_ok()];
    let (exit, _, sent) = handed(&dir, &worker, replies, LONG, |_| Wake::Stop);
    assert_eq!(exit, Exit::Stopped);
    assert!(results(&sent).is_empty());

    machine.world.exit("r1", 7);
    let next = next_process(&dir, &machine, worker);
    assert!(next.handle().settle());
    let jobs = next.snapshot().jobs;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let result = result_of(&jobs, 0);
    assert_eq!(result.job_id, "r1");
    assert_eq!(result.exit_code, Some(7));
    assert_eq!(result.outcome, js_agent_wire::Outcome::Failed);
}

/// Asserts that a result the server has not acknowledged when the agent hands over is kept
/// for the next process, which sends it, and only it: the job ended before the stop request,
/// its result rode the heartbeat of the handover unacknowledged, and the next process holds
/// that one result and not a second. Catches a handover that drops what it could not report,
/// and a next process that reports an end twice.
#[test]
fn a_result_not_yet_acknowledged_at_the_handover_is_sent_once_by_the_next_process() {
    let dir = Scratch::new("jobs-handover-unacked");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1"]);
    machine.world.exit("r1", 0);
    assert!(worker.handle().settle());
    assert!(worker.snapshot().jobs[0].result.is_some());
    let replies = vec![welcome(1), beat_ok()];
    let (exit, _, sent) = handed(&dir, &worker, replies, LONG, |_| Wake::Stop);
    assert_eq!(exit, Exit::Stopped);
    assert_eq!(
        results(&sent),
        ["r1"],
        "sent with the heartbeat of the handover, unacknowledged"
    );

    let next = next_process(&dir, &machine, worker);
    let jobs = next.snapshot().jobs;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let result = result_of(&jobs, 0);
    assert_eq!(result.job_id, "r1");
    assert_eq!(result.exit_code, Some(0));
}

/// Asserts that a start under way (its image still being pulled: nothing of it exists yet)
/// holds the handover until it runs: the heartbeats of the handover list it starting and then
/// running, the agent leaves only then, and the next process adopts it running. Catches a
/// handover that ends the process with a start half made, whose job the next process would
/// never know of and the server would count as lost.
#[test]
fn a_start_under_way_is_waited_for_and_then_handed_over_running() {
    let dir = Scratch::new("jobs-handover-start");
    let machine = FakeMachine::new();
    let image = job_start("r1").image;
    machine.world.begin_pull(&image);
    let worker = running(&dir, &machine, &["r1"]);
    assert_eq!(worker.snapshot().jobs[0].state, HeldState::Starting);
    let replies = vec![welcome(1), beat_ok(), beat_ok()];
    let world = machine.world.clone();
    let act = move |n: usize| match n {
        0 => Wake::Stop,
        _ => {
            world.end_pull(&image, true);
            Wake::Elapsed
        }
    };
    let (exit, log, sent) = handed(&dir, &worker, replies, LONG, act);

    assert_eq!(exit, Exit::Stopped);
    assert_eq!(phases(&sent), ["serving", "draining", "draining"]);
    assert_eq!(sent[1].1["jobs"][0]["state"], "starting");
    assert_eq!(sent[2].1["jobs"][0]["state"], "running");
    assert!(log.contains("js-agent: handed over"), "{log}");
    assert_eq!(machine.world.created(), ["r1"]);

    let next = next_process(&dir, &machine, worker);
    assert_eq!(next.snapshot().jobs[0].state, HeldState::Running);
    assert_eq!(
        machine.world.created(),
        ["r1"],
        "adopted, not started again"
    );
}

/// Asserts that a job's post-job check under way when the stop request comes holds the
/// handover until the check has run: the park is answered busy and the agent heartbeats on,
/// with the record of the owed check on disk and nothing said of a handover; once the check
/// has run the record is gone, the agent hands over, and the next process starts untainted
/// with the job's result to send. Catches a handover that ends the process inside a check,
/// which taints the host at the next start (`check-owed`), and an update that waits for
/// anything more than the check.
#[test]
fn a_check_under_way_is_waited_for_and_the_next_process_starts_untainted() {
    let dir = Scratch::new("jobs-handover-check");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1"]);
    machine.world.hold_checks();
    let world = machine.world.clone();
    let seen = std::cell::RefCell::new(None);
    let mut sleeper = Acting {
        calls: 0,
        act: Box::new(|n| match n {
            0 => {
                // The job ends just as the stop request comes: its cleanup begins at the
                // worker's next step, inside the handover.
                world.exit("r1", 0);
                Wake::Stop
            }
            _ => {
                // The park was answered busy: the check is under way, its record on disk.
                world.wait_checking(1);
                *seen.borrow_mut() = Some(owed(&dir));
                world.release();
                Wake::Elapsed
            }
        }),
        budget: WakeBudget::new(),
    };
    let root = state(&dir);
    let replies = vec![welcome(1), beat_ok(), beat_ok(), beat_ok()];
    let (exit, log, sent) = driven(&worker, Some(&root), replies, LONG, &mut sleeper);

    assert_eq!(*seen.borrow(), Some(vec!["owed-r1".to_owned()]));
    assert_eq!(sleeper.calls, 2, "one wait while the check ran");
    assert_eq!(phases(&sent), ["serving", "draining", "draining"]);
    assert_eq!(exit, Exit::Stopped);
    assert!(log.contains("js-agent: handed over"), "{log}");
    assert!(owed(&dir).is_empty(), "{:?}", owed(&dir));

    let next = next_process(&dir, &machine, worker);
    let snapshot = next.snapshot();
    assert_eq!(snapshot.taint, None);
    assert_eq!(snapshot.jobs.len(), 1);
    assert_eq!(result_of(&snapshot.jobs, 0).job_id, "r1");
}

/// Asserts that the next process after a hand-over runs a post-job check the earlier process
/// left owed with the agent's own Linux checks, by the rules that need no baseline: on a quiet
/// host it removes the record, starts untainted and keeps the job it adopted running. The
/// record is written as `owe` writes it before a cleanup's first step, standing for a job
/// whose cleanup was under way when the earlier process ended (a hand-over whose bound passed,
/// a second stop). Catches a start that taints the host `check-owed` for a check it can run.
#[test]
fn the_next_process_runs_a_check_left_owed_and_starts_untainted() {
    let dir = Scratch::new("jobs-handover-owed-left");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1"]);
    let (exit, _, _) = handed(&dir, &worker, vec![welcome(1), beat_ok()], LONG, |_| {
        Wake::Stop
    });
    assert_eq!(exit, Exit::Stopped);
    js_agent::lifecycle::StateRoot::open(&state(&dir))
        .unwrap()
        .owe("r0")
        .unwrap();

    let next = next_process(&dir, &machine, worker);
    let snapshot = next.snapshot();
    assert_eq!(snapshot.taint, None);
    assert!(owed(&dir).is_empty(), "{:?}", owed(&dir));
    assert_eq!(snapshot.jobs.len(), 1);
    assert_eq!(snapshot.jobs[0].state, HeldState::Running);
}

/// Asserts the end of a handover that the work under way does not let happen: with a pull
/// that never ends, the agent leaves once the drain wait has passed, says that what was under
/// way is left, and stops nothing; a second stop request ends it the same way at once.
/// Catches a handover that waits for ever, and one that falls back to stopping the jobs.
#[test]
fn a_handover_held_past_the_drain_wait_or_stopped_twice_ends_and_stops_nothing() {
    for (name, drain, second) in [
        (
            "jobs-handover-bound",
            Drain {
                wait: Duration::from_secs(10),
                settle: Duration::from_secs(30),
            },
            false,
        ),
        ("jobs-handover-twice", LONG, true),
    ] {
        let dir = Scratch::new(name);
        let machine = FakeMachine::new();
        let pulled = job_start("r2").image;
        let worker = running(&dir, &machine, &["r1"]);
        machine.world.begin_pull(&pulled);
        assert_eq!(worker.try_deliver(starts(&["r2"])), Delivery::Delivered);
        assert!(worker.handle().settle());
        let replies = vec![welcome(1), beat_ok(), beat_ok(), beat_ok(), beat_ok()];
        let act = move |n: usize| match (n, second) {
            (0, _) | (_, true) => Wake::Stop,
            _ => Wake::Elapsed,
        };
        let (exit, log, sent) = handed(&dir, &worker, replies, drain, act);

        assert_eq!(exit, Exit::Stopped, "{name}");
        let draining = phases(&sent).iter().filter(|p| *p == "draining").count();
        assert_eq!(draining, if second { 1 } else { 2 }, "{name}: {sent:?}");
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
            !machine
                .world
                .log()
                .iter()
                .any(|call| call.starts_with("stop ")),
            "{name}"
        );
    }
}

/// Asserts that a stop drains as it always did when there is no request: with no handover
/// file, with one a session was not told to look for (an agent whose next process could not
/// adopt the runs), and with a file that breaks the form, which the log names. Catches a
/// handover taken without the request, or from a file anybody could have left.
#[test]
fn a_stop_without_a_usable_request_drains() {
    for (name, write, look, said) in [
        ("jobs-handover-none", false, true, None),
        ("jobs-handover-unlooked", true, false, None),
        (
            "jobs-handover-open",
            true,
            true,
            Some(
                "js-agent: the handover file is not used (group or others have permission on \
                 it (mode 0644)): the agent drains",
            ),
        ),
    ] {
        let dir = Scratch::new(name);
        let machine = FakeMachine::new();
        let worker = start(&dir, &machine);
        assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
        assert!(worker.handle().settle());
        let file = state(&dir).join(handover::HANDOVER_FILE);
        if write {
            handover::write(&state(&dir)).unwrap();
        }
        if said.is_some() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let root = state(&dir);
        let mut sleeper = Settling {
            handle: worker.handle(),
            calls: 0,
            act: Box::new(|_| Wake::Stop),
            budget: WakeBudget::new(),
        };
        let replies = vec![welcome(1), beat_ok()];
        let looked = look.then_some(root.as_path());
        let (exit, log, _) = driven(&worker, looked, replies, LONG, &mut sleeper);

        assert_eq!(exit, Exit::Stopped, "{name}");
        assert!(
            log.contains("draining: a run or a result is left"),
            "{name}: {log}"
        );
        assert!(!log.contains("handing over"), "{name}: {log}");
        if let Some(said) = said {
            assert!(log.contains(said), "{name}: {log}");
        }
    }
}

/// Asserts that a worker asked to park before it is closed, or while a start is under way,
/// goes on (busy), and parks once closed and quiet; a parked worker answers a settle and a
/// second park, and does nothing more. Catches a park that stops a worker still taking
/// starts, and a parked worker that steps again.
#[test]
fn a_worker_parks_only_closed_and_quiet_and_then_does_nothing_more() {
    use js_agent::Parked;
    let dir = Scratch::new("jobs-handover-park");
    let machine = FakeMachine::new();
    let image = job_start("r1").image;
    machine.world.begin_pull(&image);
    let worker = start(&dir, &machine);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());

    assert_eq!(worker.park(ANSWER), Parked::Busy, "open");
    worker.close();
    assert_eq!(worker.park(ANSWER), Parked::Busy, "a start under way");
    machine.world.end_pull(&image, false);
    assert!(worker.handle().settle());
    assert_eq!(worker.snapshot().jobs[0].state, HeldState::Running);
    assert_eq!(worker.park(ANSWER), Parked::Parked);

    machine.world.exit("r1", 0);
    assert!(worker.handle().settle());
    assert_eq!(worker.park(ANSWER), Parked::Parked);
    let snapshot = worker.snapshot();
    assert_eq!(
        snapshot.jobs[0].state,
        HeldState::Running,
        "no step after the park"
    );
    assert!(snapshot.jobs[0].result.is_none());
    assert!(!worker.stalled());
    assert_eq!(
        worker.try_deliver(js_agent_wire::HeartbeatReply::default()),
        Delivery::Delivered
    );
    assert!(worker.handle().settle());
    assert!(
        worker.snapshot().jobs[0].result.is_none(),
        "a reply makes no step either"
    );
}

/// Asserts that a stop asked to hand over by an agent whose job stop is under way (the
/// server's stop for the job came with the heartbeat) finishes that stop first: the job has
/// ended and been checked, its result kept for the next process to send, before the other job
/// is handed over. Catches a handover that leaves a
/// stop half done, which the next process would not finish.
#[test]
fn a_stop_the_server_asked_for_is_finished_before_the_handover() {
    let dir = Scratch::new("jobs-handover-stopping");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1", "r2"]);
    let stop = reply(
        200,
        json!({ "commands": [{ "type": "job_stop", "job_id": "r1" }] }),
    );
    let replies = vec![welcome(1), stop, beat_ok()];
    let (exit, log, sent) = handed(&dir, &worker, replies, LONG, |_| Wake::Stop);

    assert_eq!(exit, Exit::Stopped);
    assert!(log.contains("js-agent: handed over"), "{log}");
    assert!(machine.world.log().contains(&"stop r1".to_owned()));
    assert!(!machine.world.log().contains(&"stop r2".to_owned()));
    assert_eq!(machine.world.alive(), 1, "r2 runs on");
    assert!(
        results(&sent).is_empty(),
        "parked at the heartbeat the stop came with"
    );
    let next = next_process(&dir, &machine, worker);
    let snapshot = next.snapshot();
    let ended: Vec<&str> = snapshot
        .jobs
        .iter()
        .filter_map(|job| job.result.as_ref())
        .map(|result| result.job_id.as_str())
        .collect();
    assert_eq!(ended, ["r1"]);
    let held: Vec<&str> = snapshot
        .jobs
        .iter()
        .map(|job| job.job_id.as_str())
        .collect();
    assert!(held.contains(&"r2"), "{held:?}");
}

/// Asserts the ends a handover shares with a drain: a server that refuses the agent for good
/// during the handover ends it with that refusal (the runs are then stopped, as at any refusal
/// for good: no heartbeat can carry them any more), and a park asked of a worker whose thread
/// has ended answers so. Catches a handover that goes on against a server that will not have
/// the agent, and a park that waits for ever on a dead worker.
#[test]
fn a_refusal_during_the_handover_ends_it_and_a_dead_worker_parks_as_ended() {
    let dir = Scratch::new("jobs-handover-refused");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1"]);
    let replies = vec![welcome(1), refused(401, "unauthorised")];
    let (exit, _, _) = handed(&dir, &worker, replies, LONG, |_| Wake::Stop);
    let Exit::Fatal(fatal) = exit else {
        panic!("the server refused the agent: {exit:?}");
    };
    assert_eq!(fatal.status, 401);

    let dir = Scratch::new("jobs-handover-dead");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    machine.world.panic_at("create");
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(!worker.handle().settle(), "the worker died in the create");
    assert_eq!(worker.park(ANSWER), js_agent::Parked::Ended);
}

/// Asserts that a park never holds the caller on a worker stuck in a slow call: with the worker
/// inside a create that does not return, a park answers busy once its wait is over, and so
/// does one asked while the worker's queue is full; once the create returns, the worker takes
/// the parks that waited and parks, and the next ask finds it parked. Catches a handover that
/// stops heartbeating, and stops counting its bound, behind a wedged worker.
#[test]
fn a_park_asked_of_a_worker_stuck_in_a_call_answers_busy_and_parks_once_it_is_out() {
    use js_agent::Parked;
    let dir = Scratch::new("jobs-handover-stuck");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    machine.world.hold_creates();
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    machine.world.wait_entered(1);
    worker.close();
    // The worker is inside the create: no answer comes, whatever the wait.
    assert_eq!(worker.park(Duration::from_millis(1)), Parked::Busy);
    let reply = js_agent_wire::HeartbeatReply::default;
    while worker.try_deliver(reply()) == Delivery::Delivered {}
    assert_eq!(worker.try_deliver(reply()), Delivery::Full);
    assert_eq!(worker.park(ANSWER), Parked::Busy, "a full queue");
    machine.world.release();
    assert!(worker.handle().settle());
    assert_eq!(worker.snapshot().jobs[0].state, HeldState::Running);
    assert_eq!(worker.park(ANSWER), Parked::Parked);
}

mod in_place;
