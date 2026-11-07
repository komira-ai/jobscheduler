//! A state store with a newer writer (`StoreError::Fenced`) ends the server: the tick loop,
//! the final tick of a shutdown and the exit status; so does a lost writer lock, at shutdown.
//! Nothing waits on time: ticks are stepped by the test and the clocks are manual.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{JobId, JobState, JobStoreError, NewJob, StartFailure};
use js_lease::{Boot, LeaseTable, ManualClock, Settings};
use js_state::{Store, StoreError};
use js_testkit::dir::TestDir;
use tokio::task::JoinHandle;

use super::{FENCED, Running, start, stop_or_fence, tick_loop, until_fenced};
use crate::adapt::{BoxedPool, SharedClock};
use crate::agent_api::job_rig::{Rig, clocked_rig, spec};
use crate::agents::Agents;
use crate::api::Shared;
use crate::config::{Cli, Config, parse_token};
use crate::error::Failure;
use crate::job_expiry::JobLimits;
use crate::store::{LockLost, LockWatch};
use crate::ticker::{ChannelTicker, channel_ticker};

/// A century, which carries a manual clock past any wait a queued job is held to.
const CENTURY: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/server_fence"),
        name,
    )
}

/// A server state over the rig's agent side (its job tables, store and clock) and a lease
/// table of its own in `dir`.
fn shared_over(rig: &Rig, dir: &TestDir) -> Arc<Shared> {
    shared_with(rig, dir, Some(Arc::clone(&rig.agents)))
}

/// [`shared_over`] holding `agents`, which may be none.
fn shared_with(rig: &Rig, dir: &TestDir, agents: Option<Arc<Agents>>) -> Arc<Shared> {
    let settings = Settings::new();
    let table = LeaseTable::open(
        BoxedPool(Box::new(rig.agents.registry.provider())),
        rig.shared.clone(),
        settings,
        dir.path().join("leases.toml"),
        Boot::Initialise,
    )
    .unwrap();
    let token = parse_token(&[b'c'; 40]).unwrap();
    Arc::new(Shared::new(table, token, agents))
}

/// The tick loop of `shared` as the server runs it, on a task.
fn spawn_ticking(shared: &Arc<Shared>, ticker: ChannelTicker) -> JoinHandle<()> {
    let ticking = Box::pin(tick_loop(
        Arc::clone(shared),
        ticker,
        None,
        LockWatch::new(),
    ));
    tokio::spawn(until_fenced(Arc::clone(shared), ticking))
}

/// Records the job `id` as queued.
fn queue(rig: &Rig, id: &str) -> JobId {
    let id = JobId::parse(id).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap();
    id
}

/// Asserts that a tick that meets `Fenced` in the job pass ends the loop (the task finishes,
/// `/healthz` reads not ticking), and that a tick asked for afterwards is not run: a queued
/// job that is past its wait, which a running tick would end with a commit, stays queued and
/// the store sees no commit. It catches a fenced store logged like any other failure, so the
/// loop ticks on, and a loop that ends but whose later ticks still write.
#[tokio::test]
async fn a_tick_that_meets_a_fenced_store_ends_the_loop_and_writes_nothing_after() {
    let rig = clocked_rig("fence_loop", false, JobLimits::DEFAULT);
    let dir = scratch("fence_loop_table");
    let shared = shared_over(&rig, &dir);
    let id = queue(&rig, "job-q");
    let (handle, ticker) = channel_ticker();
    let task = spawn_ticking(&shared, ticker);

    handle.step().await;
    assert!(shared.ticking());
    assert_eq!(rig.job(&id).state, JobState::Queued);

    rig.advance(CENTURY);
    rig.store.fail_with(StoreError::Fenced);
    rig.store.fail_read_after(0);
    handle.step().await;
    task.await.unwrap();
    assert!(!shared.ticking());
    assert!(rig.agents.fence.is_tripped());
    assert_eq!(rig.job(&id).state, JobState::Queued);

    let commits = rig.store.commit_stats().txns;
    handle.step().await;
    assert_eq!(rig.store.commit_stats().txns, commits);
    assert_eq!(rig.job(&id).state, JobState::Queued);
}

