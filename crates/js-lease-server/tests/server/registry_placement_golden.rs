//! Pins what the registry's own placement (`--placement registry`, the default) does with
//! whole-node leases: a fixed script of hellos, acquires (with and without a request id),
//! heartbeats, ticks and releases over a pool of two registering hosts, with, after each
//! step, the answer the step got, the hosts listing (`GET /v1/hosts`: each host's mark,
//! `reserved_for` and drain deadline), the lease list and the bytes of the lease file.
//!
//! The expected text is `golden/registry_placement.txt`, one section per step. It was
//! recorded from the code before the placement engine moved into its own crate, so a
//! change to the registry's path shows as a changed line in a reviewed diff. Regenerate on
//! purpose with
//!
//! ```text
//! JS_REGENERATE_GOLDEN=1 cargo test -p js-lease-server --test server -- --ignored regenerate_registry_placement_golden
//! ```
//!
//! The regenerating test is ignored and refuses to run unless `JS_REGENERATE_GOLDEN` is
//! set, so running every ignored test cannot rewrite the golden. When the golden differs
//! or is missing, the failure prints the whole text the script produced now.
//!
//! Nothing here measures time: the clock is a `ManualClock` that only the script moves,
//! and the tick loop runs on a `ChannelTicker` that only the script steps. The session
//! secrets the hellos are answered with are written as `<session:HOST>` wherever they
//! appear, so the text does not depend on them.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::time::Duration;

use serde_json::{Value, json};

use crate::agents_support::{POOL, Session, agent_args, beat, beat_preparing, join};
use crate::support::{Harness, Reply, Scratch, lease_body};

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/server/golden/registry_placement.txt"
);

const REGENERATE: &str = "JS_REGENERATE_GOLDEN=1 cargo test -p js-lease-server --test server -- --ignored regenerate_registry_placement_golden";

/// The request id the script's queued request carries.
const ID: &str = "nightly-7";

/// The flags of a server whose registered pool serves `whole-node` on two hosts, placing
/// with the registry (no `--placement` flag), with a grace long enough that a host the
/// script does not beat stays serving.
fn args(scratch: &Scratch) -> Vec<OsString> {
    let args = agent_args(scratch, false);
    let pool = POOL
        .replace("gpu-host", "whole-node")
        .replace("max_machines = 2\n", "max_machines = 2\ngrace = \"2h\"\n");
    scratch.write("agent-pool.toml", &pool, 0o644);
    args
}

/// A whole-node acquire, under `id` when one is given.
fn whole(id: Option<&str>) -> Value {
    let mut body = lease_body();
    body["class"] = json!("whole-node");
    if let Some(id) = id {
        body["request_id"] = json!(id);
    }
    body
}

/// What the script has seen so far, and the sessions whose secrets it hides.
struct Script {
    h: Harness,
    sessions: Vec<Session>,
    text: Vec<(String, String)>,
}

impl Script {
    async fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let args = args(&scratch);
        let h = Harness::with(scratch, args, None).await;
        Self {
            h,
            sessions: Vec::new(),
            text: Vec::new(),
        }
    }

    /// `text` with every session secret the script holds written as `<session:HOST>`.
    fn hide(&self, text: &str) -> String {
        let mut text = text.to_string();
        for session in &self.sessions {
            text = text.replace(&session.secret, &format!("<session:{}>", session.host));
        }
        text
    }

    /// The host `name` says hello; its session.
    async fn hello(&mut self, name: &str) -> usize {
        let session = join(&self.h, name).await;
        self.sessions.push(session);
        self.sessions.len() - 1
    }

    async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Reply {
        self.h.call(method, path, body).await
    }

    /// The answer `reply`, as the section shows it.
    fn answer(reply: &Reply) -> String {
        format!("{} {}", reply.status, reply.body)
    }

    /// Records the section `name`: the answers `answers` the step got, then the hosts
    /// listing, the lease list and the lease file as they stand.
    async fn record(&mut self, name: &str, answers: &[String]) {
        let mut body = String::new();
        for answer in answers {
            let _ = writeln!(body, "answer: {answer}");
        }
        let hosts = self.call("GET", "/v1/hosts", None).await;
        let leases = self.call("GET", "/v1/leases", None).await;
        let pretty = |reply: &Reply| {
            let parsed: Value = serde_json::from_str(&reply.body).unwrap_or(Value::Null);
            serde_json::to_string_pretty(&parsed).unwrap_or_default()
        };
        let _ = writeln!(body, "-- hosts {}\n{}", hosts.status, pretty(&hosts));
        let _ = writeln!(body, "-- leases {}\n{}", leases.status, pretty(&leases));
        let file = fs::read_to_string(self.h.scratch.state()).unwrap_or_default();
        let _ = writeln!(body, "-- lease file\n{file}");
        let body = self.hide(&body);
        self.text.push((name.to_string(), body));
    }

    /// The id of the lease `reply` made, or the empty string.
    fn id(reply: &Reply) -> String {
        let id = reply.json()["id"].as_str().map(str::to_string);
        id.unwrap_or_default()
    }

    async fn acquire(&self, id: Option<&str>) -> Reply {
        self.call("POST", "/v1/leases", Some(&whole(id))).await
    }

    async fn release(&self, lease: &str) -> Reply {
        self.call("DELETE", &format!("/v1/leases/{lease}"), None)
            .await
    }

    /// One heartbeat of session `at`, and the ready report of the lease it is told of.
    async fn prepare(&self, at: usize) -> Reply {
        beat_preparing(&self.h, &self.sessions[at]).await
    }

    /// One plain heartbeat of session `at`.
    async fn beat(&self, at: usize) -> Reply {
        beat(&self.h, &self.sessions[at]).await
    }

    async fn tick(&self) {
        self.h.ticks.step().await;
    }
}

