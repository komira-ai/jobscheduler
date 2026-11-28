//! What the agent tests share: flags that turn registration on, the messages an agent
//! sends, and a client that shows the headers of an answer.

use std::ffi::OsString;
use std::path::Path;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::host_certs_support::shared_ca;
use crate::support::{Harness, Kept, Reply, Scratch, base_args, connect_for, replace, request};
use crate::tls::test_identity;

/// The pool registering hosts join: room for two machines, with one GPU each.
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
arch = "x86_64"
isa = "x86-64-v3"
"#;

/// A pool of up to two machines of 8 cores and 16 GiB with no GPU, whose capabilities name an
/// `arch`, as `js-sched` needs to place a job.
pub const JOB_POOL: &str = r#"
name = "burst"
class = "burst-host"
max_machines = 2

[shape]
cpu_millis = 8000
memory_bytes = 17179869184

[capabilities]
arch = "x86_64"
"#;

/// The flags that put a test server on TLS and make it admit hosts by the certificates of the
/// CA of the tests ([`shared_ca`]): its own certificate and key, and the CA file, written under
/// `scratch`. [`request`] and the helpers here reach a server so started over TLS, presenting
/// the certificate of the host a call is for.
pub fn agent_tls_flags(scratch: &Scratch) -> Vec<OsString> {
    let (cert, key) = test_identity();
    let tls_cert = scratch.write("test-only-cert.pem", &cert, 0o644);
    let tls_key = scratch.write("test-only-key.pem", &key, 0o600);
    let ca = scratch.write("hostkeeper-ca.pem", &shared_ca().pem(), 0o644);
    let mut flags = Vec::new();
    for (flag, value) in [
        ("--tls-cert", tls_cert),
        ("--tls-key", tls_key),
        ("--agent-client-ca", ca),
    ] {
        flags.push(flag.into());
        flags.push(value.into_os_string());
    }
    flags
}

/// The `[server]` lines of a configuration file that do what [`agent_tls_flags`] do.
pub fn agent_tls_keys(scratch: &Scratch) -> String {
    let flags = agent_tls_flags(scratch);
    let value = |at: usize| crate::config_support::quoted(Path::new(&flags[at]));
    format!(
        "tls_cert = {}\ntls_key = {}\nagent_client_ca = {}\n",
        value(1),
        value(3),
        value(5)
    )
}

/// The base flags (whose pool is [`POOL`]) plus [`agent_tls_flags`]: a server that admits a
/// host by its certificate.
pub fn agent_args(scratch: &Scratch) -> Vec<OsString> {
    let mut args = base_args(scratch);
    args.extend(agent_tls_flags(scratch));
    args
}

/// The agent flags over [`JOB_POOL`], the job service on a loopback
/// port, so that a host which says hello with [`job_hello`] runs jobs.
pub fn job_host_args(scratch: &Scratch) -> Vec<OsString> {
    let mut args = agent_args(scratch);
    scratch.write("agent-pool.toml", JOB_POOL, 0o644);
    args.extend(["--job-listen", "127.0.0.1:0"].map(Into::into));
    args
}

/// A server on [`job_host_args`], keeping the bindings as `kept` says.
pub async fn start_job_hosts_in(name: &str, kept: Kept) -> Harness {
    let scratch = Scratch::new(&kept.named(name));
    let args = kept.args(&scratch, job_host_args(&scratch));
    Harness::with(scratch, args).await
}

/// [`restarted`] for a server started with [`start_job_hosts_in`].
pub async fn restarted_job_hosts(first: Harness) -> Harness {
    let kept = Kept::of(&first);
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let args = kept.args(&scratch, job_host_args(&scratch));
    let args = replace(args, "--boot", "resume");
    Harness::with(scratch, args).await
}

/// A server with agent registration on, over a fresh scratch directory.
pub async fn start_agents(name: &str) -> Harness {
    start_agents_in(name, Kept::Files).await
}

