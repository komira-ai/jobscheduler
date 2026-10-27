//! What the tests share: a lease server in this process, the CLI entry with captured
//! streams, and the waiters the tests drive.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use clap::Parser;
use js_cli::{ForwardedSignal, Services, SignalSource, Signaller, Waiter};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::{
    CapacitySource, Cli as ServerCli, Config, Running, SharedClock, StartupError, TickHandle,
    channel_ticker, start,
};
use js_pool_static::EXAMPLE_CONFIG;
use js_provider::{Call, Provider};
use js_provider_fake::FakeProvider;
use js_types::{HostId, ProviderId, Resources, Shape};
use serde_json::Value;
use tokio::runtime::{Handle, Runtime};

/// The wall time every test clock starts at.
pub const T0: u64 = 1_000_000;

/// The bearer token of the test servers.
pub const TOKEN: &str = "cli-test-token-0123456789";

/// The flags that ask for one whole host of the shipped pool.
pub const HOST: [&str; 10] = [
    "--class",
    "gpu-host",
    "--gpus",
    "1",
    "--gpu-class",
    "nvidia-a100",
    "--cpu-millis",
    "1000",
    "--memory-bytes",
    "1024",
];

/// A directory under the target directory that holds one test's files and is removed
/// when the test ends.
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    pub fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("js-cli")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Writes `text` to the file `name` with `mode` and returns its path.
    pub fn write(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// What a run of the CLI printed and returned.
#[derive(Debug)]
pub struct Outcome {
    pub code: u8,
    pub out: String,
    pub err: String,
}

impl Outcome {
    /// The stdout of a run that printed one JSON value.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.out).unwrap()
    }
}

/// How to reach a test server: enough to run the CLI against it from any thread.
#[derive(Clone)]
pub struct Js {
    pub server: String,
    pub token_file: PathBuf,
}

impl Js {
    /// The arguments of a command against this server, program name first.
    pub fn args(&self, extra: &[&str]) -> Vec<OsString> {
        let mut args: Vec<OsString> =
            vec!["js".into(), "--server".into(), self.server.clone().into()];
        args.push("--token-file".into());
        args.push(self.token_file.clone().into());
        args.extend(extra.iter().map(OsString::from));
        args
    }

    /// Runs `extra` with these services; the token must appear in nothing it printed.
    pub fn with(&self, extra: &[&str], services: Services) -> Outcome {
        run(self.args(extra), services)
    }

    /// Runs `extra` with the global flags after the subcommand.
    pub fn go_last(&self, extra: &[&str]) -> Outcome {
        let mut args: Vec<OsString> = vec!["js".into()];
        args.extend(extra.iter().map(OsString::from));
        args.extend(["--server".into(), self.server.clone().into()]);
        args.extend(["--token-file".into(), self.token_file.clone().into()]);
        run(args, services(Arc::new(Never)))
    }

    /// Runs `extra` with a waiter that must not be asked to wait.
    pub fn go(&self, extra: &[&str]) -> Outcome {
        self.with(extra, services(Arc::new(Never)))
    }

    /// Runs `extra` with this waiter.
    pub fn waiting(&self, extra: &[&str], waiter: Arc<dyn Waiter>) -> Outcome {
        self.with(extra, services(waiter))
    }
}

/// Runs the CLI on `args`, capturing both streams.
pub fn run(args: Vec<OsString>, services: Services) -> Outcome {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = js_cli::run_with(args, &mut out, &mut err, services);
    let out = String::from_utf8(out).unwrap();
    let err = String::from_utf8(err).unwrap();
    assert!(!out.contains(TOKEN) && !err.contains(TOKEN), "{out}{err}");
    Outcome { code, out, err }
}

/// Services over `waiter` and a signal source that reports nothing.
pub fn services(waiter: Arc<dyn Waiter>) -> Services {
    Services::new(waiter, Box::new(|_| Ok(())))
}

