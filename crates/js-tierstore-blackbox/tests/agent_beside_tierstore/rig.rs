//! One test's host: the packages installed, tierstore over a stallable upstream, the counting
//! proxy on its provider socket, the lease server, and the agent joined with tierstore's
//! capability; and the jobs the test submits, read back until they end.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use js_testkit::dir::TestDir;
use js_testkit::port::own_loopback_port;
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use crate::agent::{self, PROVIDER_SOCKET};
use crate::host::{self, AGENT_UNIT, Installed};
use crate::identity::{Ca, HOST_ID};
use crate::provider::Provider;
use crate::server::{POOL, Server};
use crate::stall::Stall;
use crate::tierstore::{self, MAX_PLACEMENTS, Tierstore};
use crate::upstream::{self, Credentials, READY_FILE};

/// The image every job runs: busybox, by the digest of its linux/amd64 manifest.
const IMAGE: &str = "docker.io/library/busybox@sha256:66a6306db78bf2dbf3487f293aa8d6990d8e506fdffab9cc43fe422becf886e4";

/// Where a job's input tree is mounted.
const TARGET: &str = "/work/in";

/// How long the agent may take to join with tierstore's capability.
const JOIN_LIMIT: Duration = Duration::from_secs(5 * 60);

/// How long the jobs may take to end, all together, before the test fails.
const END_LIMIT: Duration = Duration::from_secs(20 * 60);

/// The states a job ends in.
const ENDED: [&str; 5] = [
    "JOB_STATE_SUCCEEDED",
    "JOB_STATE_FAILED",
    "JOB_STATE_CANCELLED",
    "JOB_STATE_LOST",
    "JOB_STATE_FAILED_TO_START",
];

/// An input of the environment: its value, or the test fails naming it.
fn input(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is unset"))
}

/// The host of one test. Fields drop in order: the server and the proxies end before the
/// runtime they run on, and the scratch directory goes last.
pub struct Rig {
    server: Server,
    /// The proxy on tierstore's provider socket, and the calls it passed on.
    pub provider: Provider,
    /// The proxy in front of tierstore's upstream store.
    pub stall: Stall,
    /// The tierstore instance.
    pub tierstore: Tierstore,
    _runtime: Runtime,
    installed: Installed,
    tree: String,
    started: Instant,
    _dir: TestDir,
}

impl Rig {
    /// The host of the test `name`, with the agent joined and placeable.
    pub fn up(name: &str) -> Rig {
        host::require_root();
        let version = input("JS_BLACKBOX_AGENT_VERSION");
        let program = PathBuf::from(input("JS_BLACKBOX_SERVER_BIN"));
        let store: SocketAddr = input("JS_BLACKBOX_S3")
            .trim_start_matches("http://")
            .parse()
            .expect("JS_BLACKBOX_S3 is http://<ip>:<port>");
        let keys = std::fs::read_to_string(input("JS_BLACKBOX_S3_KEY_FILE"))
            .expect("JS_BLACKBOX_S3_KEY_FILE is readable");
        let mut keys = keys.lines();
        let credentials = Credentials {
            key_id: keys.next().expect("a key id line").to_owned(),
            secret: keys.next().expect("a secret line").to_owned(),
        };

        let installed = host::install(&version);
        host::stop(AGENT_UNIT);
        host::forget_agent();
        host::scratch();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let ca = Ca::new();
        ca.write_host();
        let tree = upstream::put_tree(store, &credentials);
        let stall = Stall::start(runtime.handle(), store);
        let s3 = own_loopback_port();
        let tierstore = Tierstore::start(s3, stall.addr, &credentials);
        let proxy_dir = Path::new(PROVIDER_SOCKET).parent().expect("a parent");
        host::make_dir(proxy_dir, 0o755);
        let provider = Provider::start(
            runtime.handle(),
            Path::new(PROVIDER_SOCKET),
            Path::new(tierstore::SOCKET),
        );

        let dir = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("agent-beside-tierstore"),
            name,
        );
        let server = Server::start(&program, &dir, &ca.pem());
        let cert = std::fs::read_to_string(&server.cert).expect("the server's certificate");
        agent::write(&server.url(), &cert, POOL, s3);
        host::start(AGENT_UNIT);

