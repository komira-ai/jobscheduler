//! A restart over the stand-in `crun`: the runs a dead agent left are adopted when they
//! still stand, and removed, with their reason, when they do not. The container of a run is
//! run by a supervisor process (the agent binary in its helper mode), so it outlives the
//! runtime that started it: the test drops that runtime and opens another.
//!
//! The stand-in cannot make a real cgroup, so a test puts the pid its `crun state` names
//! into the lease's cgroup itself ([`Node::place`]), as `crun` does with the container's init.
//! The wall clock of a runtime is a function a test sets; the lifecycle's own clock is one it
//! advances. Nothing waits for time.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use js_agent::crun::{CrunRuntime, EgressPort, Hooks, Reason, Wired};
use js_agent::egress::EgressError;
use js_agent::lifecycle::{
    Capacity, Clock, Config, DirLayout, Event, Exit, Lifecycle, MountRoots, NoChecks, RunHandle,
    RunSpec, Runtime, ScratchRoot, StateRoot, Status, label,
};
use js_agent_wire::{Command, HeartbeatReply, HeldJob, HeldState, LogTail, Outcome};
use js_contain::oci::SupervisorId;
use serde_json::{Value, json};

use crate::node::{LONG, NoImages, Node, PID};

/// A lifecycle clock a test moves. Clones share one reading.
#[derive(Clone, Default)]
struct Moving(Arc<AtomicU64>);

