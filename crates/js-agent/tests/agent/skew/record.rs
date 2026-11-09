//! The conversation the transcript holds, and how it is recorded: the agent program (its
//! hello builder, session and job worker, over a container runtime in memory) against a
//! lease server started in the test process with the job service on, every post and its
//! answer written down in order with the job API calls between them.
//!
//! Nothing here depends on a type of either side's wire: a frame is the JSON the transport
//! carried. The same code runs at any release that has the job service and the agent's
//! test harness (`v0.1.213` and later), so the transcript of the newest release is recorded
//! by the test [`super::this_version_still_says_what_the_previous_release_said`], which
//! writes it beside the test binary (see `docs/layering.md`, "The previous release of the
//! agent wire").

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::{Answer, Connector, Dialer, Handle, Transport};
use js_client::{Client, ClientError};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::{Cli, Config, Running, SharedClock, TickHandle, channel_ticker, start};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use crate::jobs::fake::FakeMachine;
use crate::jobs::{private, user};
use crate::server::Agent;
use crate::support::{Ran, Scratch, linux_host};

/// The wall time the server's clock is set to. The server reads time only through this
/// clock, which no step of the conversation moves: the frames carry no time.
const T0: u64 = 1_000_000;

/// The pool's join token (a test value: the server reads it from a file of the scratch).
const JOIN: &str = "join-token-S-0123456789-abcdefghijklmnop";

/// The job service's client token (a test value, like [`JOIN`]).
const CLIENT: &str = "client-token-S-0123456789-abcdefghijk";

/// The stand-in the transcript holds for the session secret the server drew: the secret
/// is random, so it is replaced wherever it appears.
pub const SESSION: &str = "previous-release-session";

/// A pool that books a host as 4 CPUs and 4 GiB and asks that it runs Linux on `x86_64`,
/// the facts of [`linux_host`].
const POOL: &str = r#"
name = "macs"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 4000
memory_bytes = 4294967296

[capabilities]
os = "linux"
arch = "x86_64"
"#;

/// The job the conversation submits: a pinned image, two CPUs, a megabyte, an hour.
fn job_body() -> Value {
    json!({
        "spec": {
            "image": "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "args": ["true"],
            "needs": { "cpuMillis": "2000", "memoryBytes": "1048576" },
            "wallLimitSeconds": "3600",
        }
    })
}

/// What happens between two of the agent's waits.
#[derive(Clone, Copy, Debug)]
pub enum Act {
    /// The submitter queues the job ([`job_body`]).
    Submit,
    /// The agent's wait passes: it posts one heartbeat and its worker acts on the reply.
    Beat,
    /// The submitter cancels the job.
    Cancel,
}

/// The conversation, after the hello: the job is queued, started by the next reply,
/// reported running, cancelled, stopped by a reply, reported killed and acknowledged; the
/// last heartbeat holds nothing. The agent is then asked to stop.
pub const SCENARIO: [Act; 8] = [
    Act::Submit,
    Act::Beat,
    Act::Beat,
    Act::Beat,
    Act::Cancel,
    Act::Beat,
    Act::Beat,
    Act::Beat,
];

/// The agent's flags beside `--server`, `--token-file` and `--pool`: a host id, and the
/// container runtime of the job worker over files under `files`.
pub fn agent_flags(files: &Scratch) -> Vec<String> {
    let exec = files.write("docker", "#!/bin/sh\n", 0o755);
    let (user, _, _) = user();
    let state = private(files, "state").display().to_string();
    let scratch = private(files, "run-scratch").display().to_string();
    [
        "--host-id",
        "h1",
        "--runtime",
        "docker",
        "--allow-container-engine",
        "--container-bin",
        &exec.display().to_string(),
        "--container-user",
        &user,
        "--container-subuid",
        "jobmap",
        "--state-root",
        &state,
        "--scratch-root",
        &scratch,
    ]
    .map(str::to_owned)
    .into()
}

