//! `CancelJob` (`cancel.rs`) over both transports, with jobs submitted through the service and
//! moved by the agent rig's server: its placement pass, and a fake agent that answers the
//! heartbeat replies the server builds from the job table. One manual clock; nothing sleeps.

use axum::http::StatusCode;
use js_agent_wire::Outcome;
use js_jobs::{AllocationStep, JobState};
use js_proto::v1;
use js_types::HostId;
use serde_json::json;
use tonic::Code;

use crate::agent_api::job_rig::{BOOKED, FakeAgent, clocked_rig, starts, stops};
use crate::job_api::writes_rig::{Fixed, NEVER, Via, id, over_fake, served, wire};

/// Asserts that once a cancel intent is committed for a placed job, no heartbeat reply
/// carries its start. Cancelled before the agent heard of it: the intent is in the job's row,
/// the next reply carries the stop and no start, the agent retires the id it never held, and
/// that answer ends the job cancelled with its allocation freed; no later reply carries
/// either, and the agent never runs it (row 38 of the design's failure list). Cancelled after
/// the agent took its start: the stop is in every reply until the agent stops listing the
/// job, and never a start. It catches a start resent after the commit and a cancel that
/// changes only memory.
#[tokio::test]
async fn no_start_is_sent_after_a_cancel_intent() {
    let mut rig = clocked_rig("cancel_no_start", true, NEVER);
    rig.register("host-a", true);
    let writes = served(&rig).await;
    let mut agent = FakeAgent::new("host-a");

    writes.submit(Via::Grpc, &wire("job-early")).await.unwrap();
    rig.tick();
    let asked = writes.cancel(Via::Grpc, "job-early").await.unwrap();
    assert_eq!(
        (asked.state(), asked.cancel_requested),
        (v1::JobState::Placed, true)
    );
    assert!(rig.job(&id("job-early")).cancel_requested);
    let reply = rig.beat(&agent).await.unwrap();
    assert!(starts(&reply).is_empty());
    assert_eq!(stops(&reply), ["job-early"]);
    agent.take(&reply);
    let reply = rig.beat(&agent).await.unwrap();
    assert!(starts(&reply).is_empty());
    agent.take(&reply);
    assert_eq!(
        rig.states(&id("job-early")),
        (JobState::Cancelled, AllocationStep::Freed)
    );
    for _ in 0..2 {
        rig.tick();
        let reply = rig.beat(&agent).await.unwrap();
        assert!(starts(&reply).is_empty());
        assert_eq!(stops(&reply), Vec::<String>::new());
        agent.take(&reply);
    }
    assert!(!agent.holds("job-early"));
    assert_eq!(agent.launched, Vec::<String>::new());

    writes.submit(Via::Json, &wire("job-late")).await.unwrap();
    rig.tick();
    let reply = rig.beat(&agent).await.unwrap();
    let started: Vec<String> = starts(&reply)
        .into_iter()
        .map(|start| start.job_id)
        .collect();
    assert_eq!(started, ["job-late"]);
    agent.take(&reply);
    let asked = writes.cancel(Via::Json, "job-late").await.unwrap();
    assert_eq!(
        (asked.state(), asked.cancel_requested),
        (v1::JobState::Placed, true)
    );
    for _ in 0..2 {
        let reply = rig.beat(&agent).await.unwrap();
        assert!(starts(&reply).is_empty());
        assert_eq!(stops(&reply), ["job-late"]);
    }
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.take(&rig.beat(&agent).await.unwrap());
    assert!(!agent.holds("job-late"));
    let reply = rig.beat(&agent).await.unwrap();
    assert!(starts(&reply).is_empty());
    assert_eq!(stops(&reply), Vec::<String>::new());
    assert_eq!(rig.job(&id("job-late")).state, JobState::Cancelled);
}

/// Asserts that a queued job is cancelled in the same call, over each transport, with no
/// allocation, answered without `cancel_requested` (it has ended), and that the placement
/// passes that follow do not place it. It catches a queued job left for a later pass to
/// place, and a cancelled job answered with the cancel still requested.
#[tokio::test]
async fn a_cancel_of_a_queued_job_is_immediate() {
    let mut rig = clocked_rig("cancel_queued", true, NEVER);
    rig.register("host-a", true);
    let writes = served(&rig).await;
    for (via, named) in [(Via::Grpc, "job-g"), (Via::Json, "job-j")] {
        writes.submit(via, &wire(named)).await.unwrap();
        let cancelled = writes.cancel(via, named).await.unwrap();
        assert_eq!(
            (cancelled.state(), cancelled.cancel_requested),
            (v1::JobState::Cancelled, false),
            "{via:?}"
        );
        assert_eq!(rig.jobs.get_allocation(&id(named)).unwrap(), None);
    }
    rig.tick();
    rig.tick();
    assert_eq!(rig.jobs.live_allocations().unwrap(), Vec::new());
    assert_eq!(rig.job(&id("job-g")).state, JobState::Cancelled);
}

