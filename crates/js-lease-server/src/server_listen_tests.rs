//! A server over a Postgres store hears what another server's job store writes
//! (`docs/design/multi-instance.md`, G16): a job the other server queued is placed, and a job it
//! placed is started, on a heartbeat this server holds, before any tick (the ticker here never
//! ticks) and before the hold's bound. The other server is a job store of its own over the same
//! database, as a second process has. Nothing waits on time: the held beat is seen by the waits
//! the server counts, and a hold the notice does not end ends at its bound with nothing to send,
//! which fails the test.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use js_jobs::{Booked, JobId, JobStore, NewJob, RandomIds};
use js_state::Store;
use js_state::postgres::{
    ExtensionPool, PostgresConfig, PostgresStore, TestDatabase, test_database,
};
use js_testkit::dir::TestDir;
use js_types::ManualClock;
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use super::{Running, start};
use crate::agent_api::job_rig::spec;
use crate::clock::SharedClock;
use crate::config::{Cli, Config};
use crate::job_store::{ServerIds, ServerJobs};
use crate::ticker::channel_ticker;

const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

/// A server over the database of `url` whose agent routes are on a port of their own, with its
/// directory named `name`; its job store and the other server's read `store_clock`.
async fn server(
    name: &str,
    url: &str,
    store_clock: Arc<js_state::ManualClock>,
) -> (Running, TestDir) {
    let dir = TestDir::under(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/server_listen"),
        name,
    );
    let write = |file: &str, text: &str, mode: u32| {
        let path = dir.path().join(file);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path.display().to_string()
    };
    let pool = "name = \"burst\"\nclass = \"gpu-host\"\nmax_machines = 2\n[shape]\ncpu_millis = 8000\nmemory_bytes = 17179869184\ngpus = 1\ngpu_class = \"nvidia-a100\"\n[capabilities]\narch = \"x86_64\"\nisa = \"x86-64-v3\"\n";
    let args = [
        ("--listen", "127.0.0.1:0".to_owned()),
        ("--boot", "initialise".to_owned()),
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
        ("--agent-pool-config", write("agent-pool.toml", pool, 0o644)),
        ("--store", write("store", &format!("{url}\n"), 0o600)),
    ];
    let args = std::iter::once("js-lease-server".to_owned()).chain(
        args.into_iter()
            .flat_map(|(flag, value)| [flag.to_owned(), value]),
    );
    let mut config = Config::from_cli(Cli::try_parse_from(args).unwrap())
        .unwrap()
        .with_job_sources(store_clock, Arc::new(RandomIds));
    config.agent_listen = Some("127.0.0.1:0".parse().unwrap());
    let (_ticks, ticker) = channel_ticker();
    let clock = SharedClock::new(ManualClock::default());
    let running = start(config, clock, ticker).await.unwrap();
    (running, dir)
}

/// The other server's job tables over the database of `url`, on `store_clock`.
fn other_server(url: &str, store_clock: Arc<js_state::ManualClock>) -> ServerJobs {
    let config = PostgresConfig::parse(url).unwrap();
    let store: Arc<dyn Store> = Arc::new(
        PostgresStore::open(&config)
            .unwrap()
            .with_clock(store_clock),
    );
    JobStore::new(Arc::new(store), ServerIds::random()).unwrap()
}

/// Waits until the database of `url` holds the server's listening connection: one whose last
/// statement is its `LISTEN`, which it runs again after each quiet wait. A notice sent after it
/// reaches the server, and the wake its connection made brings has come and gone, so only the
/// notice can answer what the test holds open.
fn listening_on(url: &str) {
    let pool = ExtensionPool::open(&PostgresConfig::parse(url).unwrap()).unwrap();
    let mut loan = pool.checkout().unwrap();
    let listening = |conn: &mut js_state::postgres::Conn| -> i64 {
        let row = conn
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND query LIKE 'LISTEN %'",
                &[],
            )
            .unwrap();
        row.get(0)
    };
    while listening(loan.conn()) == 0 {
        std::thread::yield_now();
    }
}