/// The agent program on a thread of its own, its job worker and its machine.
pub struct Spawned {
    pub agent: Agent,
    pub worker: Handle,
    pub machine: Arc<FakeMachine>,
    _files: Scratch,
}

/// Starts the agent program against `url` through `connector` and waits for the wait after
/// its hello.
pub fn spawn(
    name: &str,
    url: String,
    token: &Path,
    connector: impl Connector + Send + 'static,
) -> Spawned {
    let files = Scratch::new(&format!("{name}-files"));
    let flags = agent_flags(&files);
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let machine = Arc::new(FakeMachine::new());
    let agent = Agent::spawn_on(
        url,
        token.to_path_buf(),
        &flags,
        linux_host,
        connector,
        machine.clone(),
    );
    let worker = agent.worker();
    Spawned {
        agent,
        worker,
        machine,
        _files: files,
    }
}

/// Plays [`SCENARIO`] on `running`: each beat lets one wait pass and waits for the next,
/// then for the worker to be done with the reply; `act` does the submitter's part. Returns
/// every wait the agent asked for (the first is the one after its hello) and what the
/// program printed once it was asked to stop.
pub fn play(running: Spawned, mut act: impl FnMut(Act)) -> (Vec<Duration>, Ran) {
    let Spawned {
        agent,
        worker,
        machine: _machine,
        _files,
    } = running;
    let mut waits = vec![agent.next_wait()];
    assert!(worker.settle());
    for step in SCENARIO {
        match step {
            Act::Beat => {
                agent.elapse();
                waits.push(agent.next_wait());
                assert!(worker.settle());
            }
            other => act(other),
        }
    }
    (waits, agent.stop())
}

/// A lease server in the test process with agent registration and the job service on.
pub struct Cell {
    runtime: Runtime,
    running: Option<Running>,
    _clock: ManualClock,
    _ticks: TickHandle,
    pub scratch: Scratch,
}

impl Cell {
    /// A server on a first boot over the pool [`POOL`].
    pub fn start(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let client = scratch.write("client-token", CLIENT, 0o600);
        let join = scratch.write("join-token", JOIN, 0o600);
        let pool = scratch.write("agent-pool.toml", POOL, 0o644);
        let path = |p: &Path| p.to_str().unwrap().to_owned();
        let argv = [
            "js-lease-server",
            "--listen",
            "127.0.0.1:0",
            "--boot",
            "initialise",
            "--state-file",
            &path(&scratch.path("leases.toml")),
            "--token-file",
            &path(&client),
            "--agent-token-file",
            &path(&join),
            "--agent-pool-config",
            &path(&pool),
            "--job-listen",
            "127.0.0.1:0",
        ];
        let config = Config::from_cli(Cli::try_parse_from(argv).unwrap()).unwrap();
        let clock = ManualClock::new(Timestamp::from_millis(T0));
        let (ticks, ticker) = channel_ticker();
        let runtime = Runtime::new().unwrap();
        let started = start(config, SharedClock::new(clock.clone()), ticker);
        let running = runtime.block_on(started).unwrap();
        Self {
            runtime,
            running: Some(running),
            _clock: clock,
            _ticks: ticks,
            scratch,
        }
    }

    fn running(&self) -> &Running {
        self.running.as_ref().expect("the server runs")
    }

    /// The agent API's address as `--server` takes it.
    pub fn url(&self) -> String {
        format!("http://{}", self.running().addr())
    }

    /// The file holding the join token.
    pub fn join_file(&self) -> std::path::PathBuf {
        self.scratch.path("join-token")
    }

    fn jobs(&self) -> Client {
        let addr = self.running().job_addr().expect("the job service is on");
        let token = self.scratch.path("client-token");
        Client::connect(
            &format!("http://{addr}"),
            &token,
            None,
            Duration::from_secs(10),
        )
        .unwrap()
    }

    /// Queues the job of the conversation; returns its id.
    pub fn submit(&self) -> String {
        let reply = self.jobs().post("/v1/jobs", &job_body()).unwrap();
        assert_eq!(reply.status, 201, "{}", reply.body);
        reply.body["job"]["id"].as_str().unwrap().to_owned()
    }