/// [`start_agents`], keeping the bindings as `kept` says.
pub async fn start_agents_in(name: &str, kept: Kept) -> Harness {
    let scratch = Scratch::new(&kept.named(name));
    let args = kept.args(&scratch, agent_args(&scratch));
    Harness::with(scratch, args).await
}

/// Stops `first` and starts another server over its scratch directory with
/// `--boot resume`, as the program does after a restart: a new registry, nothing carried
/// over but the files, and the store when `first` kept its hosts in one.
pub async fn restarted(first: Harness) -> Harness {
    let kept = Kept::of(&first);
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let args = kept.args(&scratch, agent_args(&scratch));
    let args = replace(args, "--boot", "resume");
    Harness::with(scratch, args).await
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
        "capabilities": { "arch": "x86_64", "isa": "x86-64-v3" },
        "labels": { "rack": "r1" },
        "phase": "serving",
    })
}

/// The hello of a host of [`JOB_POOL`] that speaks the per-job messages: the shape of the
/// pool, no GPU, and the arch `x86_64`.
pub fn job_hello(host: &str) -> Value {
    let mut hello = hello_body(host);
    hello["shape"] = json!({ "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 });
    hello["capabilities"] = json!({ "per-job": "true", "arch": "x86_64" });
    // The process the agent runs as, which a restarted server over the same store (whose row
    // holds the live session) is told by the hello that replaces it, instead of refusing it
    // `host-live`.
    hello["agent_process"] = json!(format!("process-of-{host}"));
    hello
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

/// POSTs `body` to `path` with `bearer` as the bearer, when given. An agent route is called
/// presenting the certificate of the host the body names; a bearer is not read there.
pub async fn post(h: &Harness, path: &str, bearer: Option<&str>, body: &Value) -> Reply {
    let bytes = body.to_string().into_bytes();
    request(h.addr(), "POST", path, bearer, Some(&bytes)).await
}

/// The call `method path` with `body` and `bearer` made over TLS presenting no certificate.
pub async fn without_certificate(
    h: &Harness,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&Value>,
) -> Reply {
    let reply = h.client(None).send(method, path, bearer, body).await;
    reply.unwrap_or_else(|error| panic!("{method} {path} without a certificate: {error}"))
}

/// A hello of `host`, presenting its certificate.
pub async fn hello_as(h: &Harness, host: &str) -> Reply {
    post(h, "/v1/agent/hello", None, &hello_body(host)).await
}

/// A hello of `host` that must be admitted.
pub async fn join(h: &Harness, host: &str) -> Session {
    session(&hello_as(h, host).await, host)
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

/// The heartbeat of `session`, presenting the certificate of its host.
pub async fn beat(h: &Harness, session: &Session) -> Reply {
    post(h, "/v1/agent/heartbeat", None, &session.beat()).await
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
/// declared length longer than the body ends the request mid-body. A failed write, shut or
/// read, and a connection that ends with no answer, fail the test with what happened, so a
/// lost answer is not taken for a status of 0.
pub async fn post_raw(h: &Harness, path: &str, body: &[u8], declared: Option<usize>) -> Raw {
    let mut stream = connect_for(h.addr(), path, Some(body)).await;
    let length = declared.unwrap_or(body.len());
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nContent-Length: {length}\r\n\r\n"
    );
    let mut message = head.into_bytes();
    message.extend_from_slice(body);
    let sent = stream.write_all(&message).await;
    sent.unwrap_or_else(|error| panic!("POST {path}: the request was not sent: {error}"));
    let shut = stream.shutdown().await;
    shut.unwrap_or_else(|error| panic!("POST {path}: the sending side did not shut: {error}"));
    let mut raw = Vec::new();
    let read = stream.read_to_end(&mut raw).await;
    read.unwrap_or_else(|error| {
        panic!(
            "POST {path}: the answer was lost after {} bytes: {error}",
            raw.len()
        )
    });
    assert!(
        !raw.is_empty(),
        "POST {path}: the connection ended with no answer"
    );
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

/// The code of a refusal.
pub fn code(reply: &Reply) -> String {
    reply.json()["error"].as_str().unwrap().to_string()
}
