//! The taint a host reports about itself: acknowledged once durable, shown in the host
//! listing, kept across a restart, and a reason for the scheduler to start no job on the
//! host. Jobs go through the job service; the agent speaks the per-job messages.

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, entry, hosts, post};
use crate::job_hosts::{cpu_hello, placing_server, restarted_placing};
use crate::jobs_support::{JobBody, commands, job_agent, job_of, submit_body};
use crate::support::{Harness, Kept, Reply, TOKEN, kept_bindings, request};

/// A heartbeat of `session` carrying `extra` fields (a taint), answered 200.
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

/// `ClearHostTaint` of `host` through the job service's gateway on `h`'s job port.
async fn clear_taint(h: &Harness, host: &str) -> Reply {
    let job = h.running.job_addr().expect("the job service is on");
    let path = format!("/v1/hosts/{host}:cleartaint");
    request(job, "POST", &path, Some(TOKEN), Some(b"{}")).await
}

/// Submits the job `id` (1 core, 1 GiB), expected to be queued.
async fn submit_job(h: &Harness, id: &str) {
    let queued = submit_body(h, &JobBody::new(id).json()).await;
    assert_eq!(queued.status, 201, "{}", queued.body);
}

/// Asserts what a taint report does end to end: the reply acknowledges it, `GET /v1/hosts`
/// shows it, a job submitted before it is not started on it, a job submitted after it is not
/// either (both start on a second host that reports no taint), and the report is in the
/// state file before the acknowledgement. A heartbeat without a report is told `taint_ack`
/// false. It catches a report that is read and ignored and one acknowledged before it is
/// durable.
#[tokio::test]
async fn a_tainted_host_is_shown_acknowledged_and_sent_no_start() {
    let h = placing_server("host_taint", Kept::Files).await;
    let me = job_agent(&h, &cpu_hello("burst-1")).await;
    let clean = job_agent(&h, &cpu_hello("burst-2")).await;
    submit_job(&h, "taint-before").await;
    assert_eq!(entry(&hosts(&h).await, "burst-1").get("taint"), None);

    let taint = json!({ "taint": { "reason": "residue", "run_id": "run-1-1", "check": "tmp" } });
    let reply = beat_with(&h, &me, taint).await;
    assert_eq!(reply["taint_ack"], true);
    assert_eq!(commands(&reply), (vec![], vec![]), "{reply}");
    let shown = entry(&hosts(&h).await, "burst-1");
    assert_eq!(
        shown["taint"],
        json!({ "reason": "residue", "run_id": "run-1-1", "check": "tmp" })
    );
    let text = fs::read_to_string(agent_file(&h)).unwrap();
    assert!(text.contains("taint_reason = \"residue\""), "{text}");
    assert!(text.contains("taint_check = \"tmp\""), "{text}");

    submit_job(&h, "taint-after").await;
    h.ticks.step().await;
    let reply = beat_with(&h, &me, json!({})).await;
    assert_eq!(commands(&reply), (vec![], vec![]), "{reply}");
    assert_eq!(reply["taint_ack"], false);
    let elsewhere = beat_with(&h, &clean, json!({})).await;
    let (mut starts, stops) = commands(&elsewhere);
    starts.sort();
    let jobs: Vec<&str> = starts.iter().map(|(job, _)| job.as_str()).collect();
    assert_eq!(jobs, ["taint-after", "taint-before"], "{elsewhere}");
    assert!(stops.is_empty(), "{elsewhere}");
}