impl Moving {
    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for Moving {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A lifecycle over `runtime` on `node`, whose stops give a container `stop_grace`; the
/// events of its adoption come with it.
fn lifecycle(
    node: &Node,
    runtime: CrunRuntime,
    clock: &Moving,
    stop_grace: Duration,
) -> (Lifecycle, Vec<Event>) {
    let private = |name: &str| {
        let dir = node.dir.join(name);
        std::fs::create_dir_all(&dir).expect("a directory");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
        dir
    };
    let config = Config {
        capacity: Capacity {
            cpu_millis: 4000,
            memory_bytes: 8 << 30,
            disk_bytes: 10 << 30,
            gpus: Default::default(),
        },
        allow_host_jobs: false,
        mounts: MountRoots::new(&[]).expect("no mount roots"),
        scratch: ScratchRoot::open_with(&private("life-scratch"), DirLayout::Private)
            .expect("a scratch root"),
        files: None,
        state: StateRoot::open(&private("life-state")).expect("a state root"),
        stop_grace,
        bundle: None,
        quota: None,
    };
    let clock = Box::new(clock.clone());
    Lifecycle::adopt(config, Box::new(runtime), Box::new(NoChecks), clock).expect("a lifecycle")
}

/// Applies one command to `life`.
fn command(life: &mut Lifecycle, command: Command) -> Vec<Event> {
    let reply = HeartbeatReply {
        commands: vec![command],
        ..HeartbeatReply::default()
    };
    life.apply(&reply)
}

/// Stages `spec`, starts it and waits for its script to say it is up (the pipe it opens).
fn started(node: &Node, runtime: &mut CrunRuntime, spec: &RunSpec) -> RunHandle {
    let id = runtime.stage(spec).expect("staged");
    node.pipe(&id.0);
    runtime.start_staged(&id).expect("started");
    node.up(&id);
    id
}

/// [`started`] for the job `run`, with its init in its cgroup: a run a restart can adopt.
fn held(node: &Node, runtime: &mut CrunRuntime, run: &str) -> RunHandle {
    let id = started(node, runtime, &node.job(run, 600_000));
    node.place(&id.0, PID);
    id
}

/// The sidecar of the lease `name` as JSON.
fn sidecar_json(node: &Node, name: &str) -> Value {
    let text = std::fs::read_to_string(node.sidecar(name)).expect("the sidecar");
    serde_json::from_str(&text).expect("json")
}

/// Changes the sidecar of the lease `name` with `change`.
fn edit(node: &Node, name: &str, change: impl FnOnce(&mut Value)) {
    let mut json = sidecar_json(node, name);
    change(&mut json);
    std::fs::write(node.sidecar(name), json.to_string()).expect("write the sidecar");
}

/// Removes the directory tree `dir`.
fn remove(dir: &Path) {
    std::fs::remove_dir_all(dir).expect("remove the tree");
}

/// Makes the directory `dir`.
fn make_dir(dir: &Path) {
    std::fs::create_dir(dir).expect("make the directory");
}

/// Removes the field `key` of the sidecar `json`.
fn without(json: &mut Value, key: &str) {
    if let Some(object) = json.as_object_mut() {
        object.remove(key);
    }
}

/// Asserts nothing is left of the lease `name`.
fn assert_gone(node: &Node, name: &str) {
    let left = left_of(node, name);
    assert!(left.is_empty(), "{name}: {left:?}");
}

/// What is left of the lease `name`: its directory, the container's state, its cgroup and its
/// sidecar.
fn left_of(node: &Node, name: &str) -> Vec<&'static str> {
    let lease = node.lease_of(name);
    [
        ("directory", lease.clone()),
        ("container state", lease.join("crun")),
        ("cgroup", node.cgroup_of(name)),
        ("sidecar", node.sidecar(name)),
    ]
    .into_iter()
    .filter(|(_, path)| path.exists())
    .map(|(what, _)| what)
    .collect()
}

/// Whether `reason` is the one `want` names: `@old` and `@never` the reasons that carry no
/// text, anything else a word in the text of an inconsistency.
fn says(reason: &Reason, want: &str) -> bool {
    match reason {
        Reason::OldForm => want == "@old",
        Reason::NeverRan => want == "@never",
        Reason::Inconsistent(why) => why.contains(want),
        Reason::NoRecord => false,
    }
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

/// Proves a restart adopts a job that still runs: after the runtime that started it is
/// dropped (its supervisor and the stand-in survive), a runtime of the same agent lists it
/// with its labels and the time it has run on the wall clock (the start the sidecar recorded
/// to the clock now), reports it adopted, leaves its lease in place, and a lifecycle over it
/// lists it as a running job that a stop then ends. The sidecar of the running job says so:
/// its state, its start and its supervisor. Catches a restart that removes a running job, one
/// that lists it with no age or an age from its own start, and a sidecar that does not say a
/// job runs.
#[test]
fn a_restart_adopts_a_live_job_and_lists_it() {
    static NOW: AtomicU64 = AtomicU64::new(0);
    fn now() -> u64 {
        NOW.load(Ordering::SeqCst)
    }
    let node = Node::new("adopt-live");
    let hooks = Hooks {
        now_ms: now,
        ..Node::hooks(LONG, LONG)
    };
    NOW.store(100_000, Ordering::SeqCst);
    let mut first = node.open_with(hooks, Arc::new(NoImages));
    let id = held(&node, &mut first, "r1-hold");
    let record = sidecar_json(&node, &id.0);
    assert_eq!(record["version"], 2);
    assert_eq!(record["state"], "running");
    assert_eq!(record["started_wall_ms"], 100_000);
    let pid = record["supervisor"]["pid"].as_u64();
    assert!(pid.is_some_and(|pid| pid > 0), "{record}");
    assert_eq!(record["allocation"], "lease-r1-hold");
    NOW.store(107_500, Ordering::SeqCst);
    drop(first);

    let mut second = node.open_with(hooks, Arc::new(NoImages));
    let report = second.swept();
    assert_eq!(report.adopted, ["r1-hold"]);
    assert!(
        report.removed.is_empty() && report.failed.is_empty() && report.left.is_empty(),
        "{report:?}"
    );
    let found = second.list_labelled().expect("lists");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, id);
    assert_eq!(found[0].running_ms, 7_500);
    assert!(!found[0].never_started);
    assert_eq!(found[0].labels[label::RUN], "r1-hold");
    assert_eq!(found[0].labels[label::LEASE], "lease-r1-hold");
    assert_eq!(found[0].labels[label::AGENT], "node-1");
    assert_eq!(second.poll(&id), Ok(Status::Running));
    assert_eq!(left_of(&node, &id.0).len(), 4);

    let clock = Moving::default();
    let (mut life, events) = lifecycle(&node, second, &clock, LONG);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Adopted { run_id } if run_id == "r1-hold")),
        "{events:?}"
    );
    let listed = HeldJob {
        job_id: "r1-hold".to_owned(),
        state: HeldState::Running,
        result: None,
    };
    assert_eq!(life.held_jobs(), [listed]);
    command(
        &mut life,
        Command::JobStop {
            job_id: "r1-hold".to_owned(),
        },
    );
    life.step();
    let result = life.held_jobs()[0].result.clone().expect("a result");
    assert_eq!(result.outcome, Outcome::Killed);
}