        let rig = Rig {
            server,
            provider,
            stall,
            tierstore,
            _runtime: runtime,
            installed,
            tree,
            started: Instant::now(),
            _dir: dir,
        };
        rig.wait_joined();
        rig
    }

    /// Waits until the agent is a live host of the pool. Whether its hello reported tierstore's
    /// provider (`mount-provider.tierstore`) the host list does not show (it lists the pool's
    /// capabilities); the first submit shows it, as the job service refuses a job that names the
    /// provider `no-capable-host` when no host reported it.
    fn wait_joined(&self) {
        let limit = Instant::now() + JOIN_LIMIT;
        loop {
            let entry = self.server.host(HOST_ID);
            if entry.as_ref().is_some_and(|entry| entry["state"] == "live") {
                return;
            }
            assert!(
                Instant::now() < limit,
                "the agent was not a live host within {} s: {entry:?}\n{}\n{}",
                JOIN_LIMIT.as_secs(),
                self.server.err(),
                host::journal()
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Submits `count` jobs named `<prefix>-<n>`, each mounting the tree from tierstore and
    /// checking that its file is there, and returns their ids.
    pub fn submit(&self, prefix: &str, count: usize) -> Vec<String> {
        (0..count)
            .map(|n| {
                let id = format!("{prefix}-{n:02}");
                self.server.submit(&json!({
                    "jobId": id,
                    "image": IMAGE,
                    "command": "/bin/sh",
                    "args": ["-c", format!("test -f {TARGET}/{READY_FILE}")],
                    "needs": { "cpuMillis": "50", "memoryBytes": "67108864" },
                    "wallLimitSeconds": "600",
                    "mounts": [{
                        "provider": { "provider": "tierstore", "ref": self.tree },
                        "target": TARGET,
                    }],
                }));
                id
            })
            .collect()
    }

    /// Returns at `at`.
    pub fn wait_until(&self, at: Instant) {
        let now = Instant::now();
        if at > now {
            std::thread::sleep(at - now);
        }
    }

    /// Waits until every job of `ids` has ended and returns how each ended.
    pub fn wait_for_ends(&self, ids: &[String]) -> Vec<End> {
        let limit = Instant::now() + END_LIMIT;
        let mut ends: Vec<Option<End>> = vec![None; ids.len()];
        loop {
            for (id, end) in ids.iter().zip(ends.iter_mut()) {
                if end.is_none() {
                    let job = self.server.job(id);
                    let state = job["state"].as_str().unwrap_or_default().to_owned();
                    if ENDED.contains(&state.as_str()) {
                        *end = Some(End {
                            id: id.clone(),
                            job,
                        });
                    }
                }
            }
            if ends.iter().all(Option::is_some) {
                return ends.into_iter().flatten().collect();
            }
            let waiting: Vec<&String> = ids
                .iter()
                .zip(&ends)
                .filter(|(_, end)| end.is_none())
                .map(|(id, _)| id)
                .collect();
            assert!(
                Instant::now() < limit,
                "{} jobs had not ended within {} s: {waiting:?}\n{}",
                waiting.len(),
                END_LIMIT.as_secs(),
                host::journal()
            );
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// What the test saw, for its output: the versions, how the jobs ended, the provider calls
    /// by method, and the Prepare calls that reached tierstore second by second.
    pub fn report(&self, ends: &[End], prepares: &[Instant], first_release: Instant) -> String {
        let mut text = format!(
            "js-agent {}, tierstore {}, trees.max_placements = {MAX_PLACEMENTS}\n",
            self.installed.agent, self.installed.tierstore
        );
        for end in ends {
            let _ = writeln!(text, "{}: {}", end.id, end.text());
        }
        let calls = self.provider.calls();
        for method in ["Sweep", "Prepare", "Mount", "Release"] {
            let reached = calls
                .iter()
                .filter(|call| call.method == method && call.reached)
                .count();
            let not = calls
                .iter()
                .filter(|call| call.method == method && !call.reached)
                .count();
            let _ = writeln!(
                text,
                "{method}: {reached} reached tierstore, {not} found it down"
            );
        }
        let since = |at: Instant| at.saturating_duration_since(self.started).as_secs();
        let _ = writeln!(
            text,
            "first Release at {} s; Prepare calls that reached tierstore per second (second: count):",
            since(first_release)
        );
        let mut seconds: Vec<(u64, usize)> = Vec::new();
        for at in prepares {
            let second = since(*at);
            match seconds.last_mut() {
                Some((last, count)) if *last == second => *count += 1,
                _ => seconds.push((second, 1)),
            }
        }
        let line: Vec<String> = seconds
            .iter()
            .map(|(second, count)| format!("{second}:{count}"))
            .collect();
        let _ = writeln!(text, "{}", line.join(" "));
        let busiest = busiest_window(prepares, crate::WINDOW);
        let _ = writeln!(
            text,
            "busiest 30 s: {} Prepare calls, from {:.1} s after the first",
            busiest.count,
            busiest.from.as_secs_f64()
        );
        text
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // The agent first, so that it does not go on calling a provider and a server that are
        // going away; the next test starts its own.
        host::stop(AGENT_UNIT);
    }
}

/// How a job ended, as the job service reads it back.
#[derive(Clone)]
pub struct End {
    /// The job's id.
    pub id: String,
    /// The job (`GET /v1/jobs/<id>`'s `job`).
    pub job: Value,
}

impl End {
    /// Whether the job succeeded.
    pub fn succeeded(&self) -> bool {
        self.job["state"] == "JOB_STATE_SUCCEEDED"
    }

    /// Whether the job's start failed because its input was unavailable on the host: the
    /// host's fault, not the job's.
    pub fn input_unavailable(&self) -> bool {
        self.job["state"] == "JOB_STATE_FAILED_TO_START"
            && self.job["startFailure"] == "START_FAILURE_CODE_INPUT_UNAVAILABLE"
    }

    /// The end in one line: the state, and the start failure's code and detail when it has one.
    pub fn text(&self) -> String {
        let field = |name: &str| self.job[name].as_str().unwrap_or_default().to_owned();
        let mut text = field("state");
        let (failure, detail) = (field("startFailure"), field("startFailureDetail"));
        if !failure.is_empty() || !detail.is_empty() {
            let _ = write!(text, " ({failure}: {detail})");
        }
        text
    }
}

/// The most instants of `times` in one window of `window`, and where that window starts, from
/// the first instant.
pub struct Busiest {
    /// The instants in the window.
    pub count: usize,
    /// Where it starts, after the first instant.
    pub from: Duration,
}

/// The window of `window` that holds the most of `times`, which are in order.
pub fn busiest_window(times: &[Instant], window: Duration) -> Busiest {
    let mut busiest = Busiest {
        count: 0,
        from: Duration::ZERO,
    };
    let mut start = 0;
    for (end, at) in times.iter().enumerate() {
        while *at >= times[start] + window {
            start += 1;
        }
        let count = end + 1 - start;
        if count > busiest.count {
            busiest = Busiest {
                count,
                from: times[start].saturating_duration_since(times[0]),
            };
        }
    }
    busiest
}
