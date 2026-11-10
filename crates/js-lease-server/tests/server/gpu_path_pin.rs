//! Pins what the server does today with no scheduler flag: the flags and their defaults,
//! the status of every documented route for a request with no token, a wrong token and
//! the right one, and the files the server leaves in its state directory. The lease and run
//! routes of earlier releases stay among the probes: the golden pins them as paths the
//! router does not have.
//!
//! The expected text is `golden/gpu_path.txt`, in sections. A change that alters any of
//! it edits the golden in its own diff, where a reviewer sees it. Regenerate on purpose
//! with
//!
//! ```text
//! JS_REGENERATE_GOLDEN=1 cargo test -p js-lease-server --test server -- --ignored regenerate_gpu_path_golden
//! ```
//!
//! The regenerating test is ignored and also refuses to run unless `JS_REGENERATE_GOLDEN`
//! is set, so running every ignored test (`--include-ignored`) cannot rewrite the golden.
//!
//! The router cannot be asked for its route list, so the routes sections probe the
//! documented routes and a list of paths a later change might add; any of those that
//! starts answering shows up as a changed line.
//!
//! Nothing here measures time.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::CommandFactory;
use js_lease_server::Cli;

use crate::agents_support::{agent_args, join};
use crate::support::{Harness, Scratch, TOKEN, base_args, replace};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/server/golden/gpu_path.txt"
);

const REGENERATE: &str = "JS_REGENERATE_GOLDEN=1 cargo test -p js-lease-server --test server -- --ignored regenerate_gpu_path_golden";

const WRONG_TOKEN: &str = "wrong-token-0123456789-abcdefghijklmnop";

/// The sections of the golden file, in file order.
const SECTIONS: [&str; 5] = [
    "flags",
    "routes",
    "routes-with-agents",
    "keep-alive",
    "state-files",
];

/// Every flag by name with its default and whether it is required, sorted. A hidden flag
/// (one that only refuses) is not a flag the server has, so it is not listed.
fn flags() -> String {
    let command = Cli::command();
    let mut rows: Vec<String> = command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .filter_map(|arg| {
            let long = arg.get_long()?;
            let defaults: Vec<String> = arg
                .get_default_values()
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect();
            let choices: Vec<String> = arg
                .get_possible_values()
                .iter()
                .map(|value| value.get_name().to_owned())
                .collect();
            let default = match defaults.is_empty() {
                true => "none".to_owned(),
                false => defaults.join(","),
            };
            let mut row = format!(
                "--{long} default={default} required={}",
                arg.is_required_set()
            );
            if !choices.is_empty() {
                let _ = write!(row, " values={}", choices.join("|"));
            }
            Some(row)
        })
        .collect();
    rows.sort();
    rows.join("\n")
}

/// One request of the route table: method, path, and a body for the right token.
type Probe = (&'static str, &'static str, Option<serde_json::Value>);

/// The client routes of the documentation table, the lease and run routes of earlier
/// releases (which are not routes now), the agent routes (which do not exist without the
/// agent flags), a path under `/v1/sched/` and two wrong-method requests.
fn probes() -> Vec<Probe> {
    let mut probes = vec![
        ("GET", "/healthz", None),
        ("POST", "/v1/leases", Some(serde_json::json!({}))),
        ("GET", "/v1/leases", None),
        ("GET", "/v1/hosts", None),
        ("POST", "/v1/leases/1-1/renew", Some(serde_json::json!({}))),
        ("POST", "/v1/leases/1-1/runs", Some(serde_json::json!({}))),
        ("GET", "/v1/leases/1-1/runs", None),
        ("GET", "/v1/leases/1-1/runs/r1", None),
        ("DELETE", "/v1/leases/1-1/runs/r1", None),
        ("POST", "/v1/agent/hello", Some(serde_json::json!({}))),
        ("POST", "/v1/agent/heartbeat", Some(serde_json::json!({}))),
        ("GET", "/v1/sched/jobs", None),
        ("POST", "/v1/sched/jobs", Some(serde_json::json!({}))),
        ("GET", "/v1/sched/jobs/1", None),
        ("DELETE", "/v1/sched/jobs/1", None),
        ("GET", "/v1/nothing", None),
        ("DELETE", "/v1/leases/1-1", None),
        ("PUT", "/v1/hosts", None),
    ];
    for path in UNKNOWN_PATHS {
        for method in ["GET", "POST"] {
            probes.push((method, path, None));
        }
    }
    probes
}

/// Paths a later change might add. None is a route today, so each is a 404 under the
/// right token and a 401 under any other.
const UNKNOWN_PATHS: [&str; 14] = [
    "/",
    "/metrics",
    "/v1",
    "/v1/jobs",
    "/v1/operations",
    "/v1/placements",
    "/v1/sched",
    "/v1/sched/hosts",
    "/v1/workers",
    "/v1/hosts/gpu-01",
    "/v1/agent",
    "/v1/agent/session",
    "/v1/state",
    "/v1/leases/1-1/hold",
];

