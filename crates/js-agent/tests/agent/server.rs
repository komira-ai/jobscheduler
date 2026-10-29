//! A lease server started in the test process with agent registration on, and an agent
//! run on a thread of its own whose waits the test opens one at a time.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use clap::Parser;
use js_agent::{
    Answer, Connector, Dialer, HEARTBEAT_PATH, Handle, Machine, Sleeper, Transport, Wake,
};
use js_agent_wire::ProtocolRange;
use js_client::{Client, ClientError, Reply};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::{Cli, Config, Running, SharedClock, TickHandle, channel_ticker, start};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use crate::jobs::fake::FakeMachine;
use crate::support::{Host, Lowest, Ran, Scratch, run_on};

/// The wall time the server's clock starts at.
pub const T0: u64 = 1_000_000;

/// The token of the lease routes.
pub const CLIENT_TOKEN: &str = "client-token-0123456789-abcdefghijkl";

/// The pool's join token.
pub const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

/// The pool agents join: a class of its own that asks nothing but a CPU and a little
/// memory, and the capability `os=macos`. It states no GPU, so a host with one or none
/// satisfies it.
pub const POOL_TOML: &str = r#"
name = "macs"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 1000
memory_bytes = 1073741824

[capabilities]
os = "macos"
"#;

/// A running server and the files it was started on.
pub struct Server {
    runtime: Arc<Runtime>,
    running: Running,
    pub clock: ManualClock,
    _ticks: TickHandle,
    pub scratch: Scratch,
    extra: Vec<String>,
    pool: &'static str,
}

/// A pool that asks nothing of a host: any machine fits it, as the real one running the
/// binary in a test does.
pub const POOL_OPEN_TOML: &str = r#"
name = "macs"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 1
memory_bytes = 1
"#;

/// The flags of a server with agents on, over `scratch`.
fn args(scratch: &Scratch, boot: &str, extra: &[String], pool_text: &str) -> Vec<String> {
    let client = scratch.write("client-token", CLIENT_TOKEN, 0o600);
    let join = scratch.write("join-token", JOIN, 0o600);
    let pool = scratch.write("agent-pool.toml", pool_text, 0o644);
    let path = |p: &Path| p.to_str().unwrap().to_owned();
    let mut all: Vec<String> = [
        "js-lease-server",
        "--listen",
        "127.0.0.1:0",
        "--boot",
        boot,
        "--state-file",
        &path(&scratch.path("leases.toml")),
        "--token-file",
        &path(&client),
        "--agent-token-file",
        &path(&join),
        "--agent-pool-config",
        &path(&pool),
    ]
    .map(str::to_owned)
    .into();
    all.extend_from_slice(extra);
    all
}

fn launch(
    runtime: Arc<Runtime>,
    scratch: Scratch,
    clock: ManualClock,
    boot: &str,
    extra: Vec<String>,
    pool: &'static str,
) -> Server {
    let argv = args(&scratch, boot, &extra, pool);
    let config = Config::from_cli(Cli::try_parse_from(argv).unwrap());
    let (ticks, ticker) = channel_ticker();
    let started = start(config.unwrap(), SharedClock::new(clock.clone()), ticker);
    let running = runtime.block_on(started).unwrap();
    Server {
        runtime,
        running,
        clock,
        _ticks: ticks,
        scratch,
        extra,
        pool,
    }
}

impl Server {
    /// A server on a first boot, with `extra` flags.
    pub fn start(name: &str, extra: &[&str]) -> Self {
        Self::start_pool(name, extra, POOL_TOML)
    }

    /// A server on a first boot over the pool file `pool`.
    pub fn start_pool(name: &str, extra: &[&str], pool: &'static str) -> Self {
        let runtime = Arc::new(Runtime::new().unwrap());
        let clock = ManualClock::new(Timestamp::from_millis(T0));
        let extra = extra.iter().map(|flag| (*flag).to_owned()).collect();
        launch(
            runtime,
            Scratch::new(name),
            clock,
            "initialise",
            extra,
            pool,
        )
    }

