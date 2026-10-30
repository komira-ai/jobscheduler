//! A native job through the worker, the thread that owns the lifecycle: the backend the
//! worker is made over is the native one, so its runtime is made over the helper, its
//! cleanup is the native checks, and it says it reports no load.

use std::fs;
use std::sync::Arc;

use js_agent::native::{NativeFlags, NativeSettings};
use js_agent::usage::Outbox;
use js_agent::{Backend, Delivery, Platform, Rescue, Snapshot, Worker};
use js_agent_wire::{Command, HeartbeatReply, HeldState, Outcome};

use super::fake_helper::FakeHelper;
use super::lifecycle::job;
use crate::jobs::fake::FakeMachine;
use crate::jobs::{STEP_NEVER, acks, plan};
use crate::support::Scratch;

/// Asserts the path of a native result through the worker: the start runs the program and
/// the run is held as running; the helper's report of its exit is found by the worker's
/// next step and published as a result with the exit code; the user is killed and deleted
/// and the scratch directory goes; an acknowledgement drops the run. It also asserts that a
/// worker over the native backend says at start that the heartbeat carries no load. It
/// catches a native agent whose worker keeps a result from the server, or leaves the user.
#[test]
fn a_native_result_stays_published_until_it_is_acknowledged() {
    let dir = Scratch::new("native-worker");
    let fake = FakeHelper::start(&dir);
    let machine = FakeMachine::new();
    let mut plan = plan(&dir, &machine, STEP_NEVER);
    let (state, scratch) = (
        fs::canonicalize(dir.path("state")).unwrap(),
        fs::canonicalize(dir.path("scratch")).unwrap(),
    );
    let flags = NativeFlags {
        socket: Some(fake.socket()),
        homes: Some(fake.homes()),
        network: None,
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
    };
    let settings = NativeSettings::from_flags(
        &flags,
        &[state.as_path(), scratch.as_path()],
        dir.path("native-leases").as_path(),
    );
    plan.backend = Backend::Native(settings.unwrap().unwrap());
    plan.usage = Some(Arc::new(Outbox::new()));
    let worker = Worker::start(plan).unwrap();
    let lines = worker.take_lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("native runs report no load")),
        "{lines:?}"
    );

    fake.print(b"done\n", b"");
    let start = HeartbeatReply {
        commands: vec![Command::Start(Box::new(job("r1", "/bin/echo")))],
        ..HeartbeatReply::default()
    };
    assert_eq!(worker.try_deliver(start), Delivery::Delivered);
    assert!(worker.handle().settle());
    let snapshot = worker.snapshot();
    assert_eq!(snapshot.held.len(), 1);
    assert_eq!(snapshot.held[0].state, HeldState::Running);

    fake.exit("1.0", 0);
    for _ in 0..crate::native::POLLS {
        assert!(worker.handle().settle());
        if !worker.snapshot().results.is_empty() {
            break;
        }
        std::thread::yield_now();
    }
    let snapshot = worker.snapshot();
    assert_eq!(snapshot.results.len(), 1, "the exit was never found");
    assert_eq!(snapshot.results[0].outcome, Outcome::Succeeded);
    assert_eq!(
        snapshot.results[0].log_tail.as_ref().unwrap().stdout,
        "done\n"
    );
    let verbs = fake.verbs();
    assert_eq!(
        verbs[verbs.len() - 2..],
        ["kill-uid 1.0", "user-delete 1.0"]
    );
    assert!(!dir.path("scratch").join("r1").exists());

    assert_eq!(
        worker.try_deliver(acks("r1", "ACCEPTED")),
        Delivery::Delivered
    );
    assert!(worker.handle().settle());
    let snapshot = worker.snapshot();
    assert!(snapshot.held.is_empty() && snapshot.results.is_empty());
}

/// Asserts that a worker over the native backend ends the leases an earlier process left
/// (the journal names them) before it takes a job, and says so, and that the rescue of an
/// agent whose worker died ends the leases of the runs that worker held through a runtime of
/// its own, and says so. It catches a native run that outlives the agent that started it.
#[test]
fn leases_an_earlier_process_left_are_ended_at_start_and_by_the_rescue() {
    let dir = Scratch::new("native-worker-reclaim");
    let fake = FakeHelper::start(&dir);
    let machine = FakeMachine::new();
    let mut plan = plan(&dir, &machine, STEP_NEVER);
    let (state, scratch) = (
        fs::canonicalize(dir.path("state")).unwrap(),
        fs::canonicalize(dir.path("scratch")).unwrap(),
    );
    let flags = NativeFlags {
        socket: Some(fake.socket()),
        homes: Some(fake.homes()),
        network: None,
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
    };
    let journal = dir.path("native-leases");
    let found = [state.as_path(), scratch.as_path()];
    let settings = NativeSettings::from_flags(&flags, &found, &journal);
    let settings = settings.unwrap().unwrap();
    plan.backend = Backend::Native(settings.clone());
    fs::write(&journal, "+ 9.9\n").unwrap();
    let worker = Worker::start(plan).unwrap();
    let lines = worker.take_lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("ended the lease user 9.9")),
        "{lines:?}"
    );
    let verbs = fake.verbs();
    assert_eq!(
        verbs[verbs.len() - 2..],
        ["kill-uid 9.9", "user-delete 9.9"]
    );

    fs::write(&journal, "+ 8.8\n").unwrap();
    let platform = Platform {
        os: "macos".to_owned(),
        arch: "aarch64".to_owned(),
    };
    let rescue = Rescue::new(
        Arc::new(machine.clone()),
        Backend::Native(settings),
        "h1".to_owned(),
        platform,
        std::time::Duration::from_secs(1),
    );
    let orphan = Worker::orphaned(machine.clock.clone(), Snapshot::default(), rescue);
    let lines = orphan.rescue();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("rescue: ended the lease user 8.8")),
        "{lines:?}"
    );
    let verbs = fake.verbs();
    assert_eq!(
        verbs[verbs.len() - 2..],
        ["kill-uid 8.8", "user-delete 8.8"]
    );
}
