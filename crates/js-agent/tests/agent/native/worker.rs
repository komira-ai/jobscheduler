//! A native job through the worker, the thread that owns the lifecycle: the backend the
//! worker is made over is the native one, its runtime is the real one over a pool of a fake
//! host and a fake launcher (made by a machine of this module, which gives a worker the
//! runtime a Mac agent with Full Disk Access would make), its cleanup is the native checks,
//! and it says it reports no load.

use std::sync::Arc;

use js_agent::integrity::HostView;
use js_agent::lifecycle::Runtime;
use js_agent::mac_accounts::Pool;
use js_agent::native::{Journal, NativeConfig, NativeRuntime};
use js_agent::usage::Outbox;
use js_agent::{Delivery, Machine, Platform, Rescue, RuntimeBuild, SharedClock, Snapshot, Worker};
use js_agent_wire::{Command, HeartbeatReply, HeldState, Outcome};

use super::agent::backend;
use super::lifecycle::job;
use super::{FakeAccounts, FakeLauncher, POLLS, TERM, config, open_pool, pool_settings};
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

/// Asserts the path of a native result through the worker: the start runs the program and
/// the run is held as running; the program's exit is found by the worker's next step and
/// published as a result with the exit code and what the program printed; the account the run
/// was lent is retired (its record and home gone) and the scratch directory goes; an
/// acknowledgement drops the run. It also asserts that a worker over the native backend says
/// at start that the heartbeat carries no load. It catches a native agent whose worker keeps
/// a result from the server, or leaves the account.
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
        lines
            .iter()
            .any(|line| line.contains("native runs report no load")),
        "{lines:?}"
    );

    // The program waits for the gate file, then prints.
    let gate = dir.path("gate");
    let mut start = job("r1", "/bin/sh");
    start.native.as_mut().unwrap().args = [
        "-c",
        "while [ ! -e \"$0\" ]; do :; done; echo done",
        gate.to_str().unwrap(),
    ]
    .map(str::to_owned)
    .to_vec();
    let start = HeartbeatReply {
        commands: vec![Command::Start(Box::new(start))],
        ..HeartbeatReply::default()
    };
    assert_eq!(worker.try_deliver(start), Delivery::Delivered);
    assert!(worker.handle().settle());
    let snapshot = worker.snapshot();
    assert_eq!(snapshot.held.len(), 1);
    assert_eq!(snapshot.held[0].state, HeldState::Running);
    let lent = launcher.last();
    let account = lent.var("USER").unwrap().to_owned();
    assert!(accounts.records().contains(&account), "the account is lent");

    std::fs::write(&gate, b"").unwrap();
    for _ in 0..POLLS {
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
    for _ in 0..POLLS {
        if !accounts.records().contains(&account) {
            break;
        }
        assert!(worker.handle().settle());
        std::thread::yield_now();
    }
    assert!(
        !accounts.records().contains(&account),
        "the run's account is retired"
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
