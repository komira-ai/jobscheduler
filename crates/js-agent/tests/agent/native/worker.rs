//! A native job through the worker, the thread that owns the lifecycle: the backend the
//! worker is made over is the native one, so its runtime is made over the helper, its
//! cleanup is the native checks, and it says it reports no load.

use std::fs;
use std::sync::Arc;

use js_agent::native::{NativeFlags, NativeSettings};
use js_agent::usage::Outbox;
use js_agent::{Backend, Delivery, Worker};
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
    };
    let settings = NativeSettings::from_flags(&flags, &[state.as_path(), scratch.as_path()]);
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