/// Asserts that a loop waiting for its next tick ends when the fence trips (a heartbeat saw
/// `Fenced`), without a tick being stepped. It catches a loop that notices the fence only at
/// the next tick, which a long `--tick` makes a long time of a server serving on.
#[tokio::test]
async fn a_loop_waiting_for_a_tick_ends_when_the_fence_trips() {
    let rig = clocked_rig("fence_waiting", false, JobLimits::DEFAULT);
    let dir = scratch("fence_waiting_table");
    let shared = shared_over(&rig, &dir);
    let (handle, ticker) = channel_ticker();
    let task = spawn_ticking(&shared, ticker);
    handle.step().await;
    assert!(shared.ticking());
    let fenced = JobStoreError::Store(StoreError::Fenced);
    assert!(rig.agents.fence.note(&fenced));
    task.await.unwrap();
    assert!(!shared.ticking());
}

/// A server on a manual clock with the job service on and one job queued, the clock a century
/// past any wait the job is held to; it is returned with its job tables, the job and the
/// directory of its files named `name`.
async fn started(name: &str) -> (Running, Arc<crate::job_store::ServerJobs>, JobId, TestDir) {
    let dir = scratch(name);
    let write = |file: &str, text: &str, mode: u32| {
        let path = dir.path().join(file);
        std::fs::write(&path, text).unwrap();
        let mode = std::fs::Permissions::from_mode(mode);
        std::fs::set_permissions(&path, mode).unwrap();
        path
    };
    let pool = "name = \"burst\"\nclass = \"gpu-host\"\nmax_machines = 2\n[shape]\ncpu_millis = 8000\nmemory_bytes = 17179869184\ngpus = 1\ngpu_class = \"nvidia-a100\"\n[capabilities]\narch = \"x86_64\"\nisa = \"x86-64-v3\"\n";
    let token = write("token", "test-token-0123456789-abcdefghijkl", 0o600);
    let joins = write(
        "agent-tokens",
        "join-token-A-0123456789-abcdefghijklmnop\n",
        0o600,
    );
    let pool = write("agent-pool.toml", pool, 0o644);
    let state = dir.path().join("leases.toml");
    let args = [
        "js-lease-server",
        "--listen",
        "127.0.0.1:0",
        "--boot",
        "initialise",
        "--job-listen",
        "127.0.0.1:0",
    ]
    .map(String::from)
    .into_iter()
    .chain(
        [
            ("--state-file", state),
            ("--token-file", token),
            ("--agent-token-file", joins),
            ("--agent-pool-config", pool),
        ]
        .into_iter()
        .flat_map(|(flag, path)| [flag.to_owned(), path.display().to_string()]),
    );
    let config = Config::from_cli(Cli::try_parse_from(args).unwrap()).unwrap();
    let clock = ManualClock::default();
    let (_handle, ticker) = channel_ticker();
    let running = start(config, SharedClock::new(clock.clone()), ticker)
        .await
        .unwrap();
    let jobs = Arc::clone(running.shared.agents().unwrap().jobs.as_ref().unwrap());
    let id = JobId::parse("job-q").unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
    clock.advance(CENTURY);
    (running, jobs, id, dir)
}