    /// Stops the server and starts another over its files, as the program does after a
    /// restart: a new registry, nothing carried over but the files.
    pub fn restart(self) -> Self {
        let Self {
            runtime,
            running,
            clock,
            scratch,
            extra,
            pool,
            ..
        } = self;
        let stopped =
            runtime.block_on(async { tokio::time::timeout(HANG_GUARD, running.shutdown()).await });
        stopped.expect("the server did not shut down").unwrap();
        launch(runtime, scratch, clock, "resume", extra, pool)
    }

    /// The server's address as `--server` takes it.
    pub fn url(&self) -> String {
        format!("http://{}", self.running.addr())
    }

    /// The file holding the join token.
    pub fn join_file(&self) -> PathBuf {
        self.scratch.path("join-token")
    }

    fn client(&self) -> Client {
        let file = self.scratch.path("client-token");
        Client::connect(&self.url(), &file, None, Duration::from_secs(10)).unwrap()
    }

    /// `POST /v1/leases` for a macOS host, by capability.
    pub fn lease(&self) -> Reply {
        let body = json!({
            "class": "mac",
            "resources": { "cpu_millis": 1000, "memory_bytes": 1024 },
            "capabilities": { "os": "macos" },
            "max_wall_seconds": 3600,
        });
        self.client().post("/v1/leases", &body).unwrap()
    }

    /// `GET path` with the lease token.
    pub fn get(&self, path: &str) -> Value {
        self.client().get(path).unwrap().body
    }

    /// The entry of `host` in `GET /v1/hosts`.
    pub fn host(&self, host: &str) -> Value {
        let listing = self.get("/v1/hosts");
        let hosts = listing["hosts"].as_array().unwrap();
        let found = hosts.iter().find(|entry| entry["id"] == host);
        found
            .unwrap_or_else(|| panic!("{host} is not listed: {listing}"))
            .clone()
    }

    /// `DELETE path` with the lease token.
    pub fn delete(&self, path: &str) -> Reply {
        self.client().delete(path).unwrap()
    }

    /// `POST path` with the lease token and no body.
    pub fn post(&self, path: &str) -> Reply {
        self.client().post(path, &json!({})).unwrap()
    }

    /// `POST path` with the lease token and `body`.
    pub fn post_body(&self, path: &str, body: &Value) -> Reply {
        self.client().post(path, body).unwrap()
    }
}

/// A pool that books a host as 4 CPUs and 4 GiB and asks only that it runs Linux: a machine
/// that offers more joins it, and a lease on it holds runs up to that shape.
pub const POOL_RUNS_TOML: &str = r#"
name = "macs"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 4000
memory_bytes = 4294967296

[capabilities]
os = "linux"
"#;

/// Opens the waits of an agent one at a time.
struct Gate {
    slept: Sender<Duration>,
    go: Receiver<Wake>,
}

/// How long a test waits for the agent, and the agent's gate for the test, before the
/// test fails. It is a guard against a hang (a regression that leaves one side waiting
/// for ever would otherwise stall the whole run), never an expectation: a healthy side
/// answers within milliseconds and the tests assert on what was said, not on how soon.
const HANG_GUARD: Duration = Duration::from_secs(120);

impl Sleeper for Gate {
    fn sleep(&mut self, duration: Duration) -> Wake {
        let _ = self.slept.send(duration);
        match self.go.recv_timeout(HANG_GUARD) {
            Ok(wake) => wake,
            Err(RecvTimeoutError::Disconnected) => Wake::Stop,
            Err(RecvTimeoutError::Timeout) => {
                panic!("the test never opened the agent's wait of {duration:?}")
            }
        }
    }
}

/// An agent running on a thread, held at each of its waits until the test lets it go.
pub struct Agent {
    slept: Receiver<Duration>,
    go: Sender<Wake>,
    handle: JoinHandle<Ran>,
    worker: Receiver<Handle>,
}