/// Asserts that a second cancel answers what the first did and commits nothing, for a queued
/// job and for a placed one, by the store's count of commits. It catches a second cancel that
/// writes again (moving its time) or answers otherwise.
#[tokio::test]
async fn cancel_twice_is_cancel_once() {
    let mut rig = clocked_rig("cancel_twice", true, NEVER);
    rig.register("host-a", true);
    let writes = served(&rig).await;
    writes.submit(Via::Grpc, &wire("job-p")).await.unwrap();
    rig.tick();
    writes.submit(Via::Grpc, &wire("job-q")).await.unwrap();
    for named in ["job-p", "job-q"] {
        let before = rig.commits();
        let first = writes.cancel(Via::Grpc, named).await.unwrap();
        assert_eq!(rig.commits(), before + 1, "{named}");
        let second = writes.cancel(Via::Json, named).await.unwrap();
        assert_eq!(second, first, "{named}");
        assert_eq!(rig.commits(), before + 1, "{named}");
    }
}

/// Asserts that cancelling a job that has ended answers it in its ended state, without the
/// intent, and changes nothing: its record and result are as they were, and nothing is
/// committed. It catches a cancel that rewrites an ended job.
#[tokio::test]
async fn cancel_of_an_ended_job_changes_nothing() {
    let mut rig = clocked_rig("cancel_ended", true, NEVER);
    rig.register("host-a", true);
    let writes = served(&rig).await;
    let mut agent = FakeAgent::new("host-a");
    writes.submit(Via::Grpc, &wire("job-e")).await.unwrap();
    rig.tick();
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.run("job-e");
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.end("job-e", Outcome::Failed, Some(3));
    agent.take(&rig.beat(&agent).await.unwrap());
    let before = rig.job(&id("job-e"));
    assert_eq!(before.state, JobState::Failed);
    let commits = rig.commits();
    for via in Via::BOTH {
        let answer = writes.cancel(via, "job-e").await.unwrap();
        assert_eq!(
            (answer.state(), answer.cancel_requested),
            (v1::JobState::Failed, false),
            "{via:?}"
        );
    }
    assert_eq!(rig.job(&id("job-e")), before);
    assert_eq!(rig.commits(), commits);
}

/// Asserts that a cancel of an id no job has is `NOT_FOUND`, `unknown-job` (404), and that a
/// `job_id` that is not a job id is `INVALID_ARGUMENT` naming `jobId`. It catches a cancel
/// that records something for an unknown id.
#[tokio::test]
async fn cancel_of_an_unknown_job_is_not_found() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let status = writes.cancel_grpc("job-none").await.unwrap_err();
    assert_eq!(
        (status.code(), status.message()),
        (Code::NotFound, "unknown-job")
    );
    let answer = writes.cancel(Via::Json, "job-none").await.unwrap_err();
    assert_eq!(
        answer,
        (StatusCode::NOT_FOUND, json!({ "error": "unknown-job" }))
    );
    let status = writes.cancel_grpc("").await.unwrap_err();
    assert_eq!(
        (status.code(), status.message()),
        (Code::InvalidArgument, "field: jobId")
    );
    assert_eq!(writes.jobs.get_job(&id("job-none")).unwrap(), None);
}

/// Asserts that a cancel whose commit fails answers `INTERNAL` with `state-write-failed`
/// (500 `state-write-failed`) and leaves the job as it was: a queued job still queued, a placed
/// one still placed without the intent; and that the cancel repeated once the fault has
/// cleared is committed: the queued job ends cancelled (so without `cancel_requested`), the
/// placed one keeps its state with the intent. It catches a cancel that changes the job
/// before its commit.
#[tokio::test]
async fn a_failed_cancel_answers_500_and_changes_nothing() {
    let (store, writes) = over_fake(Fixed::new(true)).await;
    writes.submit(Via::Grpc, &wire("job-q")).await.unwrap();
    writes.submit(Via::Grpc, &wire("job-p")).await.unwrap();
    let host = HostId::new("host-a").unwrap();
    writes.jobs.place(&id("job-p"), host, BOOKED).unwrap();
    for (via, named, state, answered) in [
        (
            Via::Grpc,
            "job-q",
            JobState::Queued,
            (v1::JobState::Cancelled, false),
        ),
        (
            Via::Json,
            "job-p",
            JobState::Placed,
            (v1::JobState::Placed, true),
        ),
    ] {
        let before = writes.jobs.get_job(&id(named)).unwrap().unwrap();
        store.fail_next();
        let answer = writes.cancel(via, named).await.unwrap_err();
        assert_eq!(
            answer,
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "state-write-failed" })
            ),
            "{named}"
        );
        let after = writes.jobs.get_job(&id(named)).unwrap().unwrap();
        assert_eq!(after, before, "{named}");
        assert_eq!((after.state, after.cancel_requested), (state, false));
        let asked = writes.cancel(via, named).await.unwrap();
        assert_eq!((asked.state(), asked.cancel_requested), answered, "{named}");
    }
    store.fail_next();
    let again = writes.cancel_grpc("job-p").await.unwrap();
    assert!(
        again.cancel_requested,
        "an asked cancel is answered without a commit"
    );
}
