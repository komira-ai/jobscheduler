//! What the tests share: scratch directories, flags, a running server and a small
//! HTTP/1.1 client.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_lease::Booking;
use js_lease_server::{
    CapacitySource, Cli, Config, Limits, Running, Seated, SharedClock, StartupError, TickHandle,
    Ticker, channel_ticker, start_with_checks,
};
use js_pool_static::{EXAMPLE_CONFIG, StaticPool};
use js_provider::Pool;
use js_state::Store;
use js_testkit::dir::TestDir;
use js_types::ProviderId;
use js_types::{ManualClock, Timestamp};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::store_support::{sqlite_line, store_file};

/// The wall time every test clock starts at.
pub const T0: u64 = 1_000_000;

/// The bearer token of the test servers.
pub const TOKEN: &str = "test-token-0123456789-abcdefghijkl";

/// A directory under the target directory that holds one test's files and is removed
/// when the test ends.
pub struct Scratch {
    pub dir: PathBuf,
    _owner: TestDir,
}

impl Scratch {
    /// A new, empty directory named for `name`. Tests run side by side, so a name that two
    /// tests use must not be the same directory: every call gets a directory of its own.
    pub fn new(name: &str) -> Self {
        let owner = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-lease-server"),
            name,
        );
        Self {
            dir: owner.path().to_path_buf(),
            _owner: owner,
        }
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

    pub fn state(&self) -> PathBuf {
        self.path("leases.toml")
    }
}

/// Asserts that two scratch directories made under one name are two directories: the
/// second one's set-up does not remove the first one's files and dropping the second does
/// not remove the first. It catches a path named by the test alone, which two tests of
/// one name, or two runs of one test binary at once, would share and delete under each
/// other.
#[test]
fn scratches_of_one_name_are_separate_and_each_removes_only_itself() {
    let first = Scratch::new("same-name");
    first.write("f", "one", 0o600);
    let second = Scratch::new("same-name");
    assert_ne!(first.path("f"), second.path("f"));
    assert!(
        first.path("f").exists(),
        "a second scratch removed the first"
    );
    second.write("f", "two", 0o600);
    let kept = first.path("f");
    drop(second);
    assert_eq!(fs::read(kept).unwrap(), b"one");
}

/// Everything any test's server has logged on a thread with no capture of its own,
/// through [`quiet_logs`]'s subscriber.
static EVERYTHING: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

/// A writer into [`EVERYTHING`].
struct Everything;

impl std::io::Write for Everything {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        EVERYTHING.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs the process-wide subscriber, so the log calls of the server run (a call with
/// no subscriber skips evaluating its fields) and so a line logged from any thread,
/// the pool for blocking calls included, can be read back with [`logged_with`]. A test
/// that starts a [`LogCapture`] reads its own thread's lines there instead.
pub fn quiet_logs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(|| Everything)
            .try_init();
    });
}

/// The lines logged by the process-wide subscriber that contain `needle`. Tests run side
/// by side, so a test names something only it logs (a host id of its own).
pub fn logged_with(needle: &str) -> Vec<String> {
    let all = EVERYTHING.lock().unwrap();
    String::from_utf8_lossy(&all)
        .lines()
        .filter(|line| line.contains(needle))
        .map(str::to_string)
        .collect()
}

/// The flags every test server gets: ephemeral loopback port, a first boot, the shipped
/// two-host pool and an owner-only token file.
pub fn base_args(scratch: &Scratch) -> Vec<OsString> {
    let pool = scratch.write("pool.toml", EXAMPLE_CONFIG, 0o644);
    let token = scratch.write("token", TOKEN, 0o600);
    let mut args: Vec<OsString> = vec!["js-lease-server".into()];
    let flags = [
        ("--listen", "127.0.0.1:0".into()),
        ("--state-file", scratch.state().into_os_string()),
        ("--boot", "initialise".into()),
        ("--pool-config", pool.into_os_string()),
        ("--token-file", token.into_os_string()),
    ];
    for (flag, value) in flags {
        args.push(flag.into());
        args.push(value);
    }
    args
}

/// `base_args` with `extra` flags after them. A flag that appears twice takes the later
/// value only for options clap allows to repeat, so tests use `replace` for those.
pub fn args_with(scratch: &Scratch, extra: &[&str]) -> Vec<OsString> {
    let mut args = base_args(scratch);
    args.extend(extra.iter().map(OsString::from));
    args
}

/// `base_args` with the value of `flag` replaced.
pub fn replace(mut args: Vec<OsString>, flag: &str, value: impl Into<OsString>) -> Vec<OsString> {
    let at = args.iter().position(|arg| arg == flag).unwrap();
    args[at + 1] = value.into();
    args
}