/// Asserts that a taint report is not acknowledged while it cannot be written: the host is
/// tainted at once (it is sent no start), the reply says `taint_ack` false and the failure
/// is counted, and once the file can be written the next report is acknowledged, a tick
/// having placed nothing on the host meanwhile. It
/// catches an agent told to stop reporting a taint the server could lose in a restart.
#[tokio::test]
async fn a_taint_is_not_acknowledged_before_it_can_be_written() {
    let h = placing_server("host_taint_write", Kept::Files).await;
    let me = job_agent(&h, &cpu_hello("burst-1")).await;
    submit_job(&h, "taint-write").await;
    let blocker = h.scratch.path("leases.toml.agents.tmp");
    fs::create_dir(&blocker).unwrap();
    let errors = h.running.agent_errors();

    let taint = json!({ "taint": { "reason": "residue" } });
    let reply = beat_with(&h, &me, taint.clone()).await;
    assert_eq!(reply["taint_ack"], false);
    assert_eq!(commands(&reply), (vec![], vec![]), "{reply}");
    assert_eq!(h.running.agent_errors(), errors + 1);
    assert_eq!(
        entry(&hosts(&h).await, "burst-1")["taint"]["reason"],
        "residue"
    );

    fs::remove_dir(&blocker).unwrap();
    h.ticks.step().await;
    let reply = beat_with(&h, &me, taint).await;
    assert_eq!(reply["taint_ack"], true);
    assert_eq!(commands(&reply), (vec![], vec![]), "{reply}");
    let text = fs::read_to_string(agent_file(&h)).unwrap();
    assert!(text.contains("taint_reason = \"residue\""), "{text}");
}

/// Asserts that a taint outlives a restart: two tainted hosts are put back awaiting with
/// their taint, a job submitted after their agents say hello again is refused 422
/// `no-capable-host` (no host is clean) and recorded nowhere, and the taint is still kept
/// (in the state file, or in the store's host bindings). It runs with the bindings in files
/// and in the store, and in the store it also asserts that no `.agents` file is written. It
/// catches a restart that makes a host clean, and a store that loses a taint.
#[tokio::test]
async fn a_taint_survives_a_restart() {
    for kept in Kept::BOTH {
        a_taint_survives_a_restart_in(kept).await;
    }
}

async fn a_taint_survives_a_restart_in(kept: Kept) {
    let first = placing_server("host_taint_restart", kept).await;
    let a = job_agent(&first, &cpu_hello("burst-1")).await;
    let b = job_agent(&first, &cpu_hello("burst-2")).await;
    let taint = json!({ "taint": { "reason": "residue" } });
    beat_with(&first, &a, taint.clone()).await;
    beat_with(&first, &b, taint).await;

    let h = restarted_placing(first).await;
    let listing = hosts(&h).await;
    for id in ["burst-1", "burst-2"] {
        let shown = entry(&listing, id);
        assert_eq!(shown["state"], "awaiting", "{shown}");
        assert_eq!(shown["taint"]["reason"], "residue", "{shown}");
    }
    job_agent(&h, &cpu_hello("burst-1")).await;
    job_agent(&h, &cpu_hello("burst-2")).await;
    let refused = submit_body(&h, &JobBody::new("taint-restart").json()).await;
    assert_eq!(
        (refused.status, refused.json()),
        (422, json!({ "error": "no-capable-host" }))
    );
    assert_eq!(job_of(&h, "taint-restart").await.status, 404);
    let text = kept_bindings(&h);
    assert_eq!(text.matches("taint_reason").count(), 2, "{text}");
    for id in ["burst-1", "burst-2"] {
        assert!(text.contains(id), "{text}");
    }
    if kept == Kept::Store {
        assert!(!agent_file(&h).exists());
    }
}

