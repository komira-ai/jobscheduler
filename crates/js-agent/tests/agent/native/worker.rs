//! A native job through the worker, the thread that owns the lifecycle: the backend the
//! worker is made over is the native one, its runtime is the real one over a pool of a fake
//! host and a fake launcher (made by a machine of this module, which gives a worker the
//! runtime a Mac agent with Full Disk Access would make), its cleanup is the native checks,
//! and its load is the Mac host's, with no line at start that says the heartbeat carries none.

use std::sync::Arc;

use js_agent::integrity::HostView;
use js_agent::lifecycle::Runtime;
use js_agent::mac_accounts::Pool;
use js_agent::native::{Journal, NativeConfig, NativeRuntime};
use js_agent::usage::Outbox;
use js_agent::{Delivery, Machine, Platform, Rescue, RuntimeBuild, SharedClock, Snapshot, Worker};
use js_agent_wire::{HeartbeatReply, Outcome};

use super::agent::backend;
use super::lifecycle::job_start;
use super::{FakeAccounts, FakeLauncher, TERM, config, open_pool, pool_settings};
use crate::jobs::fake::FakeMachine;
use crate::jobs::{STEP_NEVER, acks, plan};
use crate::support::Scratch;

/// The machine of a worker whose native runtime runs over the account pool `pool` and the
/// launcher `launcher`, in place of the pool and the supervisor of the system: the clock and
/// the host are the fake machine's. Every runtime it makes (the worker's, the rescue's) is
/// over the same pool and the same journal.
#[derive(Clone)]
struct OverAccounts {
    machine: FakeMachine,
    pool: Arc<Pool>,
    launcher: Arc<FakeLauncher>,
    config: NativeConfig,
}

impl Machine for OverAccounts {
    fn clock(&self) -> SharedClock {
        self.machine.clock()
    }

    fn runtime(
        &self,
        _build: &RuntimeBuild<'_>,
    ) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
        let runtime = NativeRuntime::over(
            Some(self.pool.clone()),
            self.launcher.clone(),
            self.config.clone(),
            TERM,
        );
        let runtime = runtime
            .map_err(|why| why.to_string())?
            .with_process_table(super::end::GroupTable::new(self.launcher.clone()));
        Ok((Box::new(runtime), Vec::new()))
    }

    fn view(&self) -> Box<dyn HostView> {
        self.machine.view()
    }
}

/// What a worker of `dir` runs over: the machine, the fake host of the pool and the
/// launcher.
fn over_accounts(dir: &Scratch) -> (OverAccounts, Arc<FakeAccounts>, Arc<FakeLauncher>) {
    let accounts = FakeAccounts::new();
    let launcher = FakeLauncher::new();
    let settings = pool_settings(dir, 1);
    let pool = open_pool(dir, &settings, &accounts);
    let machine = OverAccounts {
        machine: FakeMachine::new(),
        pool,
        launcher: launcher.clone(),
        config: config(dir, &settings),
    };
    (machine, accounts, launcher)
}

/// Asserts the path of a native job's result through the worker: a job start the native
/// runtime cannot serve (an image start, on a runtime with no bundle store) is held with a
/// `FailedToStart` result carrying `image-unsupported-here`, the result stays in the
/// worker's snapshot until an acknowledgement drops the job, and nothing is lent: no account
/// and no run directory. It also asserts that a worker over the native backend says nothing
/// at start of a load it cannot carry: the Mac host's counters are its load. It catches a
/// native agent whose worker keeps a result from the server, or lends an account to a start
/// it refuses, and one whose heartbeat still carries no load.
#[test]
fn a_native_result_stays_published_until_it_is_acknowledged() {
    let dir = Scratch::new("native-worker");
    let (over, accounts, launcher) = over_accounts(&dir);
    let mut plan = plan(&dir, &over.machine, STEP_NEVER);
    plan.machine = Arc::new(over.clone());
    plan.backend = backend(&dir);
    plan.usage = Some(Arc::new(Outbox::new()));
    let worker = Worker::start(plan).unwrap();
    let lines = worker.take_lines();
    assert!(
        !lines.iter().any(|line| line.starts_with("usage:")),
        "{lines:?}"
    );

    let start = HeartbeatReply {
        commands: vec![job_start("j1", &[])],
        ..HeartbeatReply::default()
    };
    assert_eq!(worker.try_deliver(start), Delivery::Delivered);
    assert!(worker.handle().settle());
    let snapshot = worker.snapshot();
    assert_eq!(snapshot.jobs.len(), 1);
    assert_eq!(snapshot.jobs[0].job_id, "j1");
    let result = snapshot.jobs[0].result.as_ref().expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("image-unsupported-here"));
    assert!(launcher.launched().is_empty(), "no supervisor");
    assert!(accounts.records().is_empty(), "no account is lent");
    assert!(!dir.path("scratch").join("j1").exists());

    assert_eq!(
        worker.try_deliver(acks("j1", "ACCEPTED")),
        Delivery::Delivered
    );
    assert!(worker.handle().settle());
    let snapshot = worker.snapshot();
    assert!(snapshot.jobs.is_empty());
}

/// Asserts that a worker over the native backend drops the leases an earlier process left
/// with no job started (the journal names them) before it takes a job, and says so, and that
/// the rescue of an agent whose worker died does the same through a runtime of its own, and
/// says so. It catches a lease an earlier process left that stays in the journal for good.
#[test]
fn leases_an_earlier_process_left_are_dropped_at_start_and_by_the_rescue() {
    let dir = Scratch::new("native-worker-reclaim");
    let (over, _, _) = over_accounts(&dir);
    let mut plan = plan(&dir, &over.machine, STEP_NEVER);
    plan.machine = Arc::new(over.clone());
    plan.backend = backend(&dir);
    let journal = dir.path("native-leases");
    std::fs::write(&journal, "+ 9.9\n").unwrap();
    let worker = Worker::start(plan).unwrap();
    let lines = worker.take_lines();
    assert!(
        lines.contains(
            &"dropped the lease 9.9 an earlier process left before its job started".to_owned()
        ),
        "{lines:?}"
    );
    assert_eq!(Journal::new(&journal).live().unwrap(), Vec::<String>::new());

    std::fs::write(&journal, "+ 8.8\n").unwrap();
    let platform = Platform {
        os: "macos".to_owned(),
        arch: "aarch64".to_owned(),
    };
    let rescue = Rescue::new(
        Arc::new(over.clone()),
        backend(&dir),
        "h1".to_owned(),
        platform,
        std::time::Duration::from_secs(1),
    );
    let orphan = Worker::orphaned(over.machine.clock.clone(), Snapshot::default(), rescue);
    let lines = orphan.rescue();
    assert!(
        lines.contains(
            &"rescue: dropped the lease 8.8 an earlier process left before its job started"
                .to_owned()
        ),
        "{lines:?}"
    );
    assert_eq!(Journal::new(&journal).live().unwrap(), Vec::<String>::new());
}
