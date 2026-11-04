//! A restart over a real `crun`: a runtime opened over the store of one that is not used again
//! adopts the runs that still stand (their supervisor and container outlive the runtime that
//! started them) and removes the rest of that agent's leases. Every test is `#[ignore]`, as
//! the suite's are.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use js_agent::crun::{Hooks, Reason};
use js_agent::lifecycle::{Clock, Lifecycle, NoChecks, RunSpec, Runtime as _, Status, label};
use js_agent_wire::{Command, HeartbeatReply, HeldJob, HeldState, LogTail, Outcome};
use serde_json::Value;

use crate::runtime_suite::{ERR_LINE, OUT_LINE, Program, Subject, spec_of};
use crate::subject::{CrunSubject, HOST, crun_subject};

/// A lifecycle clock that does not move until a test sets it.
#[derive(Clone, Copy, Default)]
struct Still;

impl Clock for Still {
    fn now_ms(&self) -> u64 {
        0
    }
}

/// The run `run_id` of `program` as a job start asks for it: the suite's run, keyed by job and
/// with the labels of a booking (the allocation `alloc-<run id>`, the wall-time limit
/// `wall_ms`), so that a restarted runtime can adopt it.
pub fn labelled(s: &mut dyn Subject, run_id: &str, program: Program, wall_ms: u64) -> RunSpec {
    let mut spec = spec_of(s, run_id, program);
    let own = [
        (label::KEYING, "job".to_owned()),
        (label::LEASE, format!("alloc-{run_id}")),
        (label::CPU_MILLIS, spec.cpu_millis.to_string()),
        (label::MEMORY_BYTES, spec.memory_bytes.to_string()),
        (label::DISK_BYTES, "0".to_owned()),
        (label::GPUS, String::new()),
        (label::SLOT, spec.slot.to_string()),
        (label::WALL_MS, wall_ms.to_string()),
    ];
    spec.labels
        .extend(own.map(|(key, value)| (key.to_owned(), value)));
    spec
}

/// Starts the labelled run `run_id` of `program` on the subject's own runtime and returns once
/// it is up.
fn start(s: &mut CrunSubject, run_id: &str, program: Program, wall_ms: u64) -> String {
    s.prepared();
    s.before(program);
    let spec = labelled(s, run_id, program, wall_ms);
    let started = s.runtime().borrow_mut().create_and_start(&spec);
    let id = started.expect("create and start");
    s.after(run_id, program);
    id.0
}

/// Section 6.7 step 6 of the design: the jobs the server holds as running that the host does
/// not list in its first beat are lost.
fn lost(server_running: &[&str], listed: &[HeldJob]) -> Vec<String> {
    server_running
        .iter()
        .filter(|id| listed.iter().all(|job| job.job_id != **id))
        .map(|id| (*id).to_owned())
        .collect()
}

/// A lifecycle over `runtime`, on the subject's directories, whose stops give a container
/// `grace`.
fn lifecycle(
    s: &mut CrunSubject,
    runtime: js_agent::crun::CrunRuntime,
    clock: impl Clock + 'static,
    grace: Duration,
) -> Lifecycle {
    let mut config = s.config();
    config.stop_grace = grace;
    let adopted = Lifecycle::adopt(config, Box::new(runtime), Box::new(NoChecks), Box::new(clock));
    adopted.expect("a lifecycle over the runtime").0
}

/// Catches a restart that loses a job that runs: a run that runs when its runtime is left is
/// adopted by the runtime that opens next (it is reported adopted, listed with its labels, and
/// polls as running), its lease stays, and a lifecycle over that runtime lists it as a running
/// job, which a stop then ends.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_restart_adopts_a_live_job_and_lists_it() {
    let mut s = crun_subject("adopt_live");
    let name = start(&mut s, "r-up", Program::Hold, 600_000);

    let mut second = s.open_as(HOST);
    let swept = second.swept();
    assert_eq!(swept.adopted, ["r-up"], "{swept:?}");
    assert!(
        swept.removed.is_empty() && swept.failed.is_empty() && swept.left.is_empty(),
        "{swept:?}"
    );
    let found = second.list_labelled().expect("list");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id.0, name);
    assert!(!found[0].never_started);
    assert_eq!(found[0].labels[label::RUN], "r-up");
    assert_eq!(found[0].labels[label::LEASE], "alloc-r-up");
    assert_eq!(found[0].labels[label::AGENT], HOST);
    assert_eq!(second.poll(&found[0].id), Ok(Status::Running));
    assert!(s.lease_dir(&name).is_dir() && s.sidecar(&name).is_file());
    assert!(s.lease_cgroup(&name).is_dir());

    let mut life = lifecycle(&mut s, second, Still, Duration::ZERO);
    let running = HeldJob {
        job_id: "r-up".to_owned(),
        state: HeldState::Running,
        result: None,
    };
    assert_eq!(life.held_jobs(), [running]);
    let reply = HeartbeatReply {
        commands: vec![Command::JobStop {
            job_id: "r-up".to_owned(),
        }],
        ..HeartbeatReply::default()
    };
    life.apply(&reply);
    life.step();
    let result = life.held_jobs()[0].result.clone().expect("a result");
    assert_eq!(result.outcome, Outcome::Killed);
}