/// The wall clock of the runtimes of [`an_adopted_job_still_stops_at_its_wall_limit`].
static WALL: AtomicU64 = AtomicU64::new(0);

fn wall() -> u64 {
    WALL.load(Ordering::SeqCst)
}

/// A node whose job `r1-stubborn`, with a wall-time limit of ten seconds, was started by a
/// runtime at the wall time `start` and is adopted by another at `restart`, under a lifecycle
/// whose clock reads 0 and whose stops give no grace; the node, the lifecycle and the clock.
fn restarted(name: &str, (start, restart): (u64, u64)) -> (Node, Lifecycle, Moving) {
    let node = Node::new(name);
    let hooks = Hooks {
        now_ms: wall,
        ..Node::hooks(LONG, LONG)
    };
    WALL.store(start, Ordering::SeqCst);
    let mut first = node.open_with(hooks, Arc::new(NoImages));
    let id = started(&node, &mut first, &node.job("r1-stubborn", 10_000));
    node.place(&id.0, PID);
    drop(first);
    WALL.store(restart, Ordering::SeqCst);
    let second = node.open_with(hooks, Arc::new(NoImages));
    assert_eq!(
        second.swept().adopted,
        ["r1-stubborn"],
        "{:?}",
        second.swept()
    );
    let clock = Moving::default();
    let (life, _) = lifecycle(&node, second, &clock, Duration::ZERO);
    (node, life, clock)
}

/// Whether the one job `life` holds has ended as a job past its wall-time limit.
fn timed_out(life: &Lifecycle) -> bool {
    let jobs = life.held_jobs();
    jobs.len() == 1
        && jobs[0]
            .result
            .as_ref()
            .is_some_and(|result| result.outcome == Outcome::TimedOut)
}

/// Proves an adopted job is held to the rest of its wall-time limit, which runs only while an
/// agent runs: a job with ten seconds that ran four under its predecessor (on the wall clock)
/// is still running when the lifecycle's clock has advanced 5.9 s, and is stopped (SIGTERM,
/// which it ignores, then the lease cgroup's kill with no grace) and reported timed out
/// 0.2 s after; a job whose time ran out while no agent ran is stopped at the first step;
/// and a wall clock that stepped back an hour between the agents gives it at most its whole
/// limit from the adoption, not more. Catches an adopted job with no limit or with the whole
/// limit again, one that restarts its clock at the adoption, and a limit that grows when the
/// wall clock is set back.
#[test]
fn an_adopted_job_still_stops_at_its_wall_limit() {
    let (_node, mut life, clock) = restarted("adopt-wall-ran", (1_000_000, 1_004_000));
    clock.advance(5_900);
    life.step();
    assert!(life.held_jobs()[0].result.is_none(), "stopped early");
    assert_eq!(life.held_jobs()[0].state, HeldState::Running);
    clock.advance(200);
    life.step();
    assert!(timed_out(&life), "{:?}", life.held_jobs());

    let (_node, mut life, _clock) = restarted("adopt-wall-past", (1_000_000, 1_012_000));
    life.step();
    assert!(timed_out(&life), "{:?}", life.held_jobs());

    let (_node, mut life, clock) = restarted("adopt-wall-back", (10_000_000, 6_400_000));
    clock.advance(9_900);
    life.step();
    assert!(life.held_jobs()[0].result.is_none(), "stopped early");
    clock.advance(200);
    life.step();
    assert!(timed_out(&life), "{:?}", life.held_jobs());
}