/// Services over `waiter` whose signal source hands its `Signaller` to the test.
pub fn services_with_signals(
    waiter: Arc<dyn Waiter>,
) -> (Services, std::sync::mpsc::Receiver<Signaller>) {
    let (handed, receiver) = std::sync::mpsc::channel();
    let source: SignalSource = Box::new(move |signaller| {
        handed.send(signaller).unwrap();
        Ok(())
    });
    (Services::new(waiter, source), receiver)
}

/// A waiter that fails the test when it is asked to wait.
pub struct Never;

impl Waiter for Never {
    fn wait(&self, interval: Duration) {
        panic!("waited {interval:?} when nothing should wait");
    }
}

/// A waiter that does `act` instead of waiting, and counts the intervals asked of it.
pub struct Driven {
    act: Box<dyn Fn() + Send + Sync>,
    asked: Mutex<Vec<Duration>>,
}

impl Driven {
    pub fn new(act: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            act: Box::new(act),
            asked: Mutex::new(Vec::new()),
        })
    }

    /// The intervals asked of the waiter so far.
    pub fn asked(&self) -> Vec<Duration> {
        self.asked.lock().unwrap().clone()
    }
}

impl Waiter for Driven {
    fn wait(&self, interval: Duration) {
        self.asked.lock().unwrap().push(interval);
        (self.act)();
    }
}

/// A waiter the test steps: each wait blocks until the test calls [`Manual::step`], and a
/// step returns once the CLI has handled that period's renewal and waits again.
pub struct Manual {
    state: Mutex<(u64, u64)>,
    changed: Condvar,
    intervals: Mutex<Vec<Duration>>,
}

/// The longest a test blocks on the CLI before it calls the CLI hung. Only a hang
/// reaches it; the CLI under test answers at once.
const HANG: Duration = Duration::from_secs(120);

type Guard<'a> = std::sync::MutexGuard<'a, (u64, u64)>;

impl Manual {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((0, 0)),
            changed: Condvar::new(),
            intervals: Mutex::new(Vec::new()),
        })
    }

    /// The intervals the CLI asked to wait, in order.
    pub fn intervals(&self) -> Vec<Duration> {
        self.intervals.lock().unwrap().clone()
    }

    fn until<'a>(&self, mut guard: Guard<'a>, ready: impl Fn(&(u64, u64)) -> bool) -> Guard<'a> {
        while !ready(&guard) {
            let (next, timeout) = self.changed.wait_timeout(guard, HANG).unwrap();
            assert!(!timeout.timed_out(), "the CLI never reached its waiter");
            guard = next;
        }
        guard
    }

    /// Lets the CLI's pending wait return, and returns when the CLI is waiting again:
    /// the renewal that wait led to is done.
    pub fn step(&self) {
        let guard = self.until(self.state.lock().unwrap(), |s| s.0 > s.1);
        let mut guard = guard;
        guard.1 += 1;
        let target = guard.1 + 1;
        self.changed.notify_all();
        drop(self.until(guard, |s| s.0 >= target));
    }

    /// Lets the CLI's pending wait return and does not wait for it to wait again: for
    /// the period after which the CLI stops asking for more.
    pub fn release(&self) {
        let mut guard = self.until(self.state.lock().unwrap(), |s| s.0 > s.1);
        guard.1 += 1;
        self.changed.notify_all();
    }

    /// Returns once the CLI is waiting for a period to pass: the lease is held, the
    /// command is running and the renewal clock has started.
    pub fn running(&self) {
        drop(self.until(self.state.lock().unwrap(), |s| s.0 > s.1));
    }
}

impl Waiter for Manual {
    fn wait(&self, interval: Duration) {
        self.intervals.lock().unwrap().push(interval);
        let mut guard = self.state.lock().unwrap();
        guard.0 += 1;
        let mine = guard.0;
        self.changed.notify_all();
        drop(self.until(guard, |s| s.1 >= mine));
    }
}