/// Catches an exit lost with the agent that should have read it: two runs end while no agent
/// runs (one with a status, one after it wrote to each stream), and the runtime that opens
/// next adopts both with what they did, the status of the first and the tail of the second,
/// read from the records their supervisors left.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn an_exit_while_no_agent_ran_is_reported_with_its_code_and_tail() {
    let mut s = crun_subject("adopt_exited");
    s.prepared();
    let mut names = Vec::new();
    for (run, program) in [
        ("r-code", Program::Exit(7)),
        ("r-say", Program::Say { hold: false }),
    ] {
        let spec = labelled(&mut s, run, program, 600_000);
        let started = s.runtime().borrow_mut().create_and_start(&spec);
        let id = started.expect("create and start");
        s.settled(&id);
        names.push(id);
    }

    let mut second = s.open_as(HOST);
    let mut adopted = second.swept().adopted.clone();
    adopted.sort();
    assert_eq!(adopted, ["r-code", "r-say"], "{:?}", second.swept());
    let code = second.poll(&names[0]).expect("poll");
    let Status::Exited(exit) = code else {
        panic!("expected an exit, found {code:?}");
    };
    assert_eq!((exit.code, exit.oom_killed), (7, false));
    let said = second.poll(&names[1]).expect("poll");
    let Status::Exited(exit) = said else {
        panic!("expected an exit, found {said:?}");
    };
    assert_eq!(exit.code, 0);
    let tail = LogTail {
        stdout: OUT_LINE.to_owned(),
        stderr: ERR_LINE.to_owned(),
        truncated: false,
    };
    assert_eq!(second.log_tail(&names[1]), Ok(Some(tail)));
    for id in &names {
        second.remove(id).expect("remove");
        assert!(!s.lease_dir(&id.0).exists() && !s.sidecar(&id.0).exists());
    }
}

/// Catches a job taken over on a record that is not there: a run that runs whose sidecar is
/// gone is removed by the runtime that opens next (container, cgroup and directory), reported
/// removed because nothing records it, and not listed; a lifecycle over the runtime lists no
/// job, so the server's step 6 marks the job lost.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_job_with_no_record_is_removed_and_the_server_is_told_it_is_gone() {
    let mut s = crun_subject("adopt_no_record");
    let name = start(&mut s, "r-up", Program::Hold, 600_000);
    std::fs::remove_file(s.sidecar(&name)).expect("lose the sidecar");

    let mut second = s.open_as(HOST);
    let swept = second.swept();
    assert!(swept.adopted.is_empty(), "{swept:?}");
    assert_eq!(swept.removed.len(), 1, "{swept:?}");
    assert_eq!(swept.removed[0].run_id, "r-up");
    assert_eq!(swept.removed[0].reason, Reason::NoRecord);
    assert!(swept.failed.is_empty(), "{swept:?}");
    assert!(!s.lease_dir(&name).exists() && !s.lease_cgroup(&name).exists());
    assert!(second.list_labelled().expect("list").is_empty());

    let life = lifecycle(&mut s, second, Still, Duration::ZERO);
    let listed = life.held_jobs();
    assert!(listed.is_empty(), "{listed:?}");
    assert_eq!(lost(&["r-up"], &listed), ["r-up"]);
}

/// The wall clock of the runtime of [`an_adopted_job_still_stops_at_its_wall_limit`].
static WALL: AtomicU64 = AtomicU64::new(0);

fn wall() -> u64 {
    WALL.load(Ordering::SeqCst)
}

/// A lifecycle clock the test advances.
#[derive(Clone, Default)]
struct Moving(std::sync::Arc<AtomicU64>);

impl Clock for Moving {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Catches an adopted job that runs past its wall-time limit: a job with two seconds that ran
/// one under its predecessor (the start is read from the sidecar, the clock of the runtime that
/// adopts it is set one second after) is still running when the lifecycle's clock has advanced
/// 0.9 s, and is stopped (SIGTERM, which it ignores, then the lease cgroup's kill with no
/// grace) and reported timed out 0.2 s after.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn an_adopted_job_still_stops_at_its_wall_limit() {
    let mut s = crun_subject("adopt_wall");
    let name = start(&mut s, "r-up", Program::IgnoreTerm, 2_000);
    let text = std::fs::read_to_string(s.sidecar(&name)).expect("the sidecar");
    let record: Value = serde_json::from_str(&text).expect("json");
    let started = record["started_wall_ms"].as_u64().expect("a start");
    WALL.store(started + 1_000, Ordering::SeqCst);
    let hooks = Hooks {
        now_ms: wall,
        ..Hooks::default()
    };

    let second = s.open_with_hooks(HOST, hooks);
    assert_eq!(second.swept().adopted, ["r-up"], "{:?}", second.swept());
    let clock = Moving::default();
    let mut life = lifecycle(&mut s, second, clock.clone(), Duration::ZERO);
    clock.0.fetch_add(900, Ordering::SeqCst);
    life.step();
    assert!(life.held_jobs()[0].result.is_none(), "stopped early");
    clock.0.fetch_add(200, Ordering::SeqCst);
    life.step();
    let result = life.held_jobs()[0].result.clone().expect("a result");
    assert_eq!(result.outcome, Outcome::TimedOut);
}