/// Asserts that a start the agent refused as `host-tainted` taints the host here too, and
/// that the state file is made current before the reply: the beat carries no taint report,
/// so the reply says `taint_ack` false, `GET /v1/hosts` shows a `host-tainted` taint naming
/// the run, and the bindings file holds that taint. A beat whose result carries another
/// detail leaves the host clean and its file without a taint. It catches a refused start
/// that taints the host in memory only, which a restart would make clean.
#[tokio::test]
async fn a_start_refused_as_host_tainted_taints_the_host_and_is_written() {
    let h = placing_server("host_taint_refused", Kept::Files).await;
    let me = job_agent(&h, &cpu_hello("burst-1")).await;
    let ended = |detail: &str| {
        let result = json!({
            "run_id": "run-1-1",
            "lease_id": "1-1",
            "outcome": "failed_to_start",
            "detail": detail,
        });
        json!({ "results": [result] })
    };

    let other = beat_with(&h, &me, ended("quota-unavailable")).await;
    assert_eq!(other["taint_ack"], false);
    assert_eq!(entry(&hosts(&h).await, "burst-1").get("taint"), None);
    let clean = fs::read_to_string(agent_file(&h)).unwrap_or_default();
    assert!(!clean.contains("taint_reason"), "{clean}");

    let reply = beat_with(&h, &me, ended("host-tainted")).await;
    assert_eq!(reply["taint_ack"], false);
    let shown = entry(&hosts(&h).await, "burst-1");
    assert_eq!(
        shown["taint"],
        json!({ "reason": "host-tainted", "run_id": "run-1-1" })
    );
    let text = fs::read_to_string(agent_file(&h)).unwrap();
    assert!(text.contains("taint_reason = \"host-tainted\""), "{text}");
}

/// Asserts the clear end to end, with the bindings in files and in the store: a tainted host
/// that reports its taint cleared is acknowledged, `GET /v1/hosts` shows no taint, the
/// bindings hold none, a job is refused while it is tainted and starts on it after the clear (the only host),
/// and after a restart it is not put back tainted (the clear survived). A cleared report of
/// a taint other than the held one changes nothing. It catches a server that forgets the
/// taint in memory only (a restart would bring it back), one that clears a taint the report
/// does not name, and one that never lets a host out of a taint.
#[tokio::test]
async fn a_taint_the_agent_reports_cleared_ends_and_stays_ended() {
    for kept in Kept::BOTH {
        a_taint_reported_cleared_ends_in(kept).await;
    }
}