    /// Cancels the job `id`.
    pub fn cancel(&self, id: &str) {
        let path = format!("/v1/jobs/{id}:cancel");
        let reply = self.jobs().post(&path, &json!({})).unwrap();
        assert_eq!(reply.status, 200, "{}", reply.body);
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = self.runtime.block_on(running.shutdown());
        }
    }
}

/// One post and its answer as a transcript holds them.
pub fn exchange(path: &str, body: &Value, answer: &Answer) -> Value {
    let mut server = json!({ "status": answer.status, "body": answer.body });
    if let Some(after) = answer.retry_after {
        server["retry_after_s"] = json!(after.as_secs());
    }
    json!({ "agent": { "path": path, "body": body }, "server": server })
}

/// Writes down every post the agent makes through the real transport, and its answer.
#[derive(Clone, Default)]
pub struct Tape(pub Arc<Mutex<Vec<Value>>>);

struct Taping {
    inner: Box<dyn Transport>,
    flow: Arc<Mutex<Vec<Value>>>,
}

impl Connector for Tape {
    fn connect(
        &self,
        server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        let inner = Dialer.connect(server, token_file, ca_cert)?;
        Ok(Box::new(Taping {
            inner,
            flow: self.0.clone(),
        }))
    }
}

impl Transport for Taping {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        let answer = self.inner.post(path, body);
        let entry = match &answer {
            Ok(answer) => exchange(path, body, answer),
            Err(why) => json!({ "agent": { "path": path, "body": body }, "unreachable": why }),
        };
        self.flow.lock().unwrap().push(entry);
        answer
    }

    fn adopt_credential(&mut self, secret: &str) -> Result<(), String> {
        self.inner.adopt_credential(secret)
    }
}

/// Replaces every string equal to `from` in `value` with `to`.
pub fn replace_text(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(text) if text == from => *text = to.to_owned(),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| replace_text(item, from, to)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|item| replace_text(item, from, to)),
        _ => {}
    }
}

/// Records the conversation of this build's agent with this build's server, and the
/// server's answer to a hello that speaks majors 2 to 3 only (built from the agent's own
/// hello). The session secret is replaced by [`SESSION`].
pub fn record(name: &str) -> Value {
    let cell = Cell::start(name);
    let tape = Tape::default();
    let flow = tape.0.clone();
    let running = spawn(name, cell.url(), &cell.join_file(), tape);
    let mut job = None;
    let (waits, ran) = play(running, |act| {
        let id = job.get_or_insert_with(|| cell.submit()).clone();
        let event = match act {
            Act::Submit => json!({ "submitted": id }),
            Act::Cancel => {
                cell.cancel(&id);
                json!({ "cancelled": id })
            }
            Act::Beat => unreachable!("the play does the beats"),
        };
        flow.lock().unwrap().push(event);
    });
    assert!(
        waits.iter().all(|wait| *wait == Duration::from_secs(5)),
        "{waits:?}"
    );
    assert!(ran.err.contains("registered, epoch 1"), "{}", ran.err);
    let mut flow = std::mem::take(&mut *flow.lock().unwrap());

    let mut future = flow[0]["agent"]["body"].clone();
    future["protocol"] = json!({ "min": 2, "max": 3 });
    let mut transport = Dialer
        .connect(&cell.url(), &cell.join_file(), None)
        .unwrap();
    let answer = transport.post(js_agent::HELLO_PATH, &future).unwrap();
    let mut refusal = exchange(js_agent::HELLO_PATH, &future, &answer);

    let session = flow[0]["server"]["body"]["session"]
        .as_str()
        .expect("the welcome carries a session")
        .to_owned();
    let mut flow = Value::Array(std::mem::take(&mut flow));
    replace_text(&mut flow, &session, SESSION);
    replace_text(&mut refusal, &session, SESSION);
    json!({ "flow": flow, "refusal": refusal })
}
