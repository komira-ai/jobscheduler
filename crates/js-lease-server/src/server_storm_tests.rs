//! The server as the farm runs it, over Postgres: a multi-thread runtime as the program builds,
//! ten per-job hosts, a queue of two thousand jobs, and the farm's flags (`--tick 1s`,
//! `--placement-curves shadow`, `--boot resume` and the cold bookings). Every store call the
//! server makes runs on the runtime's pool for blocking calls; what the test counts is what the
//! store counts of its own work ([`crate::store::ServerStore::tally`]): the statements it sent
//! and the threads it started for its calls, per tick and per heartbeat. It also prints the
//! process's voluntary context switches (`getrusage`) beside them, the figure the farm's
//! control host showed; that figure is read, not asserted, since it depends on the scheduler.
//!
//! Nothing here waits on time: the ticks are stepped, each heartbeat is answered at once (the
//! hosts do not hold their beats), and the counts are taken between steps.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use js_jobs::JobId;
use js_testkit::dir::TestDir;
use js_types::ManualClock;
use nix::sys::resource::{UsageWho, getrusage};
use serde_json::{Value, json};

use super::listen_tests::{JOIN, database, other_server, post, queued};
use super::{Running, start};
use crate::clock::SharedClock;
use crate::config::{Cli, Config};
use crate::store::StoreTally;
use crate::ticker::{TickHandle, channel_ticker};

/// The hosts, as many as the farm's.
const HOSTS: usize = 10;

/// The queued jobs when the server starts: a deep queue, as the farm's was.
const QUEUED: usize = 2_000;

/// The ticks stepped.
const TICKS: u64 = 5;

/// The heartbeats each host sends.
const BEATS: u64 = 3;

/// The hosts' pool: ten hosts of 8 cores and 16 GiB, each with room for a few of the queued
/// jobs, so the queue stays deep.
const POOL: &str = "name = \"farm\"\nclass = \"build-host\"\nmax_machines = 10\n[shape]\ncpu_millis = 8000\nmemory_bytes = 17179869184\n[capabilities]\narch = \"x86_64\"\nisa = \"x86-64-v3\"\n";

/// A server over the database of `url` with the farm's flags, its agent routes on a port of
/// their own and its ticks stepped by the handle returned; its job store reads `store_clock`.
async fn farm_server(
    url: &str,
    store_clock: Arc<js_state::ManualClock>,
) -> (Running, TickHandle, TestDir) {
    let dir = TestDir::under(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/server_storm"),
        "farm",
    );
    let write = |file: &str, text: &str, mode: u32| {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.path().join(file);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path.display().to_string()
    };
    let args = [
        ("--listen", "127.0.0.1:0".to_owned()),
        ("--boot", "resume".to_owned()),
        ("--tick", "1s".to_owned()),
        ("--placement-curves", "shadow".to_owned()),
        ("--cold-cpu-millis", "4000".to_owned()),
        ("--cold-memory-bytes", "17179869184".to_owned()),
        ("--cold-cpu-millis-on", "macos=1500".to_owned()),
        ("--cold-memory-bytes-on", "macos=6442450944".to_owned()),
        ("--job-listen", "127.0.0.1:0".to_owned()),
        (
            "--state-file",
            dir.path().join("state").display().to_string(),
        ),
        (
            "--token-file",
            write("token", "test-token-0123456789-abcdefghijkl", 0o600),
        ),
        (
            "--agent-token-file",
            write("agent-tokens", &format!("{JOIN}\n"), 0o600),
        ),
        ("--agent-pool-config", write("agent-pool.toml", POOL, 0o644)),
        ("--store", write("store", &format!("{url}\n"), 0o600)),
    ];
    let args = std::iter::once("js-lease-server".to_owned()).chain(
        args.into_iter()
            .flat_map(|(flag, value)| [flag.to_owned(), value]),
    );
    let mut config = Config::from_cli(Cli::try_parse_from(args).unwrap())
        .unwrap()
        .with_job_sources(store_clock, Arc::new(js_jobs::RandomIds));
    config.agent_listen = Some("127.0.0.1:0".parse().unwrap());
    let (ticks, ticker) = channel_ticker();
    let clock = SharedClock::new(ManualClock::default());
    let running = start(config, clock, ticker).await.unwrap();
    (running, ticks, dir)
}