/// Checks `args` as the program does.
pub fn config(args: Vec<OsString>) -> Result<Config, StartupError> {
    Config::from_cli(Cli::try_parse_from(args).unwrap())
}

/// A running test server with its clock and ticker.
pub struct Harness {
    pub running: Running,
    pub clock: ManualClock,
    pub ticks: TickHandle,
    /// Steps the pool checks of an elastic pool; a server without one ignores it.
    pub checks: TickHandle,
    pub scratch: Scratch,
}

impl Harness {
    /// A server on the base flags.
    pub async fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let args = base_args(&scratch);
        Self::with(scratch, args, None).await
    }

    /// A server on `args`, with its hosts from `source` when given.
    pub async fn with(
        scratch: Scratch,
        args: Vec<OsString>,
        source: Option<Box<dyn CapacitySource>>,
    ) -> Self {
        let mut config = config(args).unwrap();
        if let Some(source) = source {
            config = config.with_capacity_source(source);
        }
        Self::over(scratch, config).await
    }

    /// A server on the base flags with these connection limits.
    pub async fn limited(name: &str, limits: Limits) -> Self {
        let scratch = Scratch::new(name);
        let config = config(base_args(&scratch)).unwrap().with_limits(limits);
        Self::over(scratch, config).await
    }

    /// A server on `config`.
    pub async fn over(scratch: Scratch, config: Config) -> Self {
        quiet_logs();
        let clock = ManualClock::new(Timestamp::from_millis(T0));
        let (ticks, ticker) = channel_ticker();
        let (checks, check_ticker) = channel_ticker();
        let shared = SharedClock::new(clock.clone());
        let running = start_with_checks(config, shared, ticker, check_ticker)
            .await
            .unwrap();
        Self {
            running,
            clock,
            ticks,
            checks,
            scratch,
        }
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.running.addr()
    }

    /// An authenticated request.
    pub async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Reply {
        let body = body.map(|body| body.to_string().into_bytes());
        request(self.addr(), method, path, Some(TOKEN), body.as_deref()).await
    }
}

/// A capacity source that hands out one provider whose state outlives the servers built
/// from it, as a provider that is a service of its own would.
#[derive(Clone)]
pub struct SharedPool {
    pool: Arc<std::sync::Mutex<StaticPool>>,
}

impl SharedPool {
    pub fn new() -> Self {
        Self::of(EXAMPLE_CONFIG)
    }

    /// A source whose hosts are the ones `config`, the text of a pool file, lists.
    pub fn of(config: &str) -> Self {
        let id = ProviderId::new("shared").unwrap();
        let pool = StaticPool::from_toml(id, config).unwrap();
        Self {
            pool: Arc::new(std::sync::Mutex::new(pool)),
        }
    }
}

struct SharedProvider(Arc<std::sync::Mutex<StaticPool>>);

impl CapacitySource for SharedPool {
    fn provider(&self, _bookings: &[Booking]) -> Result<Seated, StartupError> {
        Ok(Seated::all(Box::new(SharedProvider(self.pool.clone()))))
    }
}

impl SharedProvider {
    fn pool(&self) -> std::sync::MutexGuard<'_, StaticPool> {
        self.0.lock().unwrap()
    }
}

impl Pool for SharedProvider {
    fn id(&self) -> ProviderId {
        self.pool().id()
    }

    fn classes(&self) -> Vec<js_types::ResourceClass> {
        self.pool().classes()
    }

    fn start(&mut self, spec: js_provider::StartSpec) -> js_provider::StartAck {
        self.pool().start(spec)
    }

    fn stop(&mut self, lease: &js_types::LeaseId) -> js_provider::StopAck {
        self.pool().stop(lease)
    }

    fn observe(&mut self, lease: &js_types::LeaseId) -> js_provider::Observed {
        self.pool().observe(lease)
    }

    fn capacity(&self) -> Vec<js_provider::HostCapacity> {
        self.pool().capacity()
    }
}

/// What a request got back.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    /// The status line and headers, in lower case.
    pub head: String,
    pub body: String,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

/// Sends one HTTP/1.1 request over a new plain connection and reads the reply to the
/// end of the connection.
pub async fn request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) -> Reply {
    let stream = TcpStream::connect(addr).await.unwrap();
    exchange(stream, method, path, token, body).await
}

/// Sends one request over `stream` (plain or TLS) and reads the reply until the
/// server closes. A failed write or read after part of the reply was read keeps what
/// was read (a server that refuses a body over its limit closes without reading the
/// rest); one that leaves no reply at all fails the test with the error, so a lost
/// reply is not taken for a status of 0.
pub async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) -> Reply {
    let authorization = token.map(|token| format!("Bearer {token}"));
    exchange_authorized(stream, method, path, authorization.as_deref(), body).await
}

