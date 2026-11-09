//! The server: the `js-lease-server` binary as a child process, configured by flags only, on
//! loopback ports the kernel picks. Its lease port is the one line it prints on standard
//! output (`listening on ADDR`); its job port is the `addr` of the log line `listening for
//! the job service` on standard error, which a thread reads for the life of the process.

use std::fs;
use std::io::{BufRead as _, BufReader};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

use js_state::postgres::{TestDatabase, test_database};
use js_testkit::dir::TestDir;
use rustix::process::{Pid, Signal, kill_process};

use crate::agent::HANG_GUARD;

/// The program under test.
const PROGRAM: &str = env!("CARGO_BIN_EXE_js-lease-server");

/// The bearer token of the lease port and of the job service's `default` client (an
/// operator).
pub const TOKEN: &str = "test-token-0123456789-abcdefghijkl";

/// The join token the agent presents.
const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

/// The pool the agent joins.
pub const POOL_NAME: &str = "burst";

/// The pool's file: one host's shape that the agent's offer covers, the architecture the
/// `js-sched` placement needs to know, and room for one host booked whole (a mixed pool:
/// jobs share its hosts, or take one whole).
fn pool() -> String {
    format!(
        r#"
name = "{POOL_NAME}"
class = "whole-node"
max_machines = 2

[shape]
cpu_millis = 8000
memory_bytes = 17179869184

[capabilities]
arch = "{}"
"#,
        std::env::consts::ARCH
    )
}

/// Where the server keeps what the setting `--store` holds.
pub enum Store {
    /// A SQLite file beside the state file, with `--allow-sqlite-store`.
    Sqlite,
    /// A Postgres database of the test's own, on the server `KBF_TEST_POSTGRES_URL` names.
    Postgres(TestDatabase),
}

impl Store {
    /// A new Postgres database of the test's own.
    pub fn postgres() -> Self {
        Self::Postgres(test_database())
    }
}

/// The files a server is started over. A restart starts another process over the same ones.
pub struct Files {
    dir: TestDir,
    store: Store,
}

/// Writes `text` to `path` with `mode`.
fn write(path: &Path, text: &str, mode: u32) {
    fs::write(path, text).expect("a file of the server");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

impl Files {
    /// The files of the case `name`, over `store`.
    pub fn new(name: &str, store: Store) -> Self {
        let dir = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("v1-acceptance-server"),
            name,
        );
        let files = Self { dir, store };
        write(&files.path("token"), TOKEN, 0o600);
        write(&files.join_file(), &format!("{JOIN}\n"), 0o600);
        write(&files.path("agent-pool.toml"), &pool(), 0o644);
        let line = match &files.store {
            Store::Sqlite => format!("sqlite:{}", files.path("store.db").display()),
            Store::Postgres(database) => database.url(),
        };
        write(&files.path("store"), &line, 0o600);
        files
    }

    /// The file `name` of the server.
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The bearer token's file.
    pub fn token_file(&self) -> PathBuf {
        self.path("token")
    }

    /// The join token's file, which the agent is given.
    pub fn join_file(&self) -> PathBuf {
        self.path("agent-tokens")
    }

    /// The flags of a start with `--boot boot`.
    fn args(&self, boot: &str) -> Vec<String> {
        let path = |name: &str| self.path(name).display().to_string();
        let mut args: Vec<String> = [
            "--listen",
            "127.0.0.1:0",
            "--boot",
            boot,
            "--job-listen",
            "127.0.0.1:0",
            // The tick never comes: every change a case looks for rides on a heartbeat or a
            // call.
            "--tick",
            "1h",
        ]
        .map(str::to_owned)
        .into();
        for (flag, name) in [
            ("--state-file", "leases.toml"),
            ("--token-file", "token"),
            ("--agent-token-file", "agent-tokens"),
            ("--agent-pool-config", "agent-pool.toml"),
            ("--store", "store"),
        ] {
            args.extend([flag.to_owned(), path(name)]);
        }
        if matches!(self.store, Store::Sqlite) {
            args.push("--allow-sqlite-store".to_owned());
        }
        args
    }
}

/// A running server.
pub struct Server {
    child: Child,
    /// The lease port, which the agent registers on.
    pub lease: SocketAddr,
    /// The job service's port: gRPC and the HTTP/JSON gateway.
    pub job: SocketAddr,
    /// What it printed on standard error so far.
    stderr: Arc<Mutex<String>>,
}

/// Takes the `addr=` field of a log line, with any colour codes left out.
fn addr_field(line: &str) -> Option<SocketAddr> {
    let mut plain = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // An escape sequence ends at its first letter.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    let at = plain.find("addr=")? + "addr=".len();
    plain[at..].split_whitespace().next()?.parse().ok()
}

impl Server {
    /// Starts the server over `files` with `--boot boot` and returns once both of its ports
    /// are known.
    pub fn start(files: &Files, boot: &str) -> Self {
        let mut child = Command::new(PROGRAM)
            .args(files.args(boot))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the server starts");
        let stdout = child.stdout.take().expect("its standard output");
        let stderr = child.stderr.take().expect("its standard error");
        let (first, line) = channel();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stdout).read_line(&mut text);
            let _ = first.send(text);
        });
        let kept = Arc::new(Mutex::new(String::new()));
        let (job_tx, job_rx) = channel();
        let into = kept.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.contains("listening for the job service") {
                    let _ = job_tx.send(addr_field(&line));
                }
                let mut kept = into.lock().unwrap_or_else(|e| e.into_inner());
                kept.push_str(&line);
                kept.push('\n');
            }
        });
        let mut server = Self {
            child,
            lease: "0.0.0.0:0".parse().expect("an address"),
            job: "0.0.0.0:0".parse().expect("an address"),
            stderr: kept,
        };
        let printed = server.wait_for(&line, "its listening line");
        server.lease = printed
            .trim()
            .strip_prefix("listening on ")
            .and_then(|addr| addr.parse().ok())
            .unwrap_or_else(|| panic!("not a listening line: {printed:?}\n{}", server.err()));
        server.job = server
            .wait_for(&job_rx, "the job service's address")
            .unwrap_or_else(|| panic!("no address in the job service's line\n{}", server.err()));
        server
    }

    /// What `from` sends first, or the test fails naming `what` and showing what the server
    /// printed.
    fn wait_for<T>(&self, from: &Receiver<T>, what: &str) -> T {
        match from.recv_timeout(HANG_GUARD) {
            Ok(value) => value,
            Err(_) => panic!("the server never printed {what}:\n{}", self.err()),
        }
    }

    /// The lease port as the agent's `--server`.
    pub fn url(&self) -> String {
        format!("http://{}", self.lease)
    }

    /// What the server printed on standard error so far.
    pub fn err(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Stops the server as its unit would, with SIGTERM, and returns how it ended.
    pub fn stop(mut self) -> ExitStatus {
        let pid = Pid::from_child(&self.child);
        kill_process(pid, Signal::TERM).expect("SIGTERM");
        self.child.wait().expect("the server ends")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