/// The host `n` of the pool, at the agent routes `agents`: it says hello as a per-job host and
/// sends [`BEATS`] heartbeats, each as soon as the last was answered, listing nothing.
async fn host(agents: SocketAddr, n: usize) {
    let host_id = format!("farm-{n}");
    let hello = json!({
        "host_id": host_id,
        "pool": "farm",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 },
        "capabilities": { "per-job": "true", "arch": "x86_64", "isa": "x86-64-v3" },
        "phase": "serving",
    });
    let (status, welcome) = post(agents, "/v1/agent/hello", &hello).await;
    assert_eq!(status, 200, "{welcome}");
    for seq in 1..=BEATS {
        let beat = json!({
            "host_id": host_id,
            "epoch": welcome["epoch"],
            "session": welcome["session"],
            "phase": "serving",
            "seq": seq,
        });
        let (status, reply): (u16, Value) = post(agents, "/v1/agent/heartbeat", &beat).await;
        assert_eq!(status, 200, "{reply}");
    }
}

/// What the server's store has counted so far.
fn tally(running: &Running) -> StoreTally {
    let store = running.shared.server_store().unwrap();
    store.tally().unwrap()
}

/// The voluntary context switches of the whole process so far, ended threads included.
fn switches() -> i64 {
    getrusage(UsageWho::RUSAGE_SELF)
        .unwrap()
        .voluntary_context_switches()
}

/// What a window of the run cost: the store's counts and the process's switches over it.
struct Cost {
    statements: u64,
    threads: u64,
    switches: i64,
}

/// Runs `window` and returns what it cost.
async fn cost(running: &Running, window: impl std::future::Future<Output = ()>) -> Cost {
    let (before, switched) = (tally(running), switches());
    window.await;
    let after = tally(running);
    Cost {
        statements: after.statements - before.statements,
        threads: after.threads - before.threads,
        switches: switches() - switched,
    }
}

/// Catches a server whose store starts a thread for each call it makes: under the farm's load
/// that is thousands of threads started and ended a second, and every thread of the process
/// is woken by them (the start and end of a thread take the process's memory map lock, which
/// the page faults of every other thread wait on). With ten hosts beating and the ticks of a
/// two-thousand-job queue stepped, the store sends statements and starts no thread for any of
/// them: each call runs on the thread of the pool for blocking calls that makes it.
#[test]
fn a_farm_shaped_server_starts_no_thread_for_a_store_call() {
    let (_database, url) = database();
    let store_clock = Arc::new(js_state::ManualClock::default());
    let other = other_server(&url, Arc::clone(&store_clock));
    // The queue, written before the server starts, as the farm's was when its server started
    // over it (`--boot resume`).
    for n in 0..QUEUED {
        let id = JobId::parse(&format!("job-{n}")).unwrap();
        other.insert_queued(queued(&id, 1500)).unwrap();
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (running, ticks, _dir) = farm_server(&url, store_clock).await;
        let agents = running.agent_addr().unwrap();
        let beats = cost(&running, async {
            let mut hosts = tokio::task::JoinSet::new();
            for n in 1..=HOSTS {
                let _ = hosts.spawn(host(agents, n));
            }
            hosts.join_all().await;
        })
        .await;
        let stepped = cost(&running, async {
            for _ in 0..TICKS {
                ticks.step().await;
            }
        })
        .await;
        let beats_sent = u64::try_from(HOSTS).unwrap() * BEATS;
        eprintln!(
            "{HOSTS} hosts, {QUEUED} queued: {beats_sent} heartbeats sent {} statements and \
             started {} threads ({} voluntary switches); {TICKS} ticks sent {} statements and \
             started {} threads ({} voluntary switches)",
            beats.statements,
            beats.threads,
            beats.switches,
            stepped.statements,
            stepped.threads,
            stepped.switches,
        );
        assert!(beats.statements > 0 && stepped.statements > 0);
        assert_eq!((beats.threads, stepped.threads), (0, 0));
        running.shutdown().await.unwrap();
    });
}
