//! What the agent tests share: flags that turn registration on, the messages an agent
//! sends, and a client that shows the headers of an answer.

use std::ffi::OsString;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::support::{Harness, Reply, Scratch, base_args, lease_body, replace, request};

/// The join token in the token file at the start.
pub const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

/// A second join token, added to the file for a rotation or a second tenant of the pool.
pub const JOIN_B: &str = "join-token-B-0123456789-abcdefghijklmnop";

/// The pool registering hosts join: room for two machines, and a floor that the shipped
/// two-host static pool's requests (`lease_body`) fit, so one request can be served by
/// either source.
pub const POOL: &str = r#"
name = "burst"
class = "gpu-host"
max_machines = 2

[shape]
cpu_millis = 8000
memory_bytes = 17179869184
gpus = 1
gpu_class = "nvidia-a100"

[capabilities]
isa = "x86-64-v3"
"#;

/// The base flags plus the agent flags: the join token file holds [`JOIN`] and
/// [`JOIN_B`]; the static pool is dropped when `keep_static` is false.
pub fn agent_args(scratch: &Scratch, keep_static: bool) -> Vec<OsString> {
    let mut args = base_args(scratch);
    if !keep_static {
        let at = args.iter().position(|arg| arg == "--pool-config").unwrap();
        args.drain(at..at + 2);
    }
    let tokens = scratch.write("agent-tokens", &format!("{JOIN}\n{JOIN_B}\n"), 0o600);
    let pool = scratch.write("agent-pool.toml", POOL, 0o644);
    let flags = [
        ("--agent-token-file", tokens.into_os_string()),
        ("--agent-pool-config", pool.into_os_string()),
    ];
    for (flag, value) in flags {
        args.push(flag.into());
        args.push(value);
    }
    args
}

/// A server with agent registration on, over a fresh scratch directory.
pub async fn start_agents(name: &str, keep_static: bool) -> Harness {
    let scratch = Scratch::new(name);
    let args = agent_args(&scratch, keep_static);
    Harness::with(scratch, args, None).await
}

/// Stops `first` and starts another server over its scratch directory with
/// `--boot resume`, as the program does after a restart: a new registry, nothing carried
/// over but the files.
pub async fn restarted(first: Harness, keep_static: bool) -> Harness {
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let args = replace(agent_args(&scratch, keep_static), "--boot", "resume");
    Harness::with(scratch, args, None).await
}

/// A hello a host of the pool sends: the floor shape exactly, in the pool `burst`.
pub fn hello_body(host: &str) -> Value {
    json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": {
            "cpu_millis": 8000,
            "memory_bytes": 17179869184u64,
            "gpus": 1,
            "gpu_class": "nvidia-a100",
        },
        "capabilities": { "isa": "x86-64-v3" },
        "labels": { "rack": "r1" },
        "phase": "serving",
    })
}

/// The session an agent holds after its hello.
#[derive(Debug, Clone)]
pub struct Session {
    pub host: String,
    pub epoch: u64,
    pub secret: String,
}

impl Session {
    /// The heartbeat of this session.
    pub fn beat(&self) -> Value {
        json!({
            "host_id": self.host,
            "epoch": self.epoch,
            "session": self.secret,
            "phase": "serving",
        })
    }
}

/// POSTs `body` to `path` with `token` as the bearer.
pub async fn post(h: &Harness, path: &str, token: Option<&str>, body: &Value) -> Reply {
    let bytes = body.to_string().into_bytes();
    request(h.addr(), "POST", path, token, Some(&bytes)).await
}

/// A hello with `token`.
pub async fn hello_with(h: &Harness, token: &str, host: &str) -> Reply {
    post(h, "/v1/agent/hello", Some(token), &hello_body(host)).await
}

/// A hello with [`JOIN`] that must be admitted.
pub async fn join(h: &Harness, host: &str) -> Session {
    session(&hello_with(h, JOIN, host).await, host)
}