/// Runs the fixed script and returns its sections, in order.
async fn current(name: &str) -> Vec<(String, String)> {
    let mut s = Script::new(name).await;
    let one = s.hello("burst-1").await;
    let two = s.hello("burst-2").await;
    s.record("hellos", &[]).await;

    // Two leases without an id, each committed on a quiet host and made held.
    let first = s.acquire(None).await;
    let told = s.prepare(one).await;
    s.tick().await;
    let answers = [Script::answer(&first), Script::answer(&told)];
    s.record("first-whole-lease", &answers).await;
    let second = s.acquire(None).await;
    let told = s.prepare(two).await;
    s.tick().await;
    let answers = [Script::answer(&second), Script::answer(&told)];
    s.record("second-whole-lease", &answers).await;

    // A request under an id waits; one without an id is refused.
    let named = s.acquire(Some(ID)).await;
    s.tick().await;
    let repeat = s.acquire(Some(ID)).await;
    let plain = s.acquire(None).await;
    let answers = [&named, &repeat, &plain].map(Script::answer);
    s.record("queued", &answers).await;

    // The first lease ends: its host is set aside for the waiting request, then
    // committed once it is quiet, and held after the ready report.
    let freed = s.release(&Script::id(&first)).await;
    s.tick().await;
    s.record("reserved", &[Script::answer(&freed)]).await;
    let beats = [s.beat(one).await, s.beat(one).await];
    s.tick().await;
    let answers = beats.each_ref().map(Script::answer);
    s.record("committed", &answers).await;
    let told = s.prepare(one).await;
    s.tick().await;
    let again = s.acquire(Some(ID)).await;
    let answers = [Script::answer(&told), Script::answer(&again)];
    s.record("held", &answers).await;

    // Time passes; the second lease ends and its host serves again.
    s.h.clock.advance(Duration::from_secs(30));
    s.tick().await;
    let freed = s.release(&Script::id(&second)).await;
    let beats = [s.beat(two).await, s.beat(two).await];
    s.tick().await;
    let mut answers = vec![Script::answer(&freed)];
    answers.extend(beats.iter().map(Script::answer));
    s.record("second-freed", &answers).await;

    // The queued request's lease ends: no lease is left.
    let freed = s.release(&Script::id(&named)).await;
    let beats = [s.beat(one).await, s.beat(one).await];
    s.tick().await;
    let mut answers = vec![Script::answer(&freed)];
    answers.extend(beats.iter().map(Script::answer));
    s.record("all-freed", &answers).await;
    s.text
}

/// The golden file as text from `sections`.
fn compose(sections: &[(String, String)]) -> String {
    let mut text = String::from(
        "# What --placement registry does with whole-node leases. Do not edit by hand:\n\
         # regenerate on purpose (see tests/server/registry_placement_golden.rs) and justify\n\
         # each changed line.\n",
    );
    for (name, body) in sections {
        let _ = write!(text, "\n== {name} ==\n{}\n", body.trim_end());
    }
    text
}

/// Asserts that the registry's placement makes the same host choices and marks, shows the
/// same hosts listing and lease list and writes the same lease file, step by step, as when
/// the golden was recorded. It catches a change to the registry's whole-node path (a host
/// chosen otherwise, a mark set, moved or dropped at another step, a field of the listing
/// or the file that moved) that a change meant for `--placement js-sched` brought with it.
#[tokio::test]
async fn registry_placement_is_byte_identical_to_today() {
    let now = compose(&current("registry_golden").await);
    let golden = fs::read_to_string(GOLDEN).unwrap_or_default();
    assert!(
        golden == now,
        "the registry's placement no longer matches {GOLDEN}; regenerate on purpose with: \
         {REGENERATE}\n--- the script now writes ---\n{now}--- end ---"
    );
}

/// Asserts that two runs of the script give the same text: nothing in it depends on the
/// port, the run directory, a session secret, the order of a hash map or the time.
#[tokio::test]
async fn the_registry_script_does_not_vary_between_runs() {
    let first = compose(&current("registry_golden_a").await);
    let second = compose(&current("registry_golden_b").await);
    assert_eq!(first, second);
}

/// Rewrites `golden/registry_placement.txt`. Run it only on purpose, with
/// `JS_REGENERATE_GOLDEN=1`.
#[tokio::test]
#[ignore = "regenerates the golden file; run it only on purpose"]
async fn regenerate_registry_placement_golden() {
    assert!(
        std::env::var_os("JS_REGENERATE_GOLDEN").is_some_and(|value| value == "1"),
        "refusing to rewrite the golden: set JS_REGENERATE_GOLDEN=1 to do it on purpose"
    );
    fs::write(
        GOLDEN,
        compose(&current("registry_golden_regenerate").await),
    )
    .unwrap();
}