/// Proves a job that ended while no agent ran is adopted with its end: the exit code and the
/// tail of what it wrote are read from the record its supervisor left, the job lists as
/// running for no time, and it is removed with its lease like any other. Catches an exit
/// lost with the agent that should have read it, and a tail that comes back empty.
#[test]
fn a_job_that_exited_while_no_agent_ran_is_reported_with_its_code_and_tail() {
    let node = Node::new("adopt-exited");
    let mut first = node.open();
    let id = first
        .create_and_start(&node.job("r1-sayexit", 60_000))
        .expect("started");
    first.wait_ended(&id);
    drop(first);

    let mut second = node.open();
    assert_eq!(
        second.swept().adopted,
        ["r1-sayexit"],
        "{:?}",
        second.swept()
    );
    assert!(second.swept().removed.is_empty());
    let found = second.list_labelled().expect("lists");
    assert_eq!((found.len(), found[0].running_ms), (1, 0));
    let want = Exit {
        code: 5,
        oom_killed: false,
    };
    assert_eq!(second.poll(&id), Ok(Status::Exited(want)));
    let tail = LogTail {
        stdout: "to-stdout\n".to_owned(),
        stderr: "to-stderr\n".to_owned(),
        truncated: false,
    };
    assert_eq!(second.log_tail(&id), Ok(Some(tail)));
    second.remove(&id).expect("removed");
    assert_gone(&node, &id.0);
}

/// Proves a job the host cannot vouch for is removed, with its reason, and the server finds
/// out: a lease with no sidecar, a sidecar of version 1, a lease whose cgroup is gone, one
/// whose init is not in its cgroup, and one whose labels disagree with its record are each
/// torn down (container state, cgroup, lease directory, sidecar), reported removed with
/// their reason, and not listed; a lifecycle over the runtime then lists none of them (the
/// first heartbeat lists only the job that stands), so the server's step 6 marks each of them
/// lost, and not the one that stands. Catches a job taken over on a record it cannot back, a
/// lease left behind, and a restart that tells the server a lost job runs.
#[test]
fn a_job_with_no_record_is_removed_and_the_server_is_told_it_is_gone() {
    let node = Node::new("adopt-no-record");
    let mut first = node.open();
    let unrecorded = first
        .stage(&node.job("r1-nosidecar", 60_000))
        .expect("staged");
    std::fs::remove_file(node.sidecar(&unrecorded.0)).expect("lose the sidecar");
    let old = held(&node, &mut first, "r2-old-hold");
    edit(&node, &old.0, |json| json["version"] = json!(1));
    let no_cgroup = held(&node, &mut first, "r3-nocgroup-hold");
    std::fs::remove_dir_all(node.cgroup_of(&no_cgroup.0)).expect("lose the cgroup");
    let no_init = started(&node, &mut first, &node.job("r4-noinit-hold", 60_000));
    let relabelled = held(&node, &mut first, "r5-labels-hold");
    edit(&node, &relabelled.0, |json| {
        json["labels"][label::LEASE] = json!("another-lease");
    });
    let standing = held(&node, &mut first, "r6-hold");
    drop(first);

    let mut second = node.open();
    let report = second.swept().clone();
    assert_eq!(report.adopted, ["r6-hold"], "{report:?}");
    assert!(
        report.failed.is_empty() && report.left.is_empty(),
        "{report:?}"
    );
    let reasons: BTreeMap<&str, &Reason> = report
        .removed
        .iter()
        .map(|lease| (lease.run_id.as_str(), &lease.reason))
        .collect();
    assert_eq!(reasons.len(), 5, "{report:?}");
    assert_eq!(reasons["r1-nosidecar"], &Reason::NoRecord);
    assert_eq!(reasons["r2-old-hold"], &Reason::OldForm);
    assert!(says(reasons["r3-nocgroup-hold"], "the cgroup is gone"));
    assert!(says(reasons["r4-noinit-hold"], "not in the lease's cgroup"));
    assert!(says(reasons["r5-labels-hold"], "disagrees"));
    for id in [&unrecorded, &old, &no_cgroup, &no_init, &relabelled] {
        assert_gone(&node, &id.0);
    }
    let listed: Vec<_> = second
        .list_labelled()
        .expect("lists")
        .into_iter()
        .map(|found| found.id)
        .collect();
    assert_eq!(listed, [standing.clone()]);

    let clock = Moving::default();
    let (mut life, _) = lifecycle(&node, second, &clock, LONG);
    let jobs = life.held_jobs();
    let running = HeldJob {
        job_id: "r6-hold".to_owned(),
        state: HeldState::Running,
        result: None,
    };
    assert_eq!(jobs, [running]);
    assert!(life.held().is_empty());
    let server = [
        "r1-nosidecar",
        "r2-old-hold",
        "r3-nocgroup-hold",
        "r4-noinit-hold",
        "r5-labels-hold",
        "r6-hold",
    ];
    assert_eq!(lost(&server, &jobs), server[..5].to_vec());
    command(
        &mut life,
        Command::JobStop {
            job_id: "r6-hold".to_owned(),
        },
    );
    life.step();
}

