//! Pins what the server does today with no scheduler flag: the flags and their defaults,
//! the status of every documented route for a request with no token, a wrong token and
//! the right one, the files the server leaves in its state directory, and the bytes of the
//! lease file after an acquire, a renew and a release under a fixed clock.
//!
//! The expected text is `golden/gpu_path.txt`, in sections. A change that alters any of
//! it edits the golden in its own diff, where a reviewer sees it. Regenerate on purpose
//! with
//!
//! ```text
//! cargo test -p js-lease-server --test server -- --ignored regenerate_gpu_path_golden
//! ```
//!
//! Nothing here measures time: the clock is a `ManualClock` that only the test moves, and
//! the tick loop runs on a `ChannelTicker` that only the test steps.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::CommandFactory;
use js_lease_server::Cli;

use crate::agents_support::{agent_args, join};
use crate::support::{Harness, Reply, Scratch, TOKEN, base_args, lease_body, replace, request};

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/server/golden/gpu_path.txt"
);

const REGENERATE: &str =
    "cargo test -p js-lease-server --test server -- --ignored regenerate_gpu_path_golden";

const WRONG_TOKEN: &str = "wrong-token-0123456789-abcdefghijklmnop";

/// The sections of the golden file, in file order.
const SECTIONS: [&str; 5] = [
    "flags",
    "routes",
    "routes-with-agents",
    "state-files",
    "lease-file",
];

/// Every flag by name with its default and whether it is required, sorted.
fn flags() -> String {
    let command = Cli::command();
    let mut rows: Vec<String> = command
        .get_arguments()
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

/// The client routes of the documentation table, the run routes and the agent routes
/// (which do not exist without the agent flags), a path under `/v1/sched/` and two
/// wrong-method requests. Order matters: the valid-token requests change the table.
fn probes() -> Vec<Probe> {
    let acquire = || Some(lease_body());
    vec![
        ("GET", "/healthz", None),
        ("POST", "/v1/leases", acquire()),
        ("POST", "/v1/leases", acquire()),
        ("POST", "/v1/leases", acquire()),
        ("GET", "/v1/leases", None),
        ("GET", "/v1/hosts", None),
        ("POST", "/v1/leases/1-1/renew", None),
        ("POST", "/v1/leases/9-9/renew", None),
        ("GET", "/v1/leases/1-1/renew", None),
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
        ("DELETE", "/v1/leases/1-1", None),
        ("PUT", "/v1/leases", None),
    ]
}

/// What a reply looks like in the golden: its status and its body, if any.
fn show(reply: &Reply) -> String {
    match reply.body.is_empty() {
        true => format!("{}", reply.status),
        false => format!("{} {}", reply.status, reply.body.trim_end()),
    }
}

/// For each probe, the reply with no token, with a wrong token and with `token`.
async fn matrix(harness: &Harness, token: &str) -> String {
    let mut out = String::new();
    for (method, path, body) in probes() {
        let body = body.map(|body| body.to_string().into_bytes());
        let addr = harness.addr();
        let none = request(addr, method, path, None, body.as_deref()).await;
        let wrong = request(addr, method, path, Some(WRONG_TOKEN), body.as_deref()).await;
        let valid = request(addr, method, path, Some(token), body.as_deref()).await;
        let _ = writeln!(
            out,
            "{method} {path}\n  no token: {}\n  wrong token: {}\n  valid token: {}",
            show(&none),
            show(&wrong),
            show(&valid)
        );
    }
    out
}

/// The routes of a server with no agent flags.
async fn routes(tag: &str) -> String {
    let harness = Harness::new(&format!("gpu_path_{tag}_routes")).await;
    matrix(&harness, TOKEN).await
}

/// The same requests on a server with agent registration on; the valid token is the
/// client token, which the agent routes refuse.
async fn routes_with_agents(tag: &str) -> String {
    let scratch = Scratch::new(&format!("gpu_path_{tag}_routes_agents"));
    let args = agent_args(&scratch, true);
    let harness = Harness::with(scratch, args, None).await;
    matrix(&harness, TOKEN).await
}

/// The base flags with the lease file in a directory of its own, which holds nothing else.
fn args_with_state_dir(scratch: &Scratch, agents: bool) -> (Vec<OsString>, PathBuf) {
    let dir = scratch.path("state");
    fs::create_dir_all(&dir).unwrap();
    let args = match agents {
        true => agent_args(scratch, true),
        false => base_args(scratch),
    };
    let args = replace(args, "--state-file", dir.join("leases.toml"));
    (args, dir)
}

/// The names in `dir`, sorted, with a mark for a directory.
fn names(dir: &Path) -> String {
    let found: BTreeSet<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let mark = match entry.file_type().unwrap().is_dir() {
                true => "/",
                false => "",
            };
            format!("{}{mark}", entry.file_name().to_string_lossy())
        })
        .collect();
    match found.is_empty() {
        true => "(empty)".to_owned(),
        false => found.into_iter().collect::<Vec<_>>().join(" "),
    }
}

