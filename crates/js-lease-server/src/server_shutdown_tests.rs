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
    started_with(name, false).await
}

/// [`started`], with the agent routes on a loopback port of their own when `split`.
async fn started_with(
    name: &str,
    split: bool,
) -> (Running, Arc<crate::job_store::ServerJobs>, JobId, TestDir) {
    let dir = scratch(&format!("{name}_{split}"));
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
    let mut config = Config::from_cli(Cli::try_parse_from(args).unwrap()).unwrap();
    config.agent_listen = split.then(|| "127.0.0.1:0".parse().unwrap());
    if split {
        // A hold of a millisecond, so a test can let it run out; one that does not await it to
        // the end sees it pending, as it does the default hold.
        let limits = config.limits();
        config = config.with_limits(crate::config::Limits {
            drain_hold: Duration::from_millis(1),
            ..limits
        });
    }
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

/// Whether `running.hold()` returns before an always-ready future does (a hold that is not
/// held), without any clock: the first branch is polled first.
async fn hold_returns_at_once(running: &Running) -> bool {
    tokio::select! {
        biased;
        () = running.hold() => true,
        () = std::future::ready(()) => false,
    }
}

/// Asserts that a stop on a termination signal is held: `hold` makes the readiness route say
/// `draining` at once and then does not return (the default hold of 15 s is far from over),
/// so the job service is still open. It catches a stop that closes at the signal, and a
/// readiness route that keeps saying `ready` while the server waits to stop.
#[tokio::test]
async fn a_termination_is_held_with_the_readiness_route_draining() {
    let (running, _jobs, _id, _dir) = started("hold_signal").await;
    assert_eq!(running.gate.phase(), crate::ready::Phase::Ready);
    assert!(!hold_returns_at_once(&running).await);
    assert_eq!(running.gate.phase(), crate::ready::Phase::Draining);
    running.shutdown().await.unwrap();
}

/// Asserts that the agents leave at the signal, whether their routes share `--listen` or have
/// a port of their own (`server.agent_listen`): once `hold` has begun, the listener that
/// serves them drains and then refuses a new connection, the server places no job, and the
/// job service (and, with a port of their own, `--listen`) still accepts one, all before the
/// hold is over. It catches a drain that keeps taking agents for the whole hold, so they stay
/// on a server that stops instead of moving to the one that took over, and one that closes
/// more than the agents' listener.
#[tokio::test]
async fn the_agent_routes_close_at_the_signal_and_the_job_service_stays() {
    for split in [false, true] {
        let (mut running, _jobs, _id, _dir) = started_with("hold_agents_leave", split).await;
        let job = running.job_addr().unwrap();
        let agents = running.agent_addr().unwrap_or(running.addr());
        assert!(tokio::net::TcpStream::connect(agents).await.is_ok());
        assert!(!hold_returns_at_once(&running).await);
        assert!(running.shared.agents().draining.begun());
        let open = match running.agent.as_mut() {
            Some(port) => {
                assert!(
                    port.stop.sent() && !running.stop.sent(),
                    "only the agents' port stops"
                );
                (&mut port.serving).await.unwrap();
                vec![job, running.addr()]
            }
            None => {
                assert!(running.stop.sent(), "the port the agent routes share stops");
                (&mut running.serving).await.unwrap();
                vec![job]
            }
        };
        assert!(
            tokio::net::TcpStream::connect(agents).await.is_err(),
            "{split}"
        );
        for addr in open {
            assert!(
                tokio::net::TcpStream::connect(addr).await.is_ok(),
                "{split}"
            );
        }
        // The drained listener's task has ended, so the stop's await of it is not made: the
        // server is dropped, which ends the rest.
        drop(running);
    }
}

/// Asserts the whole stop of a server whose agent routes have a port of their own: the hold
/// runs out, the agents' port then refuses a connection, and the stop after it (which finds
/// that port stopping already) ends cleanly with the readiness route still `draining`. It
/// catches a stop that waits again for a port the hold stopped, or fails on it.
#[tokio::test]
async fn a_hold_that_runs_out_is_followed_by_a_clean_stop() {
    let (running, _jobs, _id, _dir) = started_with("hold_runs_out", true).await;
    let agents = running.agent_addr().unwrap();
    running.hold().await;
    assert_eq!(running.gate.phase(), crate::ready::Phase::Draining);
    running.shutdown().await.unwrap();
    assert!(tokio::net::TcpStream::connect(agents).await.is_err());
}