/// Asserts that an orderly stop runs a final tick (the queued job past its wait is ended), and
/// that a stop of a server whose store has a newer writer runs none (the same job stays
/// queued) and is an error saying why. It catches a shutdown that writes to a store the server
/// no longer owns, and one that says nothing of why it stopped.
#[tokio::test]
async fn a_fenced_shutdown_runs_no_final_tick_and_is_an_error() {
    let (running, jobs, id, _dir) = started("fence_shutdown_orderly").await;
    running.shutdown().await.unwrap();
    let ended = JobState::FailedToStart(StartFailure::NoCapacity);
    assert_eq!(jobs.get_job(&id).unwrap().unwrap().state, ended);

    let (running, jobs, id, _dir) = started("fence_shutdown_fenced").await;
    let fenced = JobStoreError::Store(StoreError::Fenced);
    assert!(running.shared.agents().unwrap().fence.note(&fenced));
    running.fenced().await;
    let stopped = running.shutdown().await.unwrap_err();
    assert_eq!(stopped.to_string(), FENCED);
    assert_eq!(jobs.get_job(&id).unwrap().unwrap().state, JobState::Queued);
}

/// Asserts that a stop after the store's writer lock was lost runs no final tick (the queued
/// job past its wait stays queued), that the loss is announced to [`Running::lock_lost`], and
/// that the stop is an error that is the loss. It catches a shutdown that writes after the
/// lock was lost, and one that says nothing of why it stopped.
#[tokio::test]
async fn a_shutdown_after_a_lost_writer_lock_runs_no_final_tick_and_is_the_loss() {
    let (running, jobs, id, _dir) = started("lock_lost_shutdown").await;
    let lost = LockLost::from(StoreError::Failed("the lock's session is gone".to_owned()));
    running.lock_watch.lose(lost.clone());
    assert_eq!(running.lock_lost().await, lost);
    let stopped = running.shutdown().await.unwrap_err();
    assert_eq!(stopped.to_string(), lost.to_string());
    assert_eq!(jobs.get_job(&id).unwrap().unwrap().state, JobState::Queued);
}

/// Asserts that the wait for a stop returns what the signal gave while the store is the
/// server's, and returns at once, with the one error line logged, when the fence trips; that
/// the stop that follows is a runtime failure with the fenced message and exit status 1; and
/// that the process prints that message as its one error line. It catches a server that, once
/// fenced, keeps running until a signal and exits 0.
#[tokio::test]
async fn a_fenced_server_stops_with_a_failure_exit_status() {
    let (running, _jobs, _id, _dir) = started("fence_exit").await;
    let signalled = std::future::ready(Ok(()));
    assert!(stop_or_fence(&running, signalled).await.is_ok());
    let broken = std::future::ready(Err(std::io::Error::other("signal")));
    let failed = stop_or_fence(&running, broken).await.unwrap_err();
    assert_eq!(failed.to_string(), "signal");

    let fenced = JobStoreError::Store(StoreError::Fenced);
    assert!(running.shared.agents().unwrap().fence.note(&fenced));
    let never = std::future::pending::<std::io::Result<()>>();
    stop_or_fence(&running, never).await.unwrap();
    let failure = Failure::Runtime(running.shutdown().await.unwrap_err());
    assert_eq!(failure.to_string(), FENCED);
    assert!(failure.code() == std::process::ExitCode::FAILURE);
    let mut printed = Vec::new();
    let code = crate::exit_code(Err(failure), &mut printed);
    assert!(code == std::process::ExitCode::FAILURE);
    assert_eq!(
        String::from_utf8(printed).unwrap(),
        format!("js-lease-server: {FENCED}\n")
    );
}

/// Asserts that a server without the agent side (which holds the only state store) is never
/// fenced: the wait for the fence is not ready, and it is not marked fenced. It catches a
/// server with no store that stops as if it had lost one.
#[tokio::test]
async fn a_server_without_a_state_store_is_never_fenced() {
    let rig = clocked_rig("fence_none", false, JobLimits::DEFAULT);
    let dir = scratch("fence_none_table");
    let shared = shared_with(&rig, &dir, None);
    let waited = tokio::select! {
        biased;
        () = shared.fenced() => false,
        () = std::future::ready(()) => true,
    };
    assert!(waited);
    assert!(!shared.is_fenced());
}
