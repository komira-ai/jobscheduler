//! The operator's routes over a host's orphans: the listing with each orphan's age, the end
//! marked in the host's row and told to the host by its next beat, and each refusal.

use std::sync::Arc;
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use js_agent_wire::{HeldState, StopReason};
use js_types::HostId;
use serde_json::{Value, json};

use super::{Ended, end, list, mark};
use crate::agent_api::job_rig::{FakeAgent, Rig, rig, stop_reasons};

fn host() -> HostId {
    HostId::new("host-a").unwrap()
}

/// A rig whose host `host-a` listed `ghost`, which the store holds no record of, beside its
/// agent: the host's row holds it as an orphan.
async fn with_an_orphan(name: &str) -> (Rig, FakeAgent) {
    let mut rig = rig(name, true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    agent.hold("ghost", "00", HeldState::Running);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply.unknown_jobs, ["ghost"]);
    agent.take(&reply);
    (rig, agent)
}

async fn body(reply: Response) -> (StatusCode, Value) {
    let status = reply.status();
    let bytes = to_bytes(reply.into_body(), 1 << 16).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn listed(rig: &Rig, host: &str) -> Result<(StatusCode, Value), (StatusCode, &'static str)> {
    let path = Path(host.to_owned());
    match list(State(rig.agents.clone()), path).await {
        Ok(reply) => Ok(body(reply).await),
        Err(error) => Err((error.status, error.code)),
    }
}

async fn ended(
    rig: &Rig,
    host: &str,
    job: &str,
) -> Result<(StatusCode, Value), (StatusCode, &'static str)> {
    let path = Path((host.to_owned(), job.to_owned()));
    match end(State(rig.agents.clone()), path).await {
        Ok(reply) => Ok(body(reply).await),
        Err(error) => Err((error.status, error.code)),
    }
}

/// Asserts the operator's path end to end: the listing shows the orphan with how long the
/// host has listed it, by the store's clock, and no end asked; an end is answered 202 and
/// marked, the listing then shows it, the next beat of the host tells it to stop the job with
/// the reason `operator`, and asking again is answered 202 and writes nothing. It catches an
/// end that does not reach the host, an age read from the wrong clock, and a second ask that
/// writes the row again.
#[tokio::test]
async fn an_operator_sees_an_orphan_and_ends_it_on_the_hosts_next_beat() {
    let (rig, agent) = with_an_orphan("orphan_end").await;
    rig.advance(Duration::from_secs(90));
    let (status, shown) = listed(&rig, "host-a").await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(shown["host"], "host-a");
    assert_eq!(shown["orphans"][0]["job_id"], "ghost");
    assert_eq!(shown["orphans"][0]["end_asked"], false);
    let age = shown["orphans"][0]["listed_for_secs"].as_u64().unwrap();
    assert!((90..100).contains(&age), "{shown}");

    let (status, asked) = ended(&rig, "host-a", "ghost").await.unwrap();
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        asked,
        json!({ "host": "host-a", "job_id": "ghost", "end": "asked" })
    );
    let (_, shown) = listed(&rig, "host-a").await.unwrap();
    assert_eq!(shown["orphans"][0]["end_asked"], true);
    let commits = rig.commits();
    let (status, _) = ended(&rig, "host-a", "ghost").await.unwrap();
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(rig.commits(), commits, "a second ask wrote the row");

    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(
        stop_reasons(&reply),
        [("ghost".to_owned(), Some(StopReason::Operator))]
    );
}

/// Asserts each refusal of the routes: a host id that is no host id and a host the store holds
/// no row of are `404 unknown-host`, a job that is no orphan of the host `404 unknown-orphan`,
/// and a store that cannot be read `503 state-write-failed`, each writing nothing. It catches
/// a refusal answered as an end, and a store failure answered as an empty listing.
#[tokio::test]
async fn each_refusal_of_the_orphan_routes_has_its_own_code() {
    let (rig, _agent) = with_an_orphan("orphan_refused").await;
    let unknown_host = (StatusCode::NOT_FOUND, "unknown-host");
    let commits = rig.commits();
    assert_eq!(listed(&rig, "no host").await, Err(unknown_host));
    assert_eq!(listed(&rig, "host-z").await, Err(unknown_host));
    assert_eq!(ended(&rig, "no host", "ghost").await, Err(unknown_host));
    assert_eq!(ended(&rig, "host-z", "ghost").await, Err(unknown_host));
    assert_eq!(
        ended(&rig, "host-a", "job-1").await,
        Err((StatusCode::NOT_FOUND, "unknown-orphan"))
    );
    let failed = (StatusCode::SERVICE_UNAVAILABLE, "state-write-failed");
    rig.store.fail_read_after(0);
    assert_eq!(listed(&rig, "host-a").await, Err(failed));
    rig.store.fail_read_after(0);
    assert_eq!(ended(&rig, "host-a", "ghost").await, Err(failed));
    assert_eq!(rig.commits(), commits);
    let (_, shown) = listed(&rig, "host-a").await.unwrap();
    assert_eq!(shown["orphans"][0]["end_asked"], false);
}

/// Asserts that an end whose write a beat of the host overtook (the row moved between the read
/// and the write) is `409 row-moved` and marks nothing, and that marking the row as read
/// then is refused the same way; asking again marks it. It catches an end written over a newer
/// row, which would undo what that beat wrote.
#[tokio::test]
async fn an_end_a_beat_overtook_is_a_conflict_and_marks_nothing() {
    let (rig, _agent) = with_an_orphan("orphan_moved").await;
    let jobs = Arc::clone(&rig.jobs);
    let stale = jobs.read_node(&host()).unwrap().unwrap();
    let rewritten = stale.record.state.clone();
    let writer = Arc::clone(&jobs);
    let read = stale.clone();
    rig.store.before_next(move || {
        writer.settle_node(&read, rewritten).unwrap().unwrap();
    });
    assert_eq!(
        ended(&rig, "host-a", "ghost").await,
        Err((StatusCode::CONFLICT, "row-moved"))
    );
    assert_eq!(
        mark(&jobs, &stale, "ghost").unwrap(),
        Ended::Moved,
        "a row read before the beat"
    );
    let row = jobs.read_node(&host()).unwrap().unwrap();
    assert!(!row.record.state.orphans["ghost"].end_asked);
    let (status, _) = ended(&rig, "host-a", "ghost").await.unwrap();
    assert_eq!(status, StatusCode::ACCEPTED);
}

/// Asserts that a server without the job service answers both routes `404 unknown-host`: it
/// holds no host's row, so no orphan. It catches a route that answers such a server as one
/// with no orphans.
#[tokio::test]
async fn a_server_without_the_job_service_has_no_orphans_to_show() {
    let rig = rig("orphans_no_jobs", false);
    let unknown_host = (StatusCode::NOT_FOUND, "unknown-host");
    assert_eq!(listed(&rig, "host-a").await, Err(unknown_host));
    assert_eq!(ended(&rig, "host-a", "ghost").await, Err(unknown_host));
}