/// Like [`exchange`], with the whole `Authorization` header value given.
pub async fn exchange_authorized<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    method: &str,
    path: &str,
    authorization: Option<&str>,
    body: Option<&[u8]>,
) -> Reply {
    let (raw, failed) = exchange_raw(stream, method, path, authorization, body).await;
    if let (true, Some(error)) = (raw.is_empty(), failed) {
        panic!("{method} {path}: no reply, the connection failed: {error}");
    }
    parse_reply(&raw)
}

/// Sends one HTTP/1.1 request over a new plain connection to a listener that is to refuse
/// it below HTTP (plain text sent to a TLS port), and reads what comes back to the end of
/// the connection, whatever error ends it: the reply, of status 0 when there was none.
pub async fn unanswered(addr: std::net::SocketAddr, method: &str, path: &str) -> Reply {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (raw, _) = exchange_raw(stream, method, path, None, None).await;
    parse_reply(&raw)
}

/// Sends one request over `stream` and reads until the connection ends: the bytes read,
/// and the first error of the write, the flush and the read.
async fn exchange_raw<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    method: &str,
    path: &str,
    authorization: Option<&str>,
    body: Option<&[u8]>,
) -> (Vec<u8>, Option<std::io::Error>) {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    if let Some(value) = authorization {
        head.push_str(&format!("Authorization: {value}\r\n"));
    }
    let body = body.unwrap_or_default();
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut message = head.into_bytes();
    message.extend_from_slice(body);
    let sent = stream.write_all(&message).await;
    let flushed = stream.flush().await;
    let mut raw = Vec::new();
    let read = stream.read_to_end(&mut raw).await.map(drop);
    (raw, sent.and(flushed).and(read).err())
}

/// Splits a raw reply into status and body.
pub fn parse_reply(raw: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .map_or(0, |code| code.parse().unwrap());
    Reply {
        status,
        head: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

/// Starts a server on `config` with a ticker for its table and a ticker for pool checks
/// that nothing steps. Every test starts its server through [`start_with_checks`], so that
/// one copy of the generic start-up code runs all of what the tests start.
pub async fn start<T: Ticker>(
    config: Config,
    clock: SharedClock,
    ticker: T,
) -> Result<Running, StartupError> {
    let (_checks, check_ticker) = channel_ticker();
    start_with_checks(config, clock, ticker, check_ticker).await
}

/// What the server logs on the calling thread, kept as text until the capture is
/// dropped. A server started inside the test's own task logs on that thread, so a test
/// on the default (current thread) runtime sees all of it.
pub struct LogCapture {
    text: Arc<std::sync::Mutex<Vec<u8>>>,
    _scope: tracing::subscriber::DefaultGuard,
}

/// A writer into the shared buffer of a [`LogCapture`].
struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogCapture {
    pub fn start() -> Self {
        let text = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = text.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || Lines(sink.clone()))
            .finish();
        Self {
            text,
            _scope: tracing::subscriber::set_default(subscriber),
        }
    }

    /// Everything logged so far.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.text.lock().unwrap()).into_owned()
    }
}

/// A lease file at term 1 holding these leases, each `(sequence, state, host)`, all
/// acquired at `T0` and ending 7200 seconds later.
pub fn lease_file(leases: &[(u64, &str, Option<&str>)]) -> String {
    let mut text = format!(
        "version = 1\nterm = 1\nnext_sequence = {}\nlease_count = {}\n",
        leases.len() + 1,
        leases.len()
    );
    for (sequence, state, host) in leases {
        text.push_str(&format!(
            "\n[[lease]]\nterm = 1\nsequence = {sequence}\nstate = \"{state}\"\n"
        ));
        if let Some(host) = host {
            text.push_str(&format!("host = \"{host}\"\n"));
        }
        text.push_str(&format!(
            "acquired_at_ms = {T0}\nexpires_at_ms = {end}\nends_at_ms = {end}\n",
            end = T0 + 7_200_000
        ));
    }
    text
}

/// Waits for `future`, failing the test instead of hanging it if the thing it waits
/// for never happens (a bound set high enough that no healthy machine reaches it; it is
/// not an assertion about how long anything takes).
pub async fn guard<F: std::future::Future>(future: F) -> F::Output {
    let bound = std::time::Duration::from_secs(30);
    match tokio::time::timeout(bound, future).await {
        Ok(output) => output,
        Err(_) => panic!("still waiting after {bound:?}: the awaited event never happened"),
    }
}