/// A capacity source of two fake hosts whose first calls are scripted to get no answer.
pub struct Scripted(pub Vec<(Call, bool)>);

impl CapacitySource for Scripted {
    fn provider(&self) -> Result<Box<dyn Provider + Send>, StartupError> {
        let resources = Resources::new(64_000, 1 << 30, 8, Some("nvidia-a100".into())).unwrap();
        let caps = BTreeMap::from([("isa".to_string(), "x86-64-v3".to_string())]);
        let shape = Shape::new(resources, caps);
        let hosts = ["h1", "h2"].map(|name| (HostId::new(name).unwrap(), shape.clone()));
        let mut fake = FakeProvider::new(ProviderId::new("fake").unwrap(), hosts.to_vec());
        for (call, landed) in &self.0 {
            fake.script_unknown(*call, *landed);
        }
        Ok(Box::new(fake))
    }
}

/// A lease server in this process, on a runtime of its own so the CLI can block the
/// test's thread, with a clock and a ticker the test moves.
pub struct Harness {
    pub rt: Runtime,
    pub running: Option<Running>,
    pub clock: ManualClock,
    pub ticks: TickHandle,
    pub scratch: Scratch,
    pub js: Js,
}

impl Harness {
    /// A plain-http server over the shipped two-host pool.
    pub fn new(name: &str) -> Self {
        Self::build(name, &[], None)
    }

    /// A server over two scripted fake hosts.
    pub fn scripted(name: &str, script: Vec<(Call, bool)>) -> Self {
        Self::build(name, &[], Some(Box::new(Scripted(script))))
    }

    /// A server with these extra server flags (and, when given, this capacity source).
    pub fn build(name: &str, extra: &[&str], source: Option<Box<dyn CapacitySource>>) -> Self {
        Self::build_in(Scratch::new(name), extra, source)
    }

    /// A server over TLS with a certificate made for the test; the certificate is in
    /// the file `cert.pem` of the scratch directory, for `--ca-cert`.
    pub fn tls(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let made = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let cert = scratch.write("cert.pem", &made.cert.pem(), 0o644);
        let key = scratch.write("key.pem", &made.signing_key.serialize_pem(), 0o600);
        let (cert, key) = (cert.to_str().unwrap(), key.to_str().unwrap());
        Self::build_in(scratch, &["--tls-cert", cert, "--tls-key", key], None)
    }

    /// A server in `scratch` with these extra server flags (and, when given, this
    /// capacity source).
    pub fn build_in(
        scratch: Scratch,
        extra: &[&str],
        source: Option<Box<dyn CapacitySource>>,
    ) -> Self {
        let pool = scratch.write("pool.toml", EXAMPLE_CONFIG, 0o644);
        let token_file = scratch.write("token", TOKEN, 0o600);
        let state = scratch.path("leases.toml");
        let mut args: Vec<OsString> = vec![
            "js-lease-server".into(),
            "--listen".into(),
            "127.0.0.1:0".into(),
            "--state-file".into(),
            state.into(),
            "--boot".into(),
            "initialise".into(),
            "--pool-config".into(),
            pool.into(),
            "--token-file".into(),
            token_file.clone().into(),
        ];
        args.extend(extra.iter().map(OsString::from));
        let mut config = Config::from_cli(ServerCli::try_parse_from(args).unwrap()).unwrap();
        if let Some(source) = source {
            config = config.with_capacity_source(source);
        }
        let rt = Runtime::new().unwrap();
        let clock = ManualClock::new(Timestamp::from_millis(T0));
        let (ticks, ticker) = channel_ticker();
        let shared = SharedClock::new(clock.clone());
        let running = rt.block_on(start(config, shared, ticker)).unwrap();
        let scheme = if extra.contains(&"--tls-cert") {
            "https"
        } else {
            "http"
        };
        let server = format!("{scheme}://{}", running.addr());
        Self {
            rt,
            running: Some(running),
            clock,
            ticks,
            scratch,
            js: Js { server, token_file },
        }
    }

