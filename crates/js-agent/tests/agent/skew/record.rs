//! The conversation the transcript holds, and how it is recorded: the agent program (its
//! hello builder, session and job worker, over a container runtime in memory) against a
//! lease server started in the test process with the job service on, every post and its
//! answer written down in order with the job API calls between them, and the messages of the
//! dispatch poll (the agent's second route for a job start and stop, which a server that
//! welcomes it with `dispatch_poll` uses in place of the heartbeat reply) beside them, under
//! `dispatched`. The poll's answers are held back from the agent until the recording lets
//! them go, so that the order of the poll and of the heartbeats is the recording's, not a
//! race.
//!
//! No frame is built from a wire type of either side: a frame is the JSON the transport
//! carried. The code uses only the agent program's test harness and the lease server's job
//! service, so it can be applied to a release tag that predates it (it was to `v0.1.213`;
//! a later tag whose harness changed may need it adapted), and the transcript of a release
//! is recorded by the test [`super::this_version_still_says_what_the_previous_release_said`], which
//! writes it beside the test binary (see `docs/layering.md`, "The previous release of the
//! agent wire").

use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::mpsc::{Receiver, Sender, channel};
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

#[cfg(target_os = "linux")]
use js_agent::{Answer, Connector, Dialer, Handle, Login, Transport};
use js_client::Client;
#[cfg(target_os = "linux")]
use js_client::ClientError;
use js_lease_server::{Cli, Config, Running, SharedClock, TickHandle, channel_ticker, start};
use js_types::{ManualClock, Timestamp};
use serde_json::{Value, json};
use tokio::runtime::{Builder, Runtime};

#[cfg(target_os = "linux")]
use crate::jobs::fake::{FakeMachine, HANG_GUARD};
#[cfg(target_os = "linux")]
use crate::jobs::{private, user};
#[cfg(target_os = "linux")]
use crate::server::Agent;
use crate::support::Scratch;
#[cfg(target_os = "linux")]
use crate::support::{Ran, linux_host};

/// The wall time the server's clock is set to. The server reads time only through this
/// clock, which no step of the conversation moves: the frames carry no time.
const T0: u64 = 1_000_000;

/// [`POOL`] with the least heartbeat interval a pool takes, [`QUICK_INTERVAL_MS`]. The
/// recording uses it: a server holds the reply of a heartbeat that has nothing to send, to an
/// agent that reports `held-beat` (this build's does), for the interval at most, in real time,
/// and the conversation has four such beats, which the default 5 s would make twenty seconds
/// of the test. The previous release's transcript says 5000 where this one says
/// [`QUICK_INTERVAL_MS`]; that is the one difference the comparison allows itself
/// ([`super::this_version_still_says_what_the_previous_release_said`]). The replay of the
/// previous agent runs against [`POOL`], whose hello is not `held-beat`.
#[cfg(target_os = "linux")]
const QUICK_POOL: &str = r#"
heartbeat_interval = "250ms"
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

/// The heartbeat interval of [`QUICK_POOL`], in milliseconds, as its welcome carries it.
pub const QUICK_INTERVAL_MS: u64 = 250;

/// The pool's join token (a test value: the server reads it from a file of the scratch).
const JOIN: &str = "join-token-S-0123456789-abcdefghijklmnop";

/// The job service's client token (a test value, like [`JOIN`]).
const CLIENT: &str = "client-token-S-0123456789-abcdefghijk";

/// The stand-in the transcript holds for the session secret the server drew: the secret
/// is random, so it is replaced wherever it appears.
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
pub enum Act {
    /// The submitter queues the job ([`job_body`]).
    Submit,
    /// The agent's wait passes: it posts one heartbeat and its worker acts on the reply.
    Beat,
    /// The submitter cancels the job.
    Cancel,
    /// The dispatch poll's answer that the server handed out at the cancel, held until now,
    /// reaches the agent, and its worker has acted on it.
    Deliver,
}