/// A reply with its header names and values, as the raw bytes gave them.
struct Seen {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        let found = self.headers.iter().find(|(key, _)| key == name);
        found.map(|(_, value)| value.as_str())
    }

    /// The status, the body if any, the content type, and the sorted header names.
    fn show(&self) -> String {
        let mut names: Vec<&str> = self.headers.iter().map(|(key, _)| key.as_str()).collect();
        names.sort_unstable();
        let mut text = self.status.to_string();
        if !self.body.is_empty() {
            let _ = write!(text, " {}", self.body.trim_end());
        }
        let kind = self.header("content-type").unwrap_or("-");
        let _ = write!(text, " [type {kind}; headers {}]", names.join(","));
        text
    }
}

/// Writes one request on `stream`.
async fn send(
    stream: &mut BufReader<TcpStream>,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
    close: bool,
) {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: test\r\n");
    if close {
        head.push_str("Connection: close\r\n");
    }
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    let body = body.unwrap_or_default();
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut message = head.into_bytes();
    message.extend_from_slice(body);
    stream.write_all(&message).await.unwrap();
    stream.flush().await.unwrap();
}

/// Reads one reply: the head, then the body by its length.
async fn read_reply(stream: &mut BufReader<TcpStream>) -> Seen {
    let mut line = String::new();
    stream.read_line(&mut line).await.unwrap();
    let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut headers = Vec::new();
    loop {
        line.clear();
        stream.read_line(&mut line).await.unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map_or(0, |(_, value)| value.parse::<usize>().unwrap());
    assert!(
        !headers.iter().any(|(name, _)| name == "transfer-encoding"),
        "a chunked reply: read it by its chunks"
    );
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    Seen {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// One request on a connection of its own, which the request asks to close.
async fn once(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) -> Seen {
    let mut stream = BufReader::new(TcpStream::connect(addr).await.unwrap());
    send(&mut stream, method, path, token, body, true).await;
    read_reply(&mut stream).await
}

/// For each probe, the reply with no token, with a wrong token and with `token`; a GET
/// is asked a second time with `token`, since a reply may change on the second call.
async fn matrix(harness: &Harness, token: &str) -> String {
    let mut out = String::new();
    let addr = harness.addr();
    for (method, path, body) in probes() {
        let body = body.map(|body| body.to_string().into_bytes());
        let body = body.as_deref();
        let none = once(addr, method, path, None, body).await;
        let wrong = once(addr, method, path, Some(WRONG_TOKEN), body).await;
        let valid = once(addr, method, path, Some(token), body).await;
        let _ = writeln!(
            out,
            "{method} {path}\n  no token: {}\n  wrong token: {}\n  valid token: {}",
            none.show(),
            wrong.show(),
            valid.show()
        );
        if method == "GET" {
            let again = once(addr, method, path, Some(token), body).await;
            let _ = writeln!(out, "  valid token, again: {}", again.show());
        }
    }
    out
}

/// Several requests on one kept-alive connection, then one that asks to close, after
/// which the server closes the connection.
async fn keep_alive(tag: &str) -> String {
    let harness = Harness::new(&format!("gpu_path_{tag}_keep_alive")).await;
    let mut stream = BufReader::new(TcpStream::connect(harness.addr()).await.unwrap());
    let steps = [
        ("GET", "/healthz", None, false),
        ("GET", "/v1/hosts", Some(TOKEN), false),
        ("GET", "/v1/hosts", Some(WRONG_TOKEN), false),
        ("GET", "/v1/hosts", Some(TOKEN), false),
        ("GET", "/v1/hosts", Some(TOKEN), true),
    ];
    let mut out = String::new();
    for (method, path, token, close) in steps {
        send(&mut stream, method, path, token, None, close).await;
        let seen = read_reply(&mut stream).await;
        let connection = seen.header("connection").unwrap_or("-").to_owned();
        let _ = writeln!(
            out,
            "{method} {path} (token {}, close asked {close}): {} [connection {connection}]",
            token.map_or("none", |t| if t == TOKEN { "valid" } else { "wrong" }),
            seen.show()
        );
    }
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).await.unwrap();
    let _ = writeln!(out, "bytes after the last reply: {}", rest.len());
    out
}

/// The routes of a server on the base flags: agent registration with no shared join token
/// file.
async fn routes(tag: &str) -> String {
    let harness = Harness::new(&format!("gpu_path_{tag}_routes")).await;
    matrix(&harness, TOKEN).await
}

/// The same requests on a server with agent registration on; the valid token is the
/// client token, which the agent routes refuse.
async fn routes_with_agents(tag: &str) -> String {
    let scratch = Scratch::new(&format!("gpu_path_{tag}_routes_agents"));
    let args = agent_args(&scratch);
    let harness = Harness::with(scratch, args).await;
    matrix(&harness, TOKEN).await
}

/// The base flags with the state file in a directory of its own, which holds nothing else.
fn args_with_state_dir(scratch: &Scratch, agents: bool) -> (Vec<OsString>, PathBuf) {
    let dir = scratch.path("state");
    fs::create_dir_all(&dir).unwrap();
    let args = match agents {
        true => agent_args(scratch),
        false => base_args(scratch),
    };
    let args = replace(args, "--state-file", dir.join("leases.toml"));
    (args, dir)
}

/// Every path under `root`, relative to it and sorted, a directory with a trailing `/`.
fn tree(root: &Path) -> String {
    fn walk(root: &Path, dir: &Path, found: &mut BTreeSet<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            match path.is_dir() {
                true => {
                    found.insert(format!("{relative}/"));
                    walk(root, &path, found);
                }
                false => {
                    found.insert(relative);
                }
            }
        }
    }
    let mut found = BTreeSet::new();
    walk(root, root, &mut found);
    found.into_iter().collect::<Vec<_>>().join(" ")
}

/// Every file under the scratch root (flag files, state directory) after a start and after a
/// shutdown, with and without the agent flags (where an agent also joins).
async fn state_files(tag: &str) -> String {
    let mut out = String::new();
    for (label, agents) in [
        ("without a join token file", false),
        ("with a join token file", true),
    ] {
        let scratch = Scratch::new(&format!("gpu_path_{tag}_files_{agents}"));
        let (args, _) = args_with_state_dir(&scratch, agents);
        let dir = scratch.dir.clone();
        let _ = writeln!(out, "{label}\n  before start: {}", tree(&dir));
        let harness = Harness::with(scratch, args).await;
        let _ = writeln!(out, "  after start: {}", tree(&dir));
        if agents {
            join(&harness, "node-a").await;
            let _ = writeln!(out, "  after an agent joins: {}", tree(&dir));
        }
        let Harness { running, .. } = harness;
        running.shutdown().await.unwrap();
        let _ = writeln!(out, "  after shutdown: {}", tree(&dir));
    }
    out
}

/// Every section, computed in scratch directories named for `tag`, so tests that run
/// at once do not share one.
async fn current(tag: &str) -> Vec<(&'static str, String)> {
    vec![
        ("flags", flags()),
        ("routes", routes(tag).await),
        ("routes-with-agents", routes_with_agents(tag).await),
        ("keep-alive", keep_alive(tag).await),
        ("state-files", state_files(tag).await),
    ]
}

/// The golden file as text from `sections`.
fn compose(sections: &[(&str, String)]) -> String {
    let mut text = String::from(
        "# What the server does with no scheduler flag. Do not edit by hand: regenerate on\n\
         # purpose (see tests/server/gpu_path_pin.rs) and justify each changed line.\n",
    );
    for (name, body) in sections {
        let _ = write!(text, "\n== {name} ==\n{}\n", body.trim_end());
    }
    text
}

/// The text of one section of `golden`.
fn section<'a>(golden: &'a str, name: &str) -> &'a str {
    let head = format!("== {name} ==\n");
    let start = golden
        .find(&head)
        .unwrap_or_else(|| panic!("golden has no section {name}; regenerate: {REGENERATE}"))
        + head.len();
    let rest = &golden[start..];
    let end = rest.find("\n== ").map_or(rest.len(), |at| at + 1);
    rest[..end].trim_end()
}