/// `body` POSTed to `path` of `addr` with the join token; the status and the body's JSON.
async fn post(addr: std::net::SocketAddr, path: &str, body: &Value) -> (u16, Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let body = body.to_string();
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {JOIN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// The job ids the heartbeat reply `reply` starts.
fn started(reply: &Value) -> Vec<String> {
    let commands = reply["commands"].as_array().into_iter().flatten();
    let starts = commands.filter(|command| command["type"] == "job_start");
    starts
        .map(|start| start["job_id"].as_str().unwrap().to_owned())
        .collect()
}

/// Says hello as `burst-1`, an agent whose heartbeats are held, and sends its first heartbeat,
/// with nothing listed; the reply once the server answers it, after `meanwhile` ran with the
/// beat held.
async fn held_beat(running: &Running, meanwhile: impl std::future::Future<Output = ()>) -> Value {
    let agents = running.agent_addr().unwrap();
    let hello = json!({
        "host_id": "burst-1",
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 1, "gpu_class": "nvidia-a100" },
        "capabilities": { "per-job": "true", "held-beat": "true", "arch": "x86_64", "isa": "x86-64-v3" },
        "phase": "serving",
    });
    let (status, welcome) = post(agents, "/v1/agent/hello", &hello).await;
    assert_eq!(status, 200, "{welcome}");
    let beat = json!({
        "host_id": "burst-1",
        "epoch": welcome["epoch"],
        "session": welcome["session"],
        "phase": "serving",
        "seq": 1,
    });
    let held = post(agents, "/v1/agent/heartbeat", &beat);
    let other = async {
        let mut waits = running.shared.agents().wakes.waits();
        waits.wait_for(|waits| *waits >= 1).await.unwrap();
        meanwhile.await;
    };
    let ((status, reply), ()) = tokio::join!(held, other);
    assert_eq!(status, 200, "{reply}");
    reply
}

fn queued(id: &JobId, cpu_millis: u64) -> NewJob {
    let mut spec = spec();
    spec.needs =
        js_jobs::Needs::Shape(js_types::Resources::new(cpu_millis, 1 << 30, 0, None).unwrap());
    NewJob {
        id: Some(id.clone()),
        spec,
        priority: 0,
        client: None,
    }
}

/// A database of its own for one test, made outside any runtime.
fn database() -> (TestDatabase, String) {
    let database = test_database();
    let url = database.url();
    (database, url)
}

/// Catches a server that does not hear the queue of a server it shares its store with: the job
/// the other server queued would wait for this server's tick. With the host's heartbeat held
/// here, the other server queues a job the host has room for; this server's pass places it and
/// the held beat is answered with its start.
#[test]
fn a_job_another_server_queued_is_placed_and_started_on_a_beat_held_here() {
    let (_database, url) = database();
    let store_clock = Arc::new(js_state::ManualClock::default());
    let other = Arc::new(other_server(&url, Arc::clone(&store_clock)));
    Runtime::new().unwrap().block_on(async {
        let (running, _dir) = server("listen_queue", &url, store_clock).await;
        let at = url.clone();
        tokio::task::spawn_blocking(move || listening_on(&at))
            .await
            .unwrap();
        let id = JobId::parse("job-q").unwrap();
        let queueing = async {
            let (other, job) = (Arc::clone(&other), queued(&id, 1500));
            tokio::task::spawn_blocking(move || other.insert_queued(job).unwrap())
                .await
                .unwrap();
        };
        let reply = held_beat(&running, queueing).await;
        assert_eq!(started(&reply), ["job-q"], "{reply}");
    });
}

/// Catches a server that does not hear a host's notice from a server it shares its store with:
/// the start that server wrote would wait for the host's next heartbeat. With the host's
/// heartbeat held here, the other server queues a job too big for any pass to place (so this
/// server places nothing) and places it on the host itself; the held beat is answered with its
/// start.
#[test]
fn a_start_another_server_wrote_ends_a_beat_held_here() {
    let (_database, url) = database();
    let store_clock = Arc::new(js_state::ManualClock::default());
    let other = Arc::new(other_server(&url, Arc::clone(&store_clock)));
    Runtime::new().unwrap().block_on(async {
        let (running, _dir) = server("listen_host", &url, store_clock).await;
        let at = url.clone();
        tokio::task::spawn_blocking(move || listening_on(&at))
            .await
            .unwrap();
        let id = JobId::parse("job-h").unwrap();
        let placing = async {
            let (other, job, id) = (Arc::clone(&other), queued(&id, 16_000), id.clone());
            tokio::task::spawn_blocking(move || {
                other.insert_queued(job).unwrap();
                let booked = Booked::Shape {
                    cpu_millis: 1500,
                    memory_bytes: 1 << 30,
                    disk_bytes: 0,
                    accelerators: 0,
                };
                other
                    .place(&id, js_types::HostId::new("burst-1").unwrap(), booked)
                    .unwrap();
            })
            .await
            .unwrap();
        };
        let reply = held_beat(&running, placing).await;
        assert_eq!(started(&reply), ["job-h"], "{reply}");
    });
}
