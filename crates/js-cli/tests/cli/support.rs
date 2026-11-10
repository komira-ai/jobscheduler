//! What the tests share: a server in this process, the CLI entry with captured streams,
//! and servers that answer canned replies.

use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use js_cli::Services;
use js_lease_server::{
    Cli as ServerCli, Config, Running, SharedClock, TickHandle, channel_ticker, start,
};
use js_testkit::dir::TestDir;
use js_types::{ManualClock, Timestamp};
use serde_json::Value;
use tokio::runtime::Runtime;

/// The wall time every test clock starts at.
pub const T0: u64 = 1_000_000;

/// The bearer token of the test servers.
pub const TOKEN: &str = "cli-test-token-0123456789abcdefghijklmn";

/// A directory under the target directory that holds one test's files and is removed
/// when the test ends.
pub struct Scratch {
    dir: PathBuf,
    _owner: TestDir,
}

impl Scratch {
    pub fn new(name: &str) -> Self {
        let owner = TestDir::under(Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-cli"), name);
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
        run(args, services())
    }

    /// Runs `extra` with services that report no signals.
    pub fn go(&self, extra: &[&str]) -> Outcome {
        self.with(extra, services())
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

/// Services over a signal source that reports nothing.
pub fn services() -> Services {
    Services::new(Box::new(|_| Ok(())))
}

/// The words of a revoke for `lambdalabs:i-1`: the cheapest authenticated call, answered 204
/// whether or not the instance has a token.
pub const REVOKE: [&str; 6] = [
    "join-token",
    "revoke",
    "--provider",
    "lambdalabs",
    "--instance-id",
    "i-1",
];

/// A pool that asks nothing of a host but an `arch` (the scheduler describes a host by
/// it), for the server's `--agent-pool-config`. No host says hello here.
const POOL: &str = r#"
name = "macs"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 1
memory_bytes = 1

[capabilities]
arch = "x86_64"
"#;

/// A server in this process, on a runtime of its own so the CLI can block the test's
/// thread.
pub struct Harness {
    /// The runtime the server runs on.
    _rt: Runtime,
    /// The server, kept running until the harness is dropped.
    _running: Running,
    /// The server's ticker handle, kept so the ticker stays open.
    _ticks: TickHandle,
    pub scratch: Scratch,
    pub js: Js,
}

impl Harness {
    /// A plain-http server whose agents register hosts into [`POOL`], so the join-token
    /// route exists. No host has said hello.
    pub fn new(name: &str) -> Self {
        Self::build_in(Scratch::new(name), &[])
    }

    /// A server over TLS with a certificate made for the test, admitting agents; the
    /// certificate is in the file `cert.pem` of the scratch directory, for `--ca-cert`.
    pub fn tls(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let made = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let cert = scratch.write("cert.pem", &made.cert.pem(), 0o644);
        let key = scratch.write("key.pem", &made.signing_key.serialize_pem(), 0o600);
        let (cert, key) = (cert.to_str().unwrap(), key.to_str().unwrap());
        Self::build_in(scratch, &["--tls-cert", cert, "--tls-key", key])
    }

    /// A server in `scratch` over the agent pool [`POOL`], with these extra server flags.
    fn build_in(scratch: Scratch, extra: &[&str]) -> Self {
        let pool = scratch.write("agent-pool.toml", POOL, 0o644);
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
            "--agent-pool-config".into(),
            pool.into(),
            "--token-file".into(),
            token_file.clone().into(),
        ];
        args.extend(extra.iter().map(OsString::from));
        let config = Config::from_cli(ServerCli::try_parse_from(args).unwrap()).unwrap();
        let rt = Runtime::new().unwrap();
        let clock = ManualClock::new(Timestamp::from_millis(T0));
        let (ticks, ticker) = channel_ticker();
        let shared = SharedClock::new(clock);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = config.with_listener(listener);
        let running = rt.block_on(start(config, shared, ticker)).unwrap();
        let scheme = if extra.contains(&"--tls-cert") {
            "https"
        } else {
            "http"
        };
        let server = format!("{scheme}://{}", running.addr());
        Self {
            _rt: rt,
            _running: running,
            _ticks: ticks,
            scratch,
            js: Js { server, token_file },
        }
    }
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
            // End the reply with a FIN, then read what the client still sends (a request
            // body) until it hangs up: closing with that unread would reset the
            // connection, which macOS reports to the client as EINVAL on its next call.
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let mut rest = [0u8; 1024];
            while stream.read(&mut rest).unwrap_or(0) > 0 {}
        }
    });
    format!("http://{addr}")
}

/// A port that every loopback name the tests use answers on and then hangs up: the
/// request is read and the connection closed with no reply, so a client fails with a
/// transport error, never a refusal.
///
/// This stands in for "a port nothing listens on". Binding a port and dropping the
/// listener is not that: another process can take the freed port before the client
/// connects. Here the sockets stay bound for the life of the test process, so no other
/// process can hold the port on an address the clients dial. `127.0.0.1` picks the port;
/// `127.9.9.9` and `::1` are bound on the same port where the host has them (an address
/// the host lacks cannot be dialled either) and a port taken on either is replaced.
pub fn hangup_port() -> u16 {
    for _ in 0..100 {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = first.local_addr().unwrap().port();
        let mut bound = vec![first];
        let mut taken = false;
        for host in ["127.9.9.9", "::1"] {
            match TcpListener::bind((host, port)) {
                Ok(listener) => bound.push(listener),
                Err(err) if err.kind() == ErrorKind::AddrNotAvailable => {}
                Err(_) => taken = true,
            }
        }
        if taken {
            continue;
        }
        for listener in bound {
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    read_request(&mut stream.unwrap());
                }
            });
        }
        return port;
    }
    panic!("no port free on every loopback address after 100 tries");
}

/// A reply with this status line and body, ended by closing the connection.
pub fn http(status: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nConnection: close\r\n\r\n{body}")
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

/// The CLI against `server` with the token of `scratch`'s file.
pub fn against(server: &str, scratch: &Scratch) -> Js {
    Js {
        server: server.to_owned(),
        token_file: scratch.write("token", TOKEN, 0o600),
    }
}

/// What the recording server read: the request line and the parsed body of each request.
pub type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// A server that answers every request with `reply` and records what it read.
pub fn recording(reply: String) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let (line, body) = read(&mut stream);
            log.lock().unwrap().push((line, body));
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (format!("http://{addr}"), seen)
}

/// Reads one request: its first line and its body as JSON.
fn read(stream: &mut impl Read) -> (String, Value) {
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
    let line = head.lines().next().unwrap().to_owned();
    (line, serde_json::from_slice(&body).unwrap_or(Value::Null))
}