/// The files in the state directory after a start, after an acquire and after a
/// shutdown, with and without the agent flags (where an agent also joins).
async fn state_files(tag: &str) -> String {
    let mut out = String::new();
    for (label, agents) in [("without agent flags", false), ("with agent flags", true)] {
        let scratch = Scratch::new(&format!("gpu_path_{tag}_files_{agents}"));
        let (args, dir) = args_with_state_dir(&scratch, agents);
        let _ = writeln!(out, "{label}\n  before start: {}", names(&dir));
        let harness = Harness::with(scratch, args, None).await;
        let _ = writeln!(out, "  after start: {}", names(&dir));
        let granted = harness
            .call("POST", "/v1/leases", Some(&lease_body()))
            .await;
        assert_eq!(granted.status, 201, "{}", granted.body);
        let _ = writeln!(out, "  after acquire: {}", names(&dir));
        if agents {
            join(&harness, "node-a").await;
            let _ = writeln!(out, "  after an agent joins: {}", names(&dir));
        }
        let Harness { running, .. } = harness;
        running.shutdown().await.unwrap();
        let _ = writeln!(out, "  after shutdown: {}", names(&dir));
    }
    out
}

/// The lease file after an acquire, a renew, a tick, a release, and the shutdown, with
/// the clock moved by hand between them.
async fn lease_file(tag: &str) -> String {
    let scratch = Scratch::new(&format!("gpu_path_{tag}_bytes"));
    let (args, dir) = args_with_state_dir(&scratch, false);
    let harness = Harness::with(scratch, args, None).await;
    let file = dir.join("leases.toml");
    let mut out = String::new();
    let mut note = |step: &str| {
        let text = fs::read_to_string(&file).unwrap();
        let _ = writeln!(out, "after {step}\n{text}");
    };
    note("start");
    let granted = harness
        .call("POST", "/v1/leases", Some(&lease_body()))
        .await;
    assert_eq!(granted.status, 201, "{}", granted.body);
    note("acquire");
    harness.clock.advance(Duration::from_secs(100));
    let renewed = harness.call("POST", "/v1/leases/1-1/renew", None).await;
    assert_eq!(renewed.status, 200, "{}", renewed.body);
    note("renew");
    harness.clock.advance(Duration::from_secs(10));
    harness.ticks.step().await;
    note("tick");
    let released = harness.call("DELETE", "/v1/leases/1-1", None).await;
    assert_eq!(released.status, 200, "{}", released.body);
    note("release");
    let Harness { running, .. } = harness;
    running.shutdown().await.unwrap();
    note("shutdown");
    out
}

/// Every section, computed in scratch directories named for `tag`, so tests that run
/// at once do not share one.
async fn current(tag: &str) -> Vec<(&'static str, String)> {
    vec![
        ("flags", flags()),
        ("routes", routes(tag).await),
        ("routes-with-agents", routes_with_agents(tag).await),
        ("state-files", state_files(tag).await),
        ("lease-file", lease_file(tag).await),
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

/// Rewrites `golden/gpu_path.txt`. Run it only on purpose.
#[tokio::test]
#[ignore = "regenerates the golden file; run it only on purpose"]
async fn regenerate_gpu_path_golden() {
    fs::write(GOLDEN, compose(&current("regenerate").await)).unwrap();
}