async fn a_taint_reported_cleared_ends_in(kept: Kept) {
    let first = placing_server("host_taint_cleared", kept).await;
    let me = job_agent(&first, &cpu_hello("burst-1")).await;
    let held = json!({ "reason": "residue", "run_id": "run-1-1", "check": "daemonised-child" });
    let reply = beat_with(&first, &me, json!({ "taint": held })).await;
    assert_eq!(reply["taint_ack"], true);
    let refused = submit_body(&first, &JobBody::new("before-clear").json()).await;
    assert_eq!(refused.status, 422, "{}", refused.body);

    let other = json!({ "reason": "residue", "run_id": "run-1-2", "check": "daemonised-child" });
    let reply = beat_with(&first, &me, json!({ "taint_cleared": other })).await;
    assert_eq!(reply["taint_cleared_ack"], true);
    assert_eq!(reply["taint_ack"], false);
    assert_eq!(entry(&hosts(&first).await, "burst-1")["taint"], held);

    let reply = beat_with(&first, &me, json!({ "taint_cleared": held })).await;
    assert_eq!(reply["taint_cleared_ack"], true);
    assert_eq!(entry(&hosts(&first).await, "burst-1").get("taint"), None);
    let text = kept_bindings(&first);
    assert_eq!(text.matches("taint_reason").count(), 0, "{text}");
    submit_job(&first, "after-clear").await;
    first.ticks.step().await;
    let reply = beat_with(&first, &me, json!({})).await;
    let (starts, _) = commands(&reply);
    let jobs: Vec<&str> = starts.iter().map(|(job, _)| job.as_str()).collect();
    assert_eq!(jobs, ["after-clear"], "{reply}");

    let h = restarted_placing(first).await;
    let text = kept_bindings(&h);
    assert_eq!(text.matches("taint_reason").count(), 0, "{text}");
    let listing = hosts(&h).await;
    assert!(
        listing["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|host| host.get("taint").is_none()),
        "{listing}"
    );
}

/// Asserts that a clear that cannot be written is not acknowledged: the beat's reply has no
/// `taint_cleared_ack` (the agent reports the clear again), and once the file can be written
/// the next report is acknowledged and the file holds no taint. It catches an agent told it
/// is cleared by a server that a restart would taint again.
#[tokio::test]
async fn a_clear_is_not_acknowledged_before_it_can_be_written() {
    let h = placing_server("host_taint_clear_write", Kept::Files).await;
    let me = job_agent(&h, &cpu_hello("burst-1")).await;
    let held = json!({ "reason": "residue" });
    beat_with(&h, &me, json!({ "taint": held })).await;
    let blocker = h.scratch.path("leases.toml.agents.tmp");
    fs::create_dir(&blocker).unwrap();

    let reply = beat_with(&h, &me, json!({ "taint_cleared": held })).await;
    assert_eq!(reply.get("taint_cleared_ack"), None, "{reply}");
    assert_eq!(reply["taint_ack"], false);

    fs::remove_dir(&blocker).unwrap();
    h.ticks.step().await;
    let reply = beat_with(&h, &me, json!({ "taint_cleared": held })).await;
    assert_eq!(reply["taint_cleared_ack"], true);
    let text = fs::read_to_string(agent_file(&h)).unwrap();
    assert!(!text.contains("taint_reason"), "{text}");
}

/// Asserts the operator's clear end to end, with the bindings in files and in the store: a host
/// whose agent reported a taint takes no job (422 `no-capable-host`); `ClearHostTaint` answers
/// `cleared` with the reason, `GET /v1/hosts` shows no taint, the bindings hold none, a job
/// submitted after it starts on the host (the only one), and a second call answers `cleared`
/// false; and after a restart the host is not put back tainted (the clear survived). It
/// catches a clear that forgets the taint in memory only (a restart would bring it back), a
/// clear that leaves placement refusing the host, and a repeat that claims a clear.
#[tokio::test]
async fn a_taint_the_operator_clears_ends_and_stays_ended() {
    for kept in Kept::BOTH {
        a_taint_cleared_by_the_operator_ends_in(kept).await;
    }
}

async fn a_taint_cleared_by_the_operator_ends_in(kept: Kept) {
    let first = placing_server("host_taint_operator", kept).await;
    let me = job_agent(&first, &cpu_hello("burst-1")).await;
    let held = json!({ "reason": "residue", "run_id": "run-1-1", "check": "daemonised-child" });
    let reply = beat_with(&first, &me, json!({ "taint": held })).await;
    assert_eq!(reply["taint_ack"], true);
    let refused = submit_body(&first, &JobBody::new("before-clear").json()).await;
    assert_eq!(refused.status, 422, "{}", refused.body);

    let cleared = clear_taint(&first, "burst-1").await;
    assert_eq!(cleared.status, 200, "{}", cleared.body);
    assert_eq!(cleared.json()["cleared"], true);
    assert_eq!(cleared.json()["clearedReason"], "residue");
    assert_eq!(cleared.json()["host"]["taintReason"], Value::Null);
    assert_eq!(entry(&hosts(&first).await, "burst-1").get("taint"), None);
    let text = kept_bindings(&first);
    assert_eq!(text.matches("taint_reason").count(), 0, "{text}");
    submit_job(&first, "after-clear").await;
    first.ticks.step().await;
    let reply = beat_with(&first, &me, json!({})).await;
    let (starts, _) = commands(&reply);
    let jobs: Vec<&str> = starts.iter().map(|(job, _)| job.as_str()).collect();
    assert_eq!(jobs, ["after-clear"], "{reply}");

    let again = clear_taint(&first, "burst-1").await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.json()["cleared"], Value::Null);
    assert_eq!(again.json()["clearedReason"], Value::Null);

    let h = restarted_placing(first).await;
    let listing = hosts(&h).await;
    assert!(
        listing["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|host| host.get("taint").is_none()),
        "{listing}"
    );
}