/// Proves each check of an adoption can fail by itself, and fails the lease with its own
/// words: of fourteen jobs that run, the one left as it is stands, and each of the others has
/// one thing wrong (a sidecar of version 1, one still starting, a field missing, a label that
/// disagrees, a path that is not the one the agent makes for the lease: directory, cgroup,
/// exit record, scratch directory; a cgroup and a scratch directory that are gone, an exit
/// record that cannot be read, a supervisor that is gone, an init that is not in the
/// cgroup), is removed saying which; a lease made and never started
/// is removed as one that never ran. Catches a check that is not made, or that is made and
/// answers for another.
#[test]
fn every_check_of_an_adoption_can_fail_and_the_lease_is_removed_with_its_reason() {
    type Tamper = fn(&Node, &str, &str);
    let node = Node::new("adopt-checks");
    let cases: [(&str, Tamper, &str); 14] = [
        ("a-ok-hold", |_, _, _| {}, "adopted"),
        (
            "b-old-hold",
            |node, name, _| edit(node, name, |json| json["version"] = json!(1)),
            "@old",
        ),
        (
            "c-starting-hold",
            |node, name, _| edit(node, name, |json| json["state"] = json!("starting")),
            "@never",
        ),
        (
            "d-nosuper-hold",
            |node, name, _| edit(node, name, |json| without(json, "supervisor")),
            "its supervisor is not recorded",
        ),
        (
            "e-label-hold",
            |node, name, _| edit(node, name, |json| json["labels"][label::SLOT] = json!("9")),
            "disagrees with the record",
        ),
        (
            "f-lease-hold",
            |node, name, _| edit(node, name, |json| json["lease_dir"] = json!("/elsewhere")),
            "lease directory is not the one",
        ),
        (
            "g-cgroup-hold",
            |node, name, _| edit(node, name, |json| json["cgroup"] = json!("/elsewhere")),
            "the cgroup is not the one",
        ),
        (
            "h-record-hold",
            |node, name, _| edit(node, name, |json| json["exit_record"] = json!("/elsewhere")),
            "exit record is not the one",
        ),
        (
            "i-scratch-hold",
            |node, name, _| {
                let other = node.dir.join("runs/other");
                edit(node, name, |json| json["scratch"] = json!(other));
            },
            "scratch directory is not the one",
        ),
        (
            "j-nocg-hold",
            |node, name, _| remove(&node.cgroup_of(name)),
            "the cgroup is gone or is a link",
        ),
        (
            "k-noscratch-hold",
            |node, _, run| remove(&node.dir.join("runs").join(run)),
            "the scratch directory is gone or is a link",
        ),
        (
            "l-badrec-hold",
            |node, name, _| make_dir(&node.lease_of(name).join("exit.json")),
            "the exit record:",
        ),
        (
            "m-killed-hold",
            |node, name, _| end_supervisor(node, name),
            "supervisor has ended",
        ),
        ("p-unplaced-hold", |_, _, _| {}, "not in the lease's cgroup"),
    ];
    let mut first = node.open();
    let mut handles = Vec::new();
    for (run, tamper, _) in &cases {
        let id = if run.starts_with("p-") {
            started(&node, &mut first, &node.job(run, 60_000))
        } else {
            held(&node, &mut first, run)
        };
        tamper(&node, &id.0, run);
        handles.push(id);
    }
    let never = first
        .stage(&node.job("z-staged-hold", 60_000))
        .expect("staged");
    drop(first);

    let mut second = node.open();
    let report = second.swept().clone();
    assert_eq!(report.adopted, ["a-ok-hold"], "{report:?}");
    assert!(
        report.failed.is_empty() && report.left.is_empty(),
        "{report:?}"
    );
    for (run, _, want) in cases.iter().skip(1) {
        let lease = report.removed.iter().find(|lease| lease.run_id == *run);
        let lease = lease.unwrap_or_else(|| panic!("{run} was not removed: {report:?}"));
        assert!(says(&lease.reason, want), "{run}: {:?}", lease.reason);
    }
    let lease = report.removed.iter().find(|lease| lease.name == never.0);
    assert_eq!(lease.map(|lease| &lease.reason), Some(&Reason::NeverRan));
    assert_eq!(report.removed.len(), cases.len(), "{report:?}");
    for id in handles.iter().skip(1).chain([&never]) {
        assert_gone(&node, &id.0);
    }

    second.stop(&handles[0], LONG).expect("stopped");
    second.remove(&handles[0]).expect("removed");
    assert_gone(&node, &handles[0].0);
}