    /// One tick of the server.
    pub fn tick(&self) {
        self.rt.block_on(self.ticks.step());
    }

    /// Moves the server's clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// A waiter that ticks this server instead of waiting.
    pub fn ticking(&self) -> Arc<Driven> {
        let (handle, ticks) = (self.rt.handle().clone(), self.ticks.clone());
        Driven::new(move || tick_on(&handle, &ticks))
    }

    /// Stops the server; later requests find nothing listening.
    pub fn stop(&mut self) {
        let running = self.running.take().unwrap();
        self.rt.block_on(running.shutdown()).unwrap();
    }

    /// The ids of the leases the server lists, read by the CLI.
    pub fn listed(&self) -> Vec<String> {
        let listed = self.js.go(&["--json", "lease", "list"]);
        assert_eq!(listed.code, 0, "{}", listed.err);
        let body = listed.json();
        let leases = body["leases"].as_array().unwrap();
        leases
            .iter()
            .map(|lease| lease["id"].as_str().unwrap().to_owned())
            .collect()
    }
}

/// One tick of a server, from a thread that is not on its runtime.
pub fn tick_on(handle: &Handle, ticks: &TickHandle) {
    handle.block_on(ticks.step());
}

/// A server that answers every request with `reply`, verbatim, and closes: for answers
/// the real server never gives. Returns its URL.
pub fn canned(reply: &str) -> String {
    let reply = reply.to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while !seen.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                seen.push(byte[0]);
            }
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    format!("http://{addr}")
}

/// A reply with this status line and body, ended by closing the connection.
pub fn http(status: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nConnection: close\r\n\r\n{body}")
}

/// A server that answers its requests one after the other with `replies` (the last one
/// for any request beyond them), each on a connection of its own. Returns its URL and
/// the request lines it has read, in order; a line is logged before its reply is sent.
pub fn sequence(replies: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&log);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let mut stream = stream.unwrap();
            let head = read_request(&mut stream);
            log.lock()
                .unwrap()
                .push(head.lines().next().unwrap().to_owned());
            let reply = replies.get(n).or(replies.last()).unwrap();
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (format!("http://{addr}"), seen)
}

/// Reads a request up to the end of its body (the head, as text).
fn read_request(stream: &mut impl Read) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let length = head
        .lines()
        .filter_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_owned)
        })
        .find_map(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    let _ = stream.read_exact(&mut body);
    head
}

/// Ends a `lease run` when dropped, by delivering `signal` to it: a test that fails
/// before it delivers the signal itself leaves no run behind to wait for.
pub struct EndsRun(pub Signaller, pub ForwardedSignal);

impl Drop for EndsRun {
    fn drop(&mut self) {
        self.0.deliver(self.1);
    }
}

/// The CLI against `server` with the token of `scratch`'s file.
pub fn against(server: &str, scratch: &Scratch) -> Js {
    Js {
        server: server.to_owned(),
        token_file: scratch.write("token", TOKEN, 0o600),
    }
}

/// The flags that ask for one whole host, less those `extra` gives its own value.
fn host_flags<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let kept = HOST.chunks(2).filter(|pair| !extra.contains(&pair[0]));
    kept.flatten().copied().collect()
}

/// The words of `lease acquire` for one whole host of the shipped pool, plus `extra`
/// (a flag in `extra` replaces the host's own).
pub fn acquire_words<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec!["lease", "acquire"];
    words.extend(host_flags(extra));
    words.extend(["--max-wall", "2h"]);
    words.extend(extra);
    words
}

/// The words of `lease run` for one whole host, with `extra` flags, then `--` and the
/// command.
pub fn run_words<'a>(extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec!["lease", "run"];
    words.extend(host_flags(extra));
    words.extend(["--max-wall", "2h"]);
    words.extend(extra);
    words.push("--");
    words.extend(command);
    words
}