/// The conversation, after the hello: the job is queued, started (the start leaves in the
/// dispatch poll, before the first beat), reported running, cancelled, the stop handed out by
/// the poll but held from the agent for one beat (which therefore reports the job running and
/// is answered with no command, where the previous release's reply carried the stop),
/// delivered, the job reported killed and acknowledged; the last heartbeat holds nothing. The
/// agent is then asked to stop.
#[cfg(target_os = "linux")]
pub const SCENARIO: [Act; 9] = [
    Act::Submit,
    Act::Beat,
    Act::Beat,
    Act::Beat,
    Act::Cancel,
    Act::Beat,
    Act::Deliver,
    Act::Beat,
    Act::Beat,
];

/// The agent's flags beside `--server`, `--token-file` and `--pool`: a host id, and the
/// container runtime of the job worker over files under `files`.
#[cfg(target_os = "linux")]
pub fn agent_flags(files: &Scratch) -> Vec<String> {
    let exec = files.write("docker", "#!/bin/sh\n", 0o755);
    let (user, _, _) = user();
    let state = private(files, "state").display().to_string();
    let scratch = private(files, "run-scratch").display().to_string();
    [
        "--host-id",
        "h1",
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
#[cfg(target_os = "linux")]
pub struct Spawned {
    pub agent: Agent,
    pub worker: Handle,
    pub machine: Arc<FakeMachine>,
    _files: Scratch,
}

/// Starts the agent program against `url` through `connector` and waits for the wait after
/// its hello.
#[cfg(target_os = "linux")]
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

/// [`spawn`] for an agent that polls the dispatch route: `connector` serves its heartbeats
/// and `dispatch` its polls.
#[cfg(target_os = "linux")]
pub fn spawn_polling(
    name: &str,
    url: String,
    token: &Path,
    connectors: (impl Connector + Send + 'static, Listening),
) -> Spawned {
    let files = Scratch::new(&format!("{name}-files"));
    let flags = agent_flags(&files);
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let machine = Arc::new(FakeMachine::new());
    let (connector, dispatch) = connectors;
    let agent = Agent::spawn_polling(
        url,
        token.to_path_buf(),
        &flags,
        linux_host,
        (connector, Arc::new(dispatch)),
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
#[cfg(target_os = "linux")]
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
            other => {
                act(other);
                // The worker has acted on what the act made reach the agent.
                assert!(worker.settle());
            }
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
        Self::start_over(name, POOL)
    }

    /// A server on a first boot over the pool [`QUICK_POOL`].
    #[cfg(target_os = "linux")]
    pub fn start_quick(name: &str) -> Self {
        Self::start_over(name, QUICK_POOL)
    }

    fn start_over(name: &str, pool: &str) -> Self {
        let scratch = Scratch::new(name);
        let client = scratch.write("client-token", CLIENT, 0o600);
        let join = scratch.write("join-token", JOIN, 0o600);
        let pool = scratch.write("agent-pool.toml", pool, 0o644);
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
        // Two workers, not one per core: the test binary's address space is capped
        // (`bounds`), and every worker thread reserves its own malloc arena, so several
        // cells started at once on a machine of many cores would exceed the cap.
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
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
#[cfg(target_os = "linux")]
pub fn exchange(path: &str, body: &Value, answer: &Answer) -> Value {
    let mut server = json!({ "status": answer.status, "body": answer.body });
    if let Some(after) = answer.retry_after {
        server["retry_after_s"] = json!(after.as_secs());
    }
    json!({ "agent": { "path": path, "body": body }, "server": server })
}

/// Writes down every post the agent makes through the real transport, and its answer.
#[derive(Clone, Default)]
#[cfg(target_os = "linux")]
pub struct Tape(pub Arc<Mutex<Vec<Value>>>);

#[cfg(target_os = "linux")]
struct Taping {
    inner: Box<dyn Transport>,
    flow: Arc<Mutex<Vec<Value>>>,
}

#[cfg(target_os = "linux")]
impl Connector for Tape {
    fn connect(
        &self,
        server: &str,
        login: &Login,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        let inner = Dialer.connect(server, login, ca_cert)?;
        Ok(Box::new(Taping {
            inner,
            flow: self.0.clone(),
        }))
    }
}

#[cfg(target_os = "linux")]
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

/// What the dispatch poll's transports tell the recording.
#[cfg(target_os = "linux")]
enum Poll {
    /// A poll was sent.
    Sent,
    /// A poll was answered with messages, which are written down.
    Handed,
}

/// The connector of the agent's dispatch poll: the real transport, writing down each message
/// the server hands out (in the order handed, as the JSON the transport carried) and telling
/// the recording when a poll was sent and when one was answered with messages.
#[cfg(target_os = "linux")]
pub struct Listening {
    handed: Arc<Mutex<Vec<Value>>>,
    events: Mutex<Sender<Poll>>,
    gate: Arc<Mutex<Receiver<()>>>,
    open: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
struct Listened {
    inner: Box<dyn Transport>,
    handed: Arc<Mutex<Vec<Value>>>,
    events: Sender<Poll>,
    gate: Arc<Mutex<Receiver<()>>>,
    open: Arc<AtomicBool>,
}

/// The messages the poll is handed, and the events of the poll in the order they happen.
#[cfg(target_os = "linux")]
pub struct Polls {
    pub handed: Arc<Mutex<Vec<Value>>>,
    events: Receiver<Poll>,
    release: Sender<()>,
    open: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
impl Polls {
    /// A poll connector and what it reports.
    pub fn new() -> (Listening, Self) {
        let handed = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = channel();
        let (release, gate) = channel();
        let open = Arc::new(AtomicBool::new(false));
        let listening = Listening {
            handed: handed.clone(),
            events: Mutex::new(events),
            gate: Arc::new(Mutex::new(gate)),
            open: open.clone(),
        };
        (
            listening,
            Self {
                handed,
                events: receiver,
                release,
                open,
            },
        )
    }

    /// Waits until a poll was answered with messages. The answer is held back from the agent
    /// until [`Polls::release`].
    #[track_caller]
    pub fn handed(&self) {
        loop {
            let event = self.events.recv_timeout(HANG_GUARD);
            if matches!(event.expect("the agent polled"), Poll::Handed) {
                break;
            }
        }
    }

    /// Lets the held answer reach the agent and waits until the poll after it was sent, which
    /// the poller does only once it has given the messages to the job worker. With `rest`, no
    /// later answer is held either.
    #[track_caller]
    pub fn release(&self, rest: bool) {
        self.open.store(rest, Ordering::SeqCst);
        self.release.send(()).expect("the held poll is waiting");
        loop {
            let event = self.events.recv_timeout(HANG_GUARD);
            if matches!(event.expect("the agent polled"), Poll::Sent) {
                break;
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Connector for Listening {
    fn connect(
        &self,
        server: &str,
        login: &Login,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        let events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(Box::new(Listened {
            inner: Dialer.connect(server, login, ca_cert)?,
            handed: self.handed.clone(),
            events: events.clone(),
            gate: self.gate.clone(),
            open: self.open.clone(),
        }))
    }
}

#[cfg(target_os = "linux")]
impl Transport for Listened {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        let _ = self.events.send(Poll::Sent);
        let answer = self.inner.post(path, body)?;
        let messages = answer.body["messages"].as_array().filter(|m| !m.is_empty());
        if let (200, Some(messages)) = (answer.status, messages) {
            let mut handed = self.handed.lock().unwrap_or_else(PoisonError::into_inner);
            handed.extend(messages.iter().cloned());
            drop(handed);
            let _ = self.events.send(Poll::Handed);
            if !self.open.load(Ordering::SeqCst) {
                let gate = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
                gate.recv_timeout(HANG_GUARD)
                    .map_err(|_| "the test never released the poll's answer".to_owned())?;
            }
        }
        Ok(answer)
    }

    fn adopt_credential(&mut self, secret: &str) -> Result<(), String> {
        self.inner.adopt_credential(secret)
    }
}

/// Replaces every string equal to `from` in `value` with `to`.
#[cfg(target_os = "linux")]
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
/// server's refusal (asserted: status 400, `unsupported-protocol`) of a hello that speaks
/// majors 2 to 3 only, majors above the one server and agent speak (built from the agent's own
/// hello). The session secret is replaced by [`SESSION`].
#[cfg(target_os = "linux")]
pub fn record(name: &str) -> Value {
    let cell = Cell::start_quick(name);
    let tape = Tape::default();
    let flow = tape.0.clone();
    let (listening, polls) = Polls::new();
    let running = spawn_polling(name, cell.url(), &cell.join_file(), (tape, listening));
    let mut job = None;
    let (waits, ran) = play(running, |act| {
        let id = job.get_or_insert_with(|| cell.submit()).clone();
        let event = match act {
            Act::Submit => {
                // The start leaves in the dispatch poll, not in a heartbeat's reply.
                polls.handed();
                polls.release(false);
                json!({ "submitted": id })
            }
            Act::Cancel => {
                cell.cancel(&id);
                polls.handed();
                json!({ "cancelled": id })
            }
            Act::Deliver => {
                polls.release(true);
                return;
            }
            Act::Beat => unreachable!("the play does the beats"),
        };
        flow.lock().unwrap().push(event);
    });
    let interval = Duration::from_millis(QUICK_INTERVAL_MS);
    let held = Duration::ZERO;
    // After the hello and each beat: the interval, but none after a reply that says
    // `beat_now`: one the server held open for the beat (nothing to send), or one that
    // carries only an acknowledgement. The server leaves the start and the stop out of this
    // agent's heartbeat replies (the dispatch poll hands them out), so no reply carries a
    // command and only the hello's welcome is followed by the interval. In order: the
    // hello's welcome (interval); the beat after the submit, to which the start was already
    // delivered by the poll (held); the beat that reports the job running (held); the next
    // (held); the beat after the cancel, which reports the job running while the poll's
    // stop is held back (held); the beat that reports the job killed, answered with its
    // acknowledgement (beat_now); the last beat (held).
    let expected = [interval, held, held, held, held, held, held];
    assert_eq!(waits, expected, "{waits:?}");
    assert!(ran.err.contains("registered, epoch 1"), "{}", ran.err);
    let mut flow = std::mem::take(&mut *flow.lock().unwrap());

    let mut future = flow[0]["agent"]["body"].clone();
    future["protocol"] = json!({ "min": 2, "max": 3 });
    let mut transport = Dialer
        .connect(&cell.url(), &Login::Token(cell.join_file()), None)
        .unwrap();
    let answer = transport.post(js_agent::HELLO_PATH, &future).unwrap();
    assert_eq!(answer.status, 400, "{}", answer.body);
    assert_eq!(
        answer.body["error"], "unsupported-protocol",
        "{}",
        answer.body
    );
    let mut refusal = exchange(js_agent::HELLO_PATH, &future, &answer);

    let session = flow[0]["server"]["body"]["session"]
        .as_str()
        .expect("the welcome carries a session")
        .to_owned();
    let mut flow = Value::Array(std::mem::take(&mut flow));
    replace_text(&mut flow, &session, SESSION);
    replace_text(&mut refusal, &session, SESSION);
    let mut dispatched = Value::Array(std::mem::take(&mut *polls.handed.lock().unwrap()));
    replace_text(&mut dispatched, &session, SESSION);
    json!({ "flow": flow, "dispatched": dispatched, "refusal": refusal })
}