/// Ends the supervisor of the lease `name` by force, as a crash of the host's process would,
/// and returns once it is gone.
fn end_supervisor(node: &Node, name: &str) {
    let id = supervisor_id(node, name);
    let group = rustix::process::Pid::from_raw(i32::try_from(id.pid).expect("a pid"));
    rustix::process::kill_process_group(group.expect("a process"), rustix::process::Signal::KILL)
        .expect("kill");
    wait_gone(id);
}

/// The supervisor the sidecar of the lease `name` names.
fn supervisor_id(node: &Node, name: &str) -> SupervisorId {
    let record = sidecar_json(node, name);
    let pid = record["supervisor"]["pid"].as_u64().expect("a pid");
    let start = record["supervisor"]["start"].as_u64().expect("a start");
    SupervisorId {
        pid: u32::try_from(pid).expect("a pid"),
        start,
    }
}

/// Returns once the process `id` is gone (it was sent SIGKILL, which takes a moment to act).
fn wait_gone(id: SupervisorId) {
    while id.alive() {
        std::thread::yield_now();
    }
}

/// An egress that is given and never asked: a runtime opened with one stands for an agent
/// with an allow-list.
#[derive(Debug)]
struct Unused;

impl EgressPort for Unused {
    fn check(&self) -> Result<(), EgressError> {
        Ok(())
    }

    fn attach(&self, _run: &str, _pid: u32) -> Result<Box<dyn Wired>, EgressError> {
        Err(EgressError::Exhausted)
    }
}

