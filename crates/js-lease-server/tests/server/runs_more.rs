//! More of the run routes: a run id the server no longer holds, one lease reaching for
//! another's run, what bounds a run in time while its lease is renewed, and a host that
//! reports a taint.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::agents_support::{
    JOIN, Session, entry, hello_body, hosts, join, post, restarted, start_agents,
};
use crate::runs_support::{beat, held, lease, queue, queued, run_body, runs, state, stop, words};
use crate::support::{Harness, lease_body};

/// `GET /v1/leases/{lease}/runs/{run}`.
async fn show(h: &Harness, lease: &str, run: &str) -> crate::support::Reply {
    h.call("GET", &format!("/v1/leases/{lease}/runs/{run}"), None)
        .await
}

/// A heartbeat of `session` carrying `extra` fields (a taint, results), answered 200.
async fn beat_with(h: &Harness, session: &Session, extra: Value) -> Value {
    let mut body = session.beat();
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    let reply = post(h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()
}

fn agent_file(h: &Harness) -> PathBuf {
    let mut path = h.scratch.state().into_os_string();
    path.push(".agents");
    PathBuf::from(path)
}

/// Asserts what a client is told about a run after a restart: the lease is still there,
/// and a `GET` or `DELETE` of a run id minted before the restart answers 410
/// `run-forgotten`, while an id of this boot that does not exist answers 404 `unknown-run`
/// and a run queued after the restart reads normally. The agent kills the forgotten run
/// without a result, so the answer is all the client gets. It catches a restart that leaves
/// the client unable to tell "killed by the restart" from "never existed".
#[tokio::test]
async fn a_run_id_of_an_earlier_boot_is_gone() {
    let first = start_agents("runs_forgotten", false).await;
    let old = join(&first, "burst-1").await;
    let lease = lease(&first).await;
    let run = queued(&first, &lease, 1000).await;
    assert_eq!(show(&first, &lease, &run).await.status, 200);
    beat(&first, &old, &[held(&run, "running")], &[]).await;

    let h = restarted(first, false).await;
    let welcome = post(&h, "/v1/agent/hello", Some(JOIN), &hello_body("burst-1")).await;
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    let gone = json!({ "error": "run-forgotten" });
    let read = show(&h, &lease, &run).await;
    assert_eq!((read.status, read.json()), (410, gone.clone()));
    let stopped = stop(&h, &lease, &run).await;
    assert_eq!((stopped.status, stopped.json()), (410, gone));
    let never = show(&h, &lease, "run-2-9").await;
    assert_eq!(
        (never.status, never.json()),
        (404, json!({ "error": "unknown-run" }))
    );
    let odd = show(&h, &lease, "ghost").await;
    assert_eq!(odd.status, 404, "{}", odd.body);
    let fresh = queued(&h, &lease, 1000).await;
    assert_eq!(fresh, "run-2-1");
    let read = show(&h, &lease, &fresh).await;
    assert_eq!(read.status, 200, "{}", read.body);
    assert_eq!(read.json()["state"], "queued");
}

/// Asserts that one lease cannot read or stop another lease's run: with two leases on two
/// hosts, a `GET` and a `DELETE` of lease A's run through lease B's path are 404
/// `unknown-run`, the run is not asked to stop (it still reads `queued`, and the host's
/// next reply starts it rather than stops it), and through its own path both work. It
/// catches a run reachable through any lease path.
#[tokio::test]
async fn one_lease_cannot_reach_another_leases_run() {
    let h = start_agents("runs_cross_lease", false).await;
    let a = join(&h, "burst-1").await;
    let _b = join(&h, "burst-2").await;
    let first = lease(&h).await;
    let second = lease(&h).await;
    assert_ne!(first, second);
    let run = queued(&h, &first, 1000).await;

    let read = show(&h, &second, &run).await;
    assert_eq!(
        (read.status, read.json()),
        (404, json!({ "error": "unknown-run" }))
    );
    let stopped = stop(&h, &second, &run).await;
    assert_eq!(
        (stopped.status, stopped.json()),
        (404, json!({ "error": "unknown-run" }))
    );
    assert!(runs(&h, &second).await.is_empty());
    assert_eq!(state(&h, &first, &run).await, "queued");
    let reply = beat(&h, &a, &[], &[]).await;
    assert_eq!(words(&reply), [format!("start {run}")]);

    assert_eq!(show(&h, &first, &run).await.status, 200);
    assert_eq!(stop(&h, &first, &run).await.status, 202);
}

/// Asserts that a run is bounded by the lease it was queued under even when the lease is
/// renewed: the start carries the time left at each send (100 s, then 60 s after 40 s), a
/// renewal does not stretch it, and once that time is up the reply carries a stop and no
/// start, and the run is given up as never started while the lease lives on. It catches
/// a start that never goes stale while the host is connected and a run that holds its
/// budget for as long as its lease is renewed.
#[tokio::test]
async fn a_run_is_bounded_by_the_lease_time_it_was_queued_with() {
    let h = start_agents("runs_bound_renewed", false).await;
    let me = join(&h, "burst-1").await;
    let mut ask = lease_body();
    ask["ttl_seconds"] = json!(100);
    let granted = h.call("POST", "/v1/leases", Some(&ask)).await;
    let lease = granted.json()["id"].as_str().unwrap().to_string();
    let run = queued(&h, &lease, 8000).await;

    let mut reply = beat(&h, &me, &[], &[]).await;
    assert_eq!(reply["commands"][0]["valid_for_ms"], 100_000);
    for _ in 0..4 {
        h.clock.advance(Duration::from_secs(10));
        reply = beat(&h, &me, &[], &[]).await;
    }
    assert_eq!(reply["commands"][0]["valid_for_ms"], 60_000);
    assert_eq!(reply["commands"][0]["limits"]["wall_ms"], 60_000);

    let renewed = h
        .call("POST", &format!("/v1/leases/{lease}/renew"), None)
        .await;
    assert_eq!(renewed.status, 200, "{}", renewed.body);
    for _ in 0..6 {
        h.clock.advance(Duration::from_secs(10));
        reply = beat(&h, &me, &[], &[]).await;
    }
    assert_eq!(words(&reply), [format!("stop {run}")]);
    assert_eq!(state(&h, &lease, &run).await, "stopping");
    reply = beat(&h, &me, &[], &[]).await;
    assert!(words(&reply).is_empty());
    assert_eq!(state(&h, &lease, &run).await, "never-started");
    assert_eq!(queue(&h, &lease, &run_body(8000)).await.status, 202);
}

/// Asserts what a taint report does end to end: the reply acknowledges it, `GET /v1/hosts`
/// shows it, a run queued before it is not started, a new run is 409 `host-tainted`, and
/// the report is in the state file before the acknowledgement. A heartbeat without a report
/// is told `taint_ack` false. It catches a report that is read and ignored and one
/// acknowledged before it is durable.
#[tokio::test]
async fn a_tainted_host_is_shown_acknowledged_and_sent_no_start() {
    let h = start_agents("runs_taint", false).await;
    let me = join(&h, "burst-1").await;
    let lease = lease(&h).await;
    let run = queued(&h, &lease, 1000).await;
    assert_eq!(entry(&hosts(&h).await, "burst-1").get("taint"), None);

    let taint = json!({ "taint": { "reason": "residue", "run_id": "run-1-1", "check": "tmp" } });
    let reply = beat_with(&h, &me, taint).await;
    assert_eq!(reply["taint_ack"], true);
    assert!(words(&reply).is_empty());
    let shown = entry(&hosts(&h).await, "burst-1");
    assert_eq!(
        shown["taint"],
        json!({ "reason": "residue", "run_id": "run-1-1", "check": "tmp" })
    );
    let text = fs::read_to_string(agent_file(&h)).unwrap();
    assert!(text.contains("taint_reason = \"residue\""), "{text}");
    assert!(text.contains("taint_check = \"tmp\""), "{text}");

    let refused = queue(&h, &lease, &run_body(1000)).await;
    assert_eq!(
        (refused.status, refused.json()),
        (409, json!({ "error": "host-tainted" }))
    );
    let reply = beat(&h, &me, &[], &[]).await;
    assert!(words(&reply).is_empty());
    assert_eq!(reply["taint_ack"], false);
    assert_eq!(state(&h, &lease, &run).await, "queued");
}

/// Asserts that a taint report is not acknowledged while it cannot be written: the host is
/// tainted at once (it is sent no start), the reply says `taint_ack` false and the failure
/// is counted, and once the file can be written the next report is acknowledged. It
/// catches an agent told to stop reporting a taint the server could lose in a restart.
#[tokio::test]
async fn a_taint_is_not_acknowledged_before_it_can_be_written() {
    let h = start_agents("runs_taint_write", false).await;
    let me = join(&h, "burst-1").await;
    let lease = lease(&h).await;
    queued(&h, &lease, 1000).await;
    let blocker = h.scratch.path("leases.toml.agents.tmp");
    fs::create_dir(&blocker).unwrap();
    let errors = h.running.agent_errors();

    let taint = json!({ "taint": { "reason": "residue" } });
    let reply = beat_with(&h, &me, taint.clone()).await;
    assert_eq!(reply["taint_ack"], false);
    assert!(words(&reply).is_empty());
    assert_eq!(h.running.agent_errors(), errors + 1);
    assert_eq!(
        entry(&hosts(&h).await, "burst-1")["taint"]["reason"],
        "residue"
    );

    fs::remove_dir(&blocker).unwrap();
    let reply = beat_with(&h, &me, taint).await;
    assert_eq!(reply["taint_ack"], true);
    let text = fs::read_to_string(agent_file(&h)).unwrap();
    assert!(text.contains("taint_reason = \"residue\""), "{text}");
}

/// Asserts that a taint outlives a restart: a tainted host that holds a lease and one that
/// holds none are both put back awaiting with their taint, the first keeps its lease, a
/// run on the first is refused `host-tainted` after its agent says hello again, and the
/// taint is still in the state file. It catches a restart that makes a host clean.
#[tokio::test]
async fn a_taint_survives_a_restart() {
    let first = start_agents("runs_taint_restart", false).await;
    let a = join(&first, "burst-1").await;
    let b = join(&first, "burst-2").await;
    let lease = lease(&first).await;
    let taint = json!({ "taint": { "reason": "residue" } });
    let on_a = entry(&hosts(&first).await, "burst-1")["lease"].is_string();
    let (leased, idle) = if on_a { (a, b) } else { (b, a) };
    beat_with(&first, &leased, taint.clone()).await;
    beat_with(&first, &idle, taint).await;

    let h = restarted(first, false).await;
    let listing = hosts(&h).await;
    for id in ["burst-1", "burst-2"] {
        let shown = entry(&listing, id);
        assert_eq!(shown["state"], "awaiting", "{shown}");
        assert_eq!(shown["taint"]["reason"], "residue", "{shown}");
    }
    let welcome = post(&h, "/v1/agent/hello", Some(JOIN), &hello_body(&leased.host)).await;
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    let refused = queue(&h, &lease, &run_body(1000)).await;
    assert_eq!(
        (refused.status, refused.json()),
        (409, json!({ "error": "host-tainted" }))
    );
    let text = fs::read_to_string(agent_file(&h)).unwrap();
    assert_eq!(text.matches("taint_reason").count(), 2, "{text}");
}

/// Asserts that a start the agent refused as `host-tainted` taints the host although no
/// heartbeat reports a taint (the agent may be unable to write its marker): the result is
/// accepted like any other, `GET /v1/hosts` shows the taint, the state file holds it, a
/// run queued meanwhile is not started and a new one is 409 `host-tainted`. It catches an
/// agent with a full disk being sent work again and again.
#[tokio::test]
async fn a_start_refused_as_host_tainted_taints_the_host() {
    let h = start_agents("runs_taint_refused", false).await;
    let me = join(&h, "burst-1").await;
    let lease = lease(&h).await;
    let run = queued(&h, &lease, 4000).await;
    let waiting = queued(&h, &lease, 1000).await;
    let reply = beat(&h, &me, &[], &[]).await;
    assert_eq!(
        words(&reply),
        [format!("start {run}"), format!("start {waiting}")]
    );

    let refused = json!({
        "run_id": run, "lease_id": lease,
        "outcome": "failed_to_start", "detail": "host-tainted",
    });
    let both = json!([held(&run, "exited_unacked"), held(&waiting, "running")]);
    let reply = beat_with(&h, &me, json!({ "held": both, "results": [refused] })).await;
    assert_eq!(reply["result_acks"][0]["ack"], "ACCEPTED");
    assert_eq!(reply["taint_ack"], false);
    let shown = entry(&hosts(&h).await, "burst-1");
    assert_eq!(shown["taint"]["reason"], "host-tainted");
    assert_eq!(shown["taint"]["run_id"], run.as_str());
    let text = fs::read_to_string(agent_file(&h)).unwrap();
    assert!(text.contains("taint_reason = \"host-tainted\""), "{text}");
    let listing = crate::runs_support::runs(&h, &lease).await;
    assert_eq!(listing[0]["outcome"], "failed_to_start");
    assert_eq!(listing[0]["detail"], "host-tainted");

    let refusal = queue(&h, &lease, &run_body(1000)).await;
    assert_eq!(
        (refusal.status, refusal.json()),
        (409, json!({ "error": "host-tainted" }))
    );
}