impl Agent {
    /// Runs the agent of `host` against `url`, with the join token in `token`, and `extra`
    /// flags. It uses the real transport, through `connector`, and a machine whose runtime
    /// is in memory.
    pub fn spawn(
        url: String,
        token: PathBuf,
        extra: &[&str],
        host: fn() -> Host,
        connector: impl Connector + Send + 'static,
    ) -> Self {
        let machine = Arc::new(FakeMachine::new());
        Self::spawn_on(url, token, extra, host, connector, machine)
    }

    /// [`Agent::spawn`] over `machine`, which the test keeps a handle to.
    pub fn spawn_on(
        url: String,
        token: PathBuf,
        extra: &[&str],
        host: fn() -> Host,
        connector: impl Connector + Send + 'static,
        machine: Arc<FakeMachine>,
    ) -> Self {
        Self::launch(url, Some(token), extra, host, connector, machine)
    }

    /// [`Agent::spawn`] with no `--token-file`: the agent finds its credential in the state
    /// root `extra` names.
    pub fn spawn_tokenless(
        url: String,
        extra: &[&str],
        host: fn() -> Host,
        connector: impl Connector + Send + 'static,
    ) -> Self {
        let machine = Arc::new(FakeMachine::new());
        Self::launch(url, None, extra, host, connector, machine)
    }

    fn launch(
        url: String,
        token: Option<PathBuf>,
        extra: &[&str],
        host: fn() -> Host,
        connector: impl Connector + Send + 'static,
        machine: Arc<FakeMachine>,
    ) -> Self {
        let (tell, worker) = channel();
        let (slept, waits) = channel();
        let (go, gate) = channel();
        let mut args: Vec<String> = vec!["--server".into(), url];
        if let Some(token) = token {
            args.extend([
                "--token-file".to_owned(),
                token.to_str().unwrap().to_owned(),
            ]);
        }
        if !extra.contains(&"--pool") {
            args.extend(["--pool".to_owned(), "macs".to_owned()]);
        }
        args.extend(extra.iter().map(|flag| (*flag).to_owned()));
        // The token is a file's content: only the file's path is an argument.
        assert!(!args.concat().contains(JOIN));
        let handle = thread::spawn(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let mut sleeper = Gate { slept, go: gate };
            let hook = move |worker: Handle| {
                let _ = tell.send(worker);
            };
            let machine: Arc<dyn Machine> = machine;
            let jobs = (machine, Some(&hook as &dyn Fn(Handle)));
            run_on(&args, &host(), &connector, &mut sleeper, &mut Lowest, jobs)
        });
        Self {
            slept: waits,
            go,
            handle,
            worker,
        }
    }

    /// The handle to the agent's job worker, once it has started: an agent started with
    /// `--container-bin` has one before its first hello.
    pub fn worker(&self) -> Handle {
        self.worker
            .recv_timeout(HANG_GUARD)
            .expect("the agent started no job worker")
    }

    /// Delivers a stop request, as a signal would, without waiting for the agent to end.
    pub fn signal(&self) {
        self.go.send(Wake::Stop).unwrap();
    }

    /// Waits until the agent has done its next exchange and returns the wait it asks for.
    pub fn next_wait(&self) -> Duration {
        self.slept
            .recv_timeout(HANG_GUARD)
            .expect("the agent ended, or said nothing for a long time, before it waited")
    }

    /// Lets the agent's current wait pass.
    pub fn elapse(&self) {
        self.go.send(Wake::Elapsed).unwrap();
    }

    /// Asks the agent to stop, as a signal would, and returns what it printed once it ends.
    pub fn stop(self) -> Ran {
        self.go.send(Wake::Stop).unwrap();
        self.handle.join().unwrap()
    }

    /// Waits for an agent that ends by itself.
    pub fn finish(self) -> Ran {
        self.handle.join().unwrap()
    }
}

/// Sends every post to whatever address the test names last, so the agent follows a server
/// that restarted on another port.
#[derive(Clone)]
pub struct Redirect(pub Arc<Mutex<String>>);

struct Redirected {
    to: Arc<Mutex<String>>,
    token_file: PathBuf,
    credential: Option<String>,
}