/// Waits for every labelled task under the same bound as [`guard`]. When the bound is
/// hit, the panic names the tasks that have not finished and says what `timings` saw.
pub async fn guard_tasks(
    mut tasks: Vec<(&'static str, tokio::task::JoinHandle<()>)>,
    timings: &js_lease::Timings,
) {
    let bound = std::time::Duration::from_secs(30);
    let waited = tokio::time::timeout(bound, async {
        for (_, task) in &mut tasks {
            task.await.unwrap();
        }
    })
    .await;
    if waited.is_err() {
        let pending: Vec<&str> = tasks
            .iter()
            .filter(|(_, task)| !task.is_finished())
            .map(|(name, _)| *name)
            .collect();
        let steps = timings.report();
        panic!(
            "still waiting after {bound:?}: unfinished tasks: {pending:?}; lease steps: {steps}"
        );
    }
}

/// A clock that moves by `step` on every reading, so a step timed with one reading
/// before and one after lasts exactly `step`.
#[derive(Debug, Clone)]
pub struct Stepping {
    micros: Arc<std::sync::atomic::AtomicU64>,
    step: std::time::Duration,
}

impl Stepping {
    pub fn of(step: std::time::Duration) -> Self {
        Self {
            micros: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            step,
        }
    }
}

impl js_types::Monotonic for Stepping {
    fn reading(&self) -> std::time::Duration {
        let step = u64::try_from(self.step.as_micros()).unwrap();
        let before = self
            .micros
            .fetch_add(step, std::sync::atomic::Ordering::SeqCst);
        std::time::Duration::from_micros(before)
    }
}

/// Yields to the runtime until `ready` holds, under the same hang guard as [`guard`].
pub async fn until(mut ready: impl FnMut() -> bool) {
    guard(async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// Asserts that two scratch directories made with one name are two directories, so tests
/// that run side by side and pick one name do not share files.
#[test]
fn scratch_directories_of_one_name_differ() {
    let (first, second) = (Scratch::new("same"), Scratch::new("same"));
    assert_ne!(first.dir, second.dir);
    first.write("mine", "x", 0o600);
    assert!(!second.path("mine").exists());
}

/// Where a server keeps its host bindings and join tokens: in the files beside the lease
/// file, or in a SQLite state store with the dev flag (`--store`, `--allow-sqlite-store`).
/// A restart test that touches them runs in both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    Files,
    Store,
}

impl Kept {
    /// Both, the files first.
    pub const BOTH: [Kept; 2] = [Kept::Files, Kept::Store];

    /// Where the server `h` keeps them.
    pub fn of(h: &Harness) -> Kept {
        match h.running.store() {
            Some(_) => Kept::Store,
            None => Kept::Files,
        }
    }

    /// `args` with, in the store, `--store` naming a SQLite store in `scratch` and the dev
    /// flag.
    pub fn args(self, scratch: &Scratch, mut args: Vec<OsString>) -> Vec<OsString> {
        if self == Kept::Store {
            let file = store_file(scratch, &sqlite_line(scratch));
            args.extend(["--store".into(), file.into_os_string()]);
            args.push("--allow-sqlite-store".into());
        }
        args
    }

    /// The `[server]` lines of a configuration file in this mode: none for the files, and for
    /// the store, `store` naming a SQLite store in `scratch` and `allow_sqlite_store`.
    pub fn keys(self, scratch: &Scratch) -> String {
        match self {
            Kept::Files => String::new(),
            Kept::Store => {
                let file = store_file(scratch, &sqlite_line(scratch));
                let file = crate::config_support::quoted(&file);
                format!("store = {file}\nallow_sqlite_store = true\n")
            }
        }
    }

    /// A scratch name of this mode: `name` for the files, with `_store` after it for the
    /// store, so the two runs of a test do not share a name.
    pub fn named(self, name: &str) -> String {
        match self {
            Kept::Files => name.to_owned(),
            Kept::Store => format!("{name}_store"),
        }
    }
}

/// What `h` keeps of its host bindings, as text: the file `<state file>.agents` (empty when
/// there is none), or, with a store, each key under `hosts/binding/` and its value, one per
/// line. A test reads it for a host id, a digest or a field.
pub fn kept_bindings(h: &Harness) -> String {
    kept(h, ".agents", "hosts/binding/")
}

/// What `h` keeps of its join tokens, as [`kept_bindings`] reads the bindings: the file
/// `<state file>.joins`, or the keys under `hosts/join/` and their values.
pub fn kept_joins(h: &Harness) -> String {
    kept(h, ".joins", "hosts/join/")
}

fn kept(h: &Harness, suffix: &str, prefix: &str) -> String {
    let Some(store) = h.running.store() else {
        let mut path = h.scratch.state().into_os_string();
        path.push(suffix);
        return fs::read_to_string(PathBuf::from(path)).unwrap_or_default();
    };
    let rows = store.scan_prefix(prefix).unwrap().into_iter();
    let line = |(key, row): (String, js_state::Versioned)| {
        format!("{key} {}\n", String::from_utf8_lossy(&row.value))
    };
    rows.map(line).collect()
}
