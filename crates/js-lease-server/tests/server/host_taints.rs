//! The taint a host reports about itself: acknowledged once durable, shown in the host
//! listing, kept across a restart, and a reason for the scheduler to start no job on the
//! host. Jobs go through the job service; the agent speaks the per-job messages.

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, entry, hosts, post};
use crate::job_hosts::{cpu_hello, placing_server, restarted_placing};
use crate::jobs_support::{JobBody, commands, job_agent, job_of, submit_body};
use crate::support::{Harness, Kept, kept_bindings};

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