impl Connector for Redirect {
    fn connect(
        &self,
        _server: &str,
        token_file: &Path,
        _ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        Ok(Box::new(Redirected {
            to: self.0.clone(),
            token_file: token_file.to_path_buf(),
            credential: None,
        }))
    }
}

impl Transport for Redirected {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        let url = self.to.lock().unwrap().clone();
        Dialer
            .connect(&url, &self.token_file, None)
            .map_err(|err| err.to_string())
            .and_then(|mut transport| {
                if let Some(credential) = &self.credential {
                    transport.adopt_credential(credential)?;
                }
                transport.post(path, body)
            })
    }

    fn adopt_credential(&mut self, secret: &str) -> Result<(), String> {
        self.credential = Some(secret.to_owned());
        Ok(())
    }
}

/// Says in every hello that the agent speaks these majors instead of its own, as an agent
/// of another version would, so a real server refuses it.
pub struct OtherMajors(pub ProtocolRange);

struct Rewriting {
    inner: Box<dyn Transport>,
    majors: ProtocolRange,
}

impl Connector for OtherMajors {
    fn connect(
        &self,
        server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        let inner = Dialer.connect(server, token_file, ca_cert)?;
        Ok(Box::new(Rewriting {
            inner,
            majors: self.0,
        }))
    }
}

impl Transport for Rewriting {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        let mut body = body.clone();
        body["protocol"] = json!({ "min": self.majors.min, "max": self.majors.max });
        self.inner.post(path, &body)
    }

    fn adopt_credential(&mut self, secret: &str) -> Result<(), String> {
        self.inner.adopt_credential(secret)
    }
}

/// Counts the heartbeats the agent posts.
#[derive(Clone, Default)]
pub struct Counting(pub Arc<std::sync::atomic::AtomicUsize>);

struct Counted {
    inner: Box<dyn Transport>,
    beats: Arc<std::sync::atomic::AtomicUsize>,
}

impl Connector for Counting {
    fn connect(
        &self,
        server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        let inner = Dialer.connect(server, token_file, ca_cert)?;
        Ok(Box::new(Counted {
            inner,
            beats: self.0.clone(),
        }))
    }
}

impl Transport for Counted {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        let beat = usize::from(path == HEARTBEAT_PATH);
        self.beats
            .fetch_add(beat, std::sync::atomic::Ordering::SeqCst);
        self.inner.post(path, body)
    }

    fn adopt_credential(&mut self, secret: &str) -> Result<(), String> {
        self.inner.adopt_credential(secret)
    }
}

/// Records the body of every answer, so a test can look for what must not be printed.
#[derive(Clone, Default)]
pub struct Spy(pub Arc<Mutex<Vec<Value>>>);

struct Spying {
    inner: Box<dyn Transport>,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Connector for Spy {
    fn connect(
        &self,
        server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        let inner = Dialer.connect(server, token_file, ca_cert)?;
        Ok(Box::new(Spying {
            inner,
            seen: self.0.clone(),
        }))
    }
}

impl Transport for Spying {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        let answer = self.inner.post(path, body);
        if let Ok(answer) = &answer {
            self.seen.lock().unwrap().push(answer.body.clone());
        }
        answer
    }

    fn adopt_credential(&mut self, secret: &str) -> Result<(), String> {
        self.inner.adopt_credential(secret)
    }
}

/// Records the body of every heartbeat the agent posts.
#[derive(Clone, Default)]
pub struct Recorder(pub Arc<Mutex<Vec<Value>>>);

struct Recording {
    inner: Box<dyn Transport>,
    posted: Arc<Mutex<Vec<Value>>>,
}

impl Connector for Recorder {
    fn connect(
        &self,
        server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        let inner = Dialer.connect(server, token_file, ca_cert)?;
        Ok(Box::new(Recording {
            inner,
            posted: self.0.clone(),
        }))
    }
}

impl Transport for Recording {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        if path == HEARTBEAT_PATH {
            self.posted.lock().unwrap().push(body.clone());
        }
        self.inner.post(path, body)
    }
}
