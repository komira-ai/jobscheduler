//! The shutdown of a server: the orderly stop runs a last tick. Nothing waits on time: the
//! clocks are manual.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{JobId, JobState, NewJob, StartFailure};
use js_testkit::dir::TestDir;
use js_types::ManualClock;

use super::{Running, start};
use crate::agent_api::job_rig::spec;
use crate::clock::SharedClock;
use crate::config::{Cli, Config};
use crate::ticker::channel_ticker;

/// A century, which carries a manual clock past any wait a queued job is held to.
const CENTURY: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/server_shutdown"),
        name,
    )
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
    let state = dir.path().join("state");
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
    let jobs = Arc::clone(running.shared.agents().jobs.as_ref().unwrap());
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

/// Asserts that an orderly stop runs a final tick: the queued job past its wait is ended.
/// It catches a shutdown that stops without the tick that ends what ran out of time.
#[tokio::test]
async fn an_orderly_shutdown_runs_a_final_tick() {
    let (running, jobs, id, _dir) = started("shutdown_orderly").await;
    running.shutdown().await.unwrap();
    let ended = JobState::FailedToStart(StartFailure::NoCapacity);
    assert_eq!(jobs.get_job(&id).unwrap().unwrap().state, ended);
}
