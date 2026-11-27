//! The lease server: its released binary as a child process, configured by flags only, over TLS
//! on loopback ports the kernel picks, trusting the test's CA for agents, with a SQLite store.
//! Its lease port is the one line it prints on standard output (`listening on ADDR`); its job
//! port is the `addr` of the log line `listening for the job service` on standard error.

use std::io::{BufRead as _, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_client::Client;
use serde_json::Value;

use crate::host::put;
use crate::identity::server_cert;

/// How long the server may take to print its ports.
const START_LIMIT: Duration = Duration::from_secs(60);

/// The bearer token of the job service and of the lease port's reads.
const TOKEN: &str = "client-token-0123456789-abcdefghijkl";

/// The pool's join token, which an agent with a host certificate neither reads nor sends.
const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

/// The pool the farm's Linux nodes join.
pub const POOL: &str = "farm";

/// The pool: one host's shape the agent's offer covers on a hosted runner, and the
/// architecture placement needs.
const POOL_TOML: &str = r#"
name = "farm"
class = "whole-node"
max_machines = 1

[shape]
cpu_millis = 2000
memory_bytes = 4294967296

[capabilities]
arch = "x86_64"
"#;

/// A running server.
pub struct Server {
    child: Child,
    /// The lease port, which the agent says hello on.
    pub lease: SocketAddr,
    /// The job service's port.
    pub job: SocketAddr,
    /// The server's certificate, which the agent and the test trust.
    pub cert: PathBuf,
    token: PathBuf,
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
    /// Starts `program` on a first boot over the files of `dir`, trusting `agent_ca` for
    /// agents, and returns once both of its ports are known.
    pub fn start(program: &Path, dir: &Path, agent_ca: &str) -> Server {
        let file = |name: &str, contents: &str, mode: u32| {
            let path = dir.join(name);
            put(&path, contents, mode);
            path
        };
        let (cert, key) = server_cert();
        let cert = file("server-cert.pem", &cert, 0o644);
        let key = file("server-key.pem", &key, 0o600);
        let ca = file("agent-ca.pem", agent_ca, 0o644);
        let token = file("client-token", TOKEN, 0o600);
        let join = file("join-token", &format!("{JOIN}\n"), 0o600);
        let pool = file("agent-pool.toml", POOL_TOML, 0o644);
        let store = file(
            "store",
            &format!("sqlite:{}", dir.join("store.db").display()),
            0o600,
        );
        let mut child = Command::new(program)
            .arg("--listen")
            .arg("127.0.0.1:0")
            .arg("--job-listen")
            .arg("127.0.0.1:0")
            .arg("--boot")
            .arg("initialise")
            .arg("--state-file")
            .arg(dir.join("leases.toml"))
            .arg("--token-file")
            .arg(&token)
            .arg("--agent-token-file")
            .arg(&join)
            .arg("--agent-pool-config")
            .arg(&pool)
            .arg("--store")
            .arg(&store)
            .arg("--allow-sqlite-store")
            .arg("--tls-cert")
            .arg(&cert)
            .arg("--tls-key")
            .arg(&key)
            .arg("--agent-client-ca")
            .arg(&ca)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("start {}: {e}", program.display()));
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
        let mut server = Server {
            child,
            lease: "0.0.0.0:0".parse().expect("an address"),
            job: "0.0.0.0:0".parse().expect("an address"),
            cert,
            token,
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
        match from.recv_timeout(START_LIMIT) {
            Ok(value) => value,
            Err(_) => panic!("the server never printed {what}:\n{}", self.err()),
        }
    }

    /// The lease port as the agent's `--server`: by name, as the farm's flags name the server.
    pub fn url(&self) -> String {
        format!("https://localhost:{}", self.lease.port())
    }

    /// What the server printed on standard error so far.
    pub fn err(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// A client of the port `addr` with the client token.
    fn client(&self, addr: SocketAddr) -> Client {
        Client::connect(
            &format!("https://localhost:{}", addr.port()),
            &self.token,
            Some(&self.cert),
            Duration::from_secs(30),
        )
        .expect("a client of the server")
    }

    /// The entry of `host` in `GET /v1/hosts`, if it is listed.
    pub fn host(&self, host: &str) -> Option<Value> {
        let reply = self
            .client(self.lease)
            .get("/v1/hosts")
            .expect("the server answers");
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply.body["hosts"]
            .as_array()
            .and_then(|hosts| hosts.iter().find(|entry| entry["id"] == host))
            .cloned()
    }

    /// Submits `spec` (`POST /v1/jobs`), which must be accepted.
    pub fn submit(&self, spec: &Value) {
        let reply = self
            .client(self.job)
            .post("/v1/jobs", &serde_json::json!({ "spec": spec }))
            .expect("the job service answers");
        assert_eq!(reply.status, 201, "submit {spec}: {}", reply.body);
    }

    /// The job `id` (`GET /v1/jobs/<id>`).
    pub fn job(&self, id: &str) -> Value {
        let reply = self
            .client(self.job)
            .get(&format!("/v1/jobs/{id}"))
            .expect("the job service answers");
        assert_eq!(reply.status, 200, "job {id}: {}", reply.body);
        reply.body["job"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
