//! The whole-node class over HTTP: a lease on a registered host is committed and stays
//! pending until the host's agent reports it ready, and a run waits for the same report.
//! (`gpu-host` has not opted into this gate; its tests are the other agent tests.)

use serde_json::{Value, json};

use crate::agents_support::{JOIN, POOL, Session, agent_args, join, post};
use crate::runs_support::{queue, run_body, runs};
use crate::support::{Harness, Scratch, lease_body, replace};

/// The pool file of a pool whose class is `whole-node`.
fn whole_pool() -> String {
    POOL.replace("gpu-host", "whole-node")
}

/// The base request, for the `whole-node` class.
fn whole_body() -> Value {
    let mut body = lease_body();
    body["class"] = json!("whole-node");
    body
}

/// The flags of a server whose registered pool serves `whole-node`.
fn args(scratch: &Scratch) -> Vec<std::ffi::OsString> {
    let args = agent_args(scratch, false);
    scratch.write("agent-pool.toml", &whole_pool(), 0o644);
    args
}

/// A server with agent registration on and a `whole-node` pool.
async fn start(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let args = args(&scratch);
    Harness::with(scratch, args, None).await
}

/// The heartbeat of `session` that reports its host ready for lease `id`.
async fn report_ready(h: &Harness, session: &Session, id: &str) {
    let mut message = session.beat();
    message["exclusive_ready"] = json!(id);
    let ready = post(h, "/v1/agent/heartbeat", Some(JOIN), &message).await;
    assert_eq!(ready.status, 200, "{}", ready.body);
}

/// Asserts the commit over HTTP. A whole-node request is 202 pending and the host's
/// heartbeat reply names the lease in `exclusive`; the lease is not granted at the next
/// tick, only after the agent reports `exclusive_ready` for exactly that lease. It
/// catches a lease granted before the host is prepared and a reply that does not tell
/// the agent what to prepare for.
#[tokio::test]
async fn a_whole_node_lease_is_pending_until_the_agent_reports_ready() {
    let h = start("whole_pending").await;
    let me = join(&h, "burst-1").await;
    let asked = h.call("POST", "/v1/leases", Some(&whole_body())).await;
    assert_eq!(asked.status, 202, "{}", asked.body);
    let id = asked.json()["id"].as_str().unwrap().to_string();

    let reply = post(&h, "/v1/agent/heartbeat", Some(JOIN), &me.beat()).await;
    assert_eq!(reply.json()["exclusive"], json!(id));
    h.ticks.step().await;
    let listed = h.call("GET", "/v1/leases", None).await.json();
    assert_ne!(listed["leases"][0]["host"], "burst-1", "{listed}");

    report_ready(&h, &me, &id).await;
    h.ticks.step().await;
    let listed = h.call("GET", "/v1/leases", None).await.json();
    assert_eq!(listed["leases"][0]["host"], "burst-1", "{listed}");
}

/// Asserts the run gate of a whole-node lease over HTTP. A lease restored by a restart
/// sits on a host whose agent has said hello again but has not reported `exclusive_ready`
/// for it (the preparation is in memory only): a run is 409 `not-ready` and nothing is
/// queued; once the agent reports ready for exactly that lease the same request is
/// accepted 202. It catches a run queued on an unprepared host, a restored lease that
/// reads ready, and a refusal with no HTTP code.
#[tokio::test]
async fn a_run_waits_for_a_restored_whole_node_lease_to_be_ready() {
    let first = start("whole_not_ready").await;
    let old = join(&first, "burst-1").await;
    let asked = first.call("POST", "/v1/leases", Some(&whole_body())).await;
    let id = asked.json()["id"].as_str().unwrap().to_string();
    report_ready(&first, &old, &id).await;
    first.ticks.step().await;

    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let resumed = replace(args(&scratch), "--boot", "resume");
    let h = Harness::with(scratch, resumed, None).await;
    let me = join(&h, "burst-1").await;
    let early = queue(&h, &id, &run_body(1000)).await;
    assert_eq!(early.status, 409, "{}", early.body);
    assert_eq!(early.json(), json!({ "error": "not-ready" }));
    assert!(runs(&h, &id).await.is_empty());

    report_ready(&h, &me, &id).await;
    let accepted = queue(&h, &id, &run_body(1000)).await;
    assert_eq!(accepted.status, 202, "{}", accepted.body);
}