/// The session a welcome (a 200 answer) opened.
pub fn session(reply: &Reply, host: &str) -> Session {
    assert_eq!(reply.status, 200, "{}", reply.body);
    let welcome = reply.json();
    Session {
        host: host.to_string(),
        epoch: welcome["epoch"].as_u64().unwrap(),
        secret: welcome["session"].as_str().unwrap().to_string(),
    }
}

/// The heartbeat of `session` with [`JOIN`].
pub async fn beat(h: &Harness, session: &Session) -> Reply {
    post(h, "/v1/agent/heartbeat", Some(JOIN), &session.beat()).await
}

/// `GET /v1/hosts` with the client token.
pub async fn hosts(h: &Harness) -> Value {
    let reply = h.call("GET", "/v1/hosts", None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()
}

/// The entry of `host` in the listing.
pub fn entry(listing: &Value, host: &str) -> Value {
    let hosts = listing["hosts"].as_array().unwrap();
    hosts
        .iter()
        .find(|entry| entry["id"] == host)
        .unwrap_or_else(|| panic!("{host} is not listed: {listing}"))
        .clone()
}

/// Every id the listing holds.
pub fn listed(listing: &Value) -> Vec<String> {
    let hosts = listing["hosts"].as_array().unwrap();
    let ids = hosts.iter().map(|entry| entry["id"].as_str().unwrap());
    ids.map(str::to_string).collect()
}

/// An answer with its head, for the headers.
pub struct Raw {
    pub status: u16,
    pub head: String,
    pub body: String,
}

/// POSTs `body` and returns the whole answer. `declared` is the Content-Length to claim
/// (the body's own length when `None`). The sending side is shut after the body, so a
/// declared length longer than the body ends the request mid-body.
pub async fn post_raw(
    h: &Harness,
    path: &str,
    token: &str,
    body: &[u8],
    declared: Option<usize>,
) -> Raw {
    let mut stream = TcpStream::connect(h.addr()).await.unwrap();
    let length = declared.unwrap_or(body.len());
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Length: {length}\r\n\r\n"
    );
    let mut message = head.into_bytes();
    message.extend_from_slice(body);
    let _ = stream.write_all(&message).await;
    let _ = stream.shutdown().await;
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .map_or(0, |c| c.parse().unwrap());
    Raw {
        status,
        head: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

/// The heartbeat of `session` that reports its host ready for the whole-node lease `id`.
pub async fn report_ready(h: &Harness, session: &Session, id: &str) {
    let mut message = session.beat();
    message["exclusive_ready"] = json!(id);
    let ready = post(h, "/v1/agent/heartbeat", Some(JOIN), &message).await;
    assert_eq!(ready.status, 200, "{}", ready.body);
}

/// A request for a lease, answered by a registered host as the commit gate has it: 202
/// pending until the agent of its host (one of `sessions`) has reported the host ready for
/// exactly this lease, then the lease is granted at the next tick. A static host answers
/// 201 at once. Returns the entry of the lease in the list, which has the host it holds.
pub async fn grant(h: &Harness, sessions: &[&Session]) -> Value {
    grant_with(h, sessions, &lease_body()).await
}

/// As [`grant`], for the request `body`.
pub async fn grant_with(h: &Harness, sessions: &[&Session], body: &Value) -> Value {
    let asked = h.call("POST", "/v1/leases", Some(body)).await;
    assert!(matches!(asked.status, 201 | 202), "{}", asked.body);
    let id = asked.json()["id"].as_str().unwrap().to_string();
    if asked.status == 202 {
        for session in sessions {
            report_ready(h, session, &id).await;
        }
        h.ticks.step().await;
    }
    let listed = h.call("GET", "/v1/leases", None).await.json();
    let leases = listed["leases"].as_array().unwrap();
    let held = leases.iter().find(|lease| lease["id"] == id);
    held.unwrap_or_else(|| panic!("{id} is not listed: {listed}"))
        .clone()
}
