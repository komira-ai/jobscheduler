//! What the run tests share: a scripted agent that speaks the agent routes (a heartbeat
//! with the runs it holds and the results it carries), and the calls a lease holder makes
//! to the run routes.

use std::time::Duration;

use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, post};
use crate::support::{Harness, Reply, lease_body};

/// A job of `cpu` thousandths of a core and 1 MiB, with every other field left to its
/// default.
pub fn run_body(cpu: u64) -> Value {
    json!({
        "image": "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "args": ["true"],
        "limits": { "cpu_millis": cpu, "memory_bytes": 1048576 },
    })
}

/// A lease on the one registered host, expected to be granted. Returns its id.
pub async fn lease(h: &Harness) -> String {
    let granted = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(granted.status, 201, "{}", granted.body);
    granted.json()["id"].as_str().unwrap().to_string()
}

/// `POST /v1/leases/{lease}/runs` with `body`.
pub async fn queue(h: &Harness, lease: &str, body: &Value) -> Reply {
    let path = format!("/v1/leases/{lease}/runs");
    h.call("POST", &path, Some(body)).await
}

/// A run expected to be queued. Returns its id.
pub async fn queued(h: &Harness, lease: &str, cpu: u64) -> String {
    let reply = queue(h, lease, &run_body(cpu)).await;
    assert_eq!(reply.status, 202, "{}", reply.body);
    assert_eq!(reply.json()["state"], "queued");
    reply.json()["run_id"].as_str().unwrap().to_string()
}

/// `DELETE /v1/leases/{lease}/runs/{run}`.
pub async fn stop(h: &Harness, lease: &str, run: &str) -> Reply {
    let path = format!("/v1/leases/{lease}/runs/{run}");
    h.call("DELETE", &path, None).await
}

/// `GET /v1/leases/{lease}/runs`, expected to answer 200: the `runs` array.
pub async fn runs(h: &Harness, lease: &str) -> Vec<Value> {
    let path = format!("/v1/leases/{lease}/runs");
    let reply = h.call("GET", &path, None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()["runs"].as_array().unwrap().clone()
}

/// The state the listing gives `run`.
pub async fn state(h: &Harness, lease: &str, run: &str) -> String {
    let listing = runs(h, lease).await;
    let entry = listing.iter().find(|entry| entry["run_id"] == run);
    let entry = entry.unwrap_or_else(|| panic!("{run} is not listed: {listing:?}"));
    entry["state"].as_str().unwrap().to_string()
}

/// A run the agent holds, as the heartbeat lists it.
pub fn held(run: &str, state: &str) -> Value {
    json!({ "run_id": run, "state": state })
}

/// A result of a run that succeeded.
pub fn result(run: &str, lease: &str) -> Value {
    json!({ "run_id": run, "lease_id": lease, "outcome": "succeeded", "exit_code": 0 })
}

/// The heartbeat of `session` with these held runs and results, answered 200: the reply.
pub async fn beat(h: &Harness, session: &Session, held: &[Value], results: &[Value]) -> Value {
    let mut body = session.beat();
    body["held"] = json!(held);
    body["results"] = json!(results);
    let reply = post(h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()
}

/// The commands of a reply as `start <run>`, `stop <run>` and `cancel <run>`.
pub fn words(reply: &Value) -> Vec<String> {
    let commands = reply["commands"].as_array().unwrap();
    let word = |command: &Value| {
        let kind = command["type"].as_str().unwrap();
        format!("{kind} {}", command["run_id"].as_str().unwrap())
    };
    commands.iter().map(word).collect()
}

/// The acknowledgements of a reply as `<run> <VERDICT>`.
pub fn verdicts(reply: &Value) -> Vec<String> {
    let acks = reply["result_acks"].as_array().unwrap();
    let word = |ack: &Value| {
        format!(
            "{} {}",
            ack["run_id"].as_str().unwrap(),
            ack["ack"].as_str().unwrap()
        )
    };
    acks.iter().map(word).collect()
}

/// Lets `seconds` pass in steps of ten, the agent heartbeating (and listing `held`) at
/// each, so the host stays inside its grace period.
pub async fn idle(h: &Harness, session: &Session, seconds: u64, held: &[Value]) {
    for _ in 0..seconds / 10 {
        h.clock.advance(Duration::from_secs(10));
        beat(h, session, held, &[]).await;
    }
}