/// Proves a lease nothing records that cannot be removed is reported and kept for the next
/// start: with `crun delete` failing (a container that never ran has no state to delete, so
/// what fails is the removal of the lease's cgroup, whose files the delete leaves), the lease of
/// a run whose sidecar is gone is reported as failed with the step and stays; once it works the next start removes it, as one nothing
/// records. Catches a start that forgets a lease it could not remove and says nothing.
#[test]
fn a_lease_with_no_record_that_cannot_be_removed_is_reported_and_tried_again() {
    let node = Node::new("adopt-no-record-stuck");
    let mut dead = node.open();
    let id = dead.stage(&node.job("r1-exit", 60_000)).expect("staged");
    std::fs::remove_file(node.sidecar(&id.0)).expect("lose the sidecar");
    std::fs::write(node.dir.join("delete-fails"), "").expect("switch");

    let second = node.open();
    let report = second.swept();
    assert!(report.removed.is_empty(), "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].run_id, "r1-exit");
    assert!(
        report.failed[0].why.contains("remove the lease's cgroup"),
        "{report:?}"
    );
    assert!(node.lease_of(&id.0).is_dir());

    std::fs::remove_file(node.dir.join("delete-fails")).expect("switch off");
    let third = node.open();
    assert_eq!(third.swept().removed.len(), 1, "{:?}", third.swept());
    assert_eq!(third.swept().removed[0].reason, Reason::NoRecord);
    assert_gone(&node, &id.0);
}

/// Proves a start whose running record cannot be written is a failed start that leaves the
/// run to be removed, not a run a restart could not find: with a directory where the new
/// sidecar goes, the start fails saying it could not record the lease, the old sidecar (state
/// starting) is as it was, and the removal ends the run and leaves nothing. Catches a start
/// that reports success with a sidecar that says the run never started, and a failed record
/// that leaves a started container no removal can reach.
#[test]
fn a_start_whose_running_record_cannot_be_written_fails_and_is_removed() {
    let node = Node::new("adopt-unrecorded");
    let mut runtime = node.open();
    let id = runtime.stage(&node.job("r1-exit", 60_000)).expect("staged");
    let tmp = node.sidecar(&id.0).with_file_name(format!(".{}.tmp", id.0));
    std::fs::create_dir(tmp).expect("a directory");
    let why = runtime.start_staged(&id).expect_err("not recorded");
    assert!(
        why.to_string().contains("record the running lease"),
        "{why}"
    );
    assert_eq!(sidecar_json(&node, &id.0)["state"], "starting");
    runtime.remove(&id).expect("removed");
    assert_gone(&node, &id.0);
}

/// Proves a run that is ending when the agent restarts is adopted, not torn down: `crun` has
/// deleted the container (its `state` fails) while the supervisor still drains, or has not
/// made it yet, and the supervisor lives, so the run is adopted as running; the same `crun`
/// failure with the supervisor gone and no exit record is a lease removed saying the
/// supervisor ended. Catches an adoption that needs `crun state` to name an init and so
/// removes a job that is only ending, and one that adopts a run nothing runs.
#[test]
fn a_live_supervisor_with_no_container_named_is_adopted_and_a_dead_one_is_removed() {
    let node = Node::new("adopt-ending");
    let mut first = node.open();
    let mut ids = Vec::new();
    for run in ["r1-ending-deaf", "r2-gone-deaf"] {
        let id = first
            .create_and_start(&node.job(run, 60_000))
            .expect("started");
        ids.push(id);
    }
    end_supervisor(&node, &ids[1].0);
    drop(first);

    let mut second = node.open_impatient();
    let report = second.swept().clone();
    assert_eq!(report.adopted, ["r1-ending-deaf"], "{report:?}");
    assert_eq!(report.removed.len(), 1, "{report:?}");
    assert_eq!(report.removed[0].run_id, "r2-gone-deaf");
    assert!(
        says(&report.removed[0].reason, "supervisor has ended"),
        "{report:?}"
    );
    assert_gone(&node, &ids[1].0);
    assert_eq!(second.poll(&ids[0]), Ok(Status::Running));
    second.remove(&ids[0]).expect("removed");
    assert_gone(&node, &ids[0].0);
}

/// Proves a sidecar still in the state starting that names a live supervisor (the agent
/// died between starting the supervisor and recording the run) is removed as a run that
/// never ran, and its supervisor is ended with it. Catches a teardown that leaves the
/// supervisor, and with it the job, running with no lease.
#[test]
fn a_starting_lease_that_names_a_live_supervisor_is_removed_and_the_supervisor_ended() {
    let node = Node::new("adopt-starting-live");
    let mut first = node.open();
    let id = held(&node, &mut first, "r1-hold");
    let supervisor = supervisor_id(&node, &id.0);
    assert!(supervisor.alive());
    edit(&node, &id.0, |json| json["state"] = json!("starting"));
    drop(first);

    let second = node.open();
    let report = second.swept();
    assert!(report.adopted.is_empty(), "{report:?}");
    assert_eq!(report.removed.len(), 1, "{report:?}");
    assert_eq!(report.removed[0].reason, Reason::NeverRan);
    wait_gone(supervisor);
    assert_gone(&node, &id.0);
}

/// Proves a runtime opened with an egress adopts nothing: a job that runs is removed, saying
/// its network wiring cannot be taken over, and its supervisor is ended, because the egress
/// removes the wiring of an earlier process. Catches an adopted run left with no network.
#[test]
fn a_runtime_with_an_egress_removes_the_runs_it_would_adopt() {
    let node = Node::new("adopt-egress");
    let mut first = node.open();
    let id = held(&node, &mut first, "r1-hold");
    let supervisor = supervisor_id(&node, &id.0);
    drop(first);

    let mut settings = node.settings("/root");
    settings.egress = Some(Arc::new(Unused));
    let second = CrunRuntime::open_with(settings, Arc::new(NoImages), Node::hooks(LONG, LONG))
        .expect("opens");
    let report = second.swept();
    assert!(report.adopted.is_empty(), "{report:?}");
    assert_eq!(report.removed.len(), 1, "{report:?}");
    assert!(
        says(
            &report.removed[0].reason,
            "network wiring cannot be taken over"
        ),
        "{report:?}"
    );
    wait_gone(supervisor);
    assert_gone(&node, &id.0);
}

/// Proves a removal that cannot end the supervisor fails, naming it, and leaves the lease
/// for another try: the sidecar names a process that a group kill does not reach (a child of
/// the test, in the test's own group), so the forced end never completes within the wait;
/// the removal is an error naming the process and the lease stays; once the process is gone
/// the removal works. Catches a removal that waits for the supervisor without end.
#[test]
fn a_removal_that_cannot_end_the_supervisor_fails_and_can_be_tried_again() {
    let node = Node::new("adopt-remove-stuck");
    let mut first = node.open();
    let id = held(&node, &mut first, "r1-hold");
    end_supervisor(&node, &id.0);
    let mut child = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("a child");
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.id())).expect("its stat");
    let start: u64 = stat
        .rsplit_once(')')
        .and_then(|(_, after)| after.split_whitespace().nth(19))
        .and_then(|field| field.parse().ok())
        .expect("a start time");
    edit(&node, &id.0, |json| {
        json["supervisor"] = json!({ "pid": child.id(), "start": start });
    });
    drop(first);

    let mut second = node.open_with(Node::hooks(Duration::ZERO, LONG), Arc::new(NoImages));
    assert_eq!(second.swept().adopted, ["r1-hold"], "{:?}", second.swept());
    let why = second.remove(&id).expect_err("not ended");
    let text = why.to_string();
    assert!(
        text.contains("end the supervisor") && text.contains(&child.id().to_string()),
        "{text}"
    );
    assert!(node.lease_of(&id.0).is_dir());

    child.kill().expect("kill the child");
    child.wait().expect("reaped");
    second.remove(&id).expect("removed");
    assert_gone(&node, &id.0);
}