/// Asserts that every pinned surface of the GPU path reads as it did when the golden was
/// recorded.
#[tokio::test]
async fn the_gpu_path_matches_its_golden() {
    let golden = fs::read_to_string(GOLDEN)
        .unwrap_or_else(|err| panic!("cannot read {GOLDEN}: {err}; regenerate: {REGENERATE}"));
    let mut wrong = Vec::new();
    for (name, body) in current("golden").await {
        let expected = section(&golden, name);
        if body.trim_end() != expected {
            wrong.push(format!(
                "section {name} changed\n--- golden\n{expected}\n--- now\n{}",
                body.trim_end()
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{}\nregenerate on purpose with: {REGENERATE}",
        wrong.join("\n")
    );
    let names: Vec<&str> = golden
        .lines()
        .filter_map(|line| line.strip_prefix("== ")?.strip_suffix(" =="))
        .collect();
    assert_eq!(names, SECTIONS, "the golden holds exactly these sections");
}

/// Asserts that two runs of the pinned scenarios give the same text: nothing in a section
/// depends on the port, the run directory, the order of a hash map or the time.
#[tokio::test]
async fn the_pinned_text_does_not_vary_between_runs() {
    let first = compose(&current("first").await);
    let second = compose(&current("second").await);
    assert_eq!(first, second);
}

/// Rewrites `golden/gpu_path.txt`. Run it only on purpose, with `JS_REGENERATE_GOLDEN=1`.
#[tokio::test]
#[ignore = "regenerates the golden file; run it only on purpose"]
async fn regenerate_gpu_path_golden() {
    assert!(
        std::env::var_os("JS_REGENERATE_GOLDEN").is_some_and(|value| value == "1"),
        "refusing to rewrite the golden: set JS_REGENERATE_GOLDEN=1 to do it on purpose"
    );
    fs::write(GOLDEN, compose(&current("regenerate").await)).unwrap();
}
