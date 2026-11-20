//! Step 5 of `SubmitJob` (design komira-ai/jobscheduler#903): a job with nothing ahead of it is
//! placed in the call, and its start rides on the host's held heartbeat. The tests wait on the
//! server's own progress (each wait of a held reply is counted), never on time.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::{Isolation, JobSpec, JobState, NewJob};
use js_proto::v1::{self, SubmitJobRequest};
use tonic::Request;

use super::{Ctx, submit_job};
use crate::admit::{Admit, JobAdmission};
use crate::agent_api::job_rig::{FakeAgent, Rig, clocked_rig, spec, starts};
use crate::config::Role;
use crate::job_api::JobApi;
use crate::job_api::roles::Caller;
use crate::job_api::writes_rig::{CEILING, NEVER, id, wire};

/// A bound no test reaches: a hold a test does not end returns at it.
const LONG: Duration = Duration::from_secs(60);

/// The job service over `rig`'s tables, admitting over its registry's hosts, its submit placing
/// with `rig`'s pass when `placing`.
fn api(rig: &Rig, placing: bool) -> JobApi {
    let admission = JobAdmission::new(
        Admit::default(),
        rig.agents.registry.clone(),
        Arc::clone(&rig.jobs),
    );
    let ctx = Ctx::served(CEILING, Arc::new(admission));
    let ctx = match placing {
        true => ctx.placing(Arc::clone(&rig.agents)),
        false => ctx,
    };
    JobApi::new(Arc::clone(&rig.jobs), rig.shared.clone()).with_submit(ctx)
}

/// The answer to submitting `spec` to `api` as an operator.
async fn submit(api: &JobApi, spec: v1::JobSpec) -> v1::Job {
    let caller = Caller {
        name: js_jobs::ClientName::default_client(),
        role: Role::Operator,
    };
    let request = Request::new(SubmitJobRequest { spec: Some(spec) });
    let answer = submit_job(api, &caller, request).await.unwrap();
    answer.into_inner().job.unwrap()
}

/// A placing server with `host-a` registered, whose agent reports `per-job` alone.
fn placing_rig(name: &str) -> Rig {
    let mut rig = clocked_rig(name, true, NEVER);
    rig.register("host-a", true);
    rig
}

/// Records the queued job `id` at `priority` that no host of the rigs here can take: it runs on
/// the host itself, and no host reported `host-jobs`.
fn stuck(rig: &Rig, id: &str, priority: i32) {
    let mut spec: JobSpec = spec();
    spec.constraints.isolation = Isolation::Host;
    let new = NewJob {
        id: Some(crate::job_api::writes_rig::id(id)),
        spec,
        priority,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap();
}

/// Asserts the acceptance of the design: with an idle host whose heartbeat is held, a submit
/// into an empty queue is answered `placed` on that host, and the held reply carries its start,
/// with no tick and no second heartbeat. It catches a submit that leaves the
/// job to the tick, and a placement in the call that does not wake the held heartbeat.
#[tokio::test]
async fn a_submit_into_an_empty_queue_starts_on_the_held_heartbeat() {
    let mut rig = clocked_rig("submit_held", true, NEVER).holding_for(LONG);
    rig.register_held("host-a");
    let agent = FakeAgent::new("host-a");
    let api = api(&rig, true);
    let submitted = async {
        let mut waits = rig.agents.wakes.waits();
        waits.wait_for(|waits| *waits >= 1).await.unwrap();
        submit(&api, wire("job-s")).await
    };
    let (reply, job) = tokio::join!(rig.beat(&agent), submitted);
    assert_eq!(
        (job.state(), job.host_id.as_str()),
        (v1::JobState::Placed, "host-a")
    );
    let reply = reply.unwrap();
    let started: Vec<String> = starts(&reply).into_iter().map(|s| s.job_id).collect();
    assert_eq!(started, ["job-s"]);
}

/// Asserts that a job submitted behind a queued job of its own priority, or of a higher one, is
/// answered `queued` and left queued (no pass ran in the call, though the host has room), while
/// one of a higher priority than every queued job is placed in its call. It catches a submit
/// that jumps the queue past a job it should wait behind, and one that counts lower priorities
/// as ahead.
#[tokio::test]
async fn only_a_job_with_nothing_of_its_priority_ahead_is_placed_in_the_call() {
    let rig = placing_rig("submit_ahead");
    stuck(&rig, "job-0", 0);
    let api = api(&rig, true);
    let behind = submit(&api, wire("job-b")).await;
    assert_eq!(behind.state(), v1::JobState::Queued);
    assert_eq!(rig.job(&id("job-b")).state, JobState::Queued);
    let first = submit(
        &api,
        v1::JobSpec {
            priority: 1,
            ..wire("job-p")
        },
    )
    .await;
    assert_eq!(
        (first.state(), first.host_id.as_str()),
        (v1::JobState::Placed, "host-a")
    );
    stuck(&rig, "job-5", 5);
    let lower = submit(
        &api,
        v1::JobSpec {
            priority: 2,
            ..wire("job-l")
        },
    )
    .await;
    assert_eq!(lower.state(), v1::JobState::Queued);
    assert_eq!(rig.job(&id("job-l")).state, JobState::Queued);
}

/// Asserts that a retry of a job already queued runs no pass and is answered as the job stands
/// (`queued`), and that a service that does not place answers a new job `queued` too. It
/// catches a retry that places (and so answers otherwise than the first answer said), and a
/// service built without the pass that still runs one.
#[tokio::test]
async fn a_retry_and_a_service_that_does_not_place_leave_the_job_queued() {
    let rig = placing_rig("submit_retry_place");
    let first = submit(&api(&rig, false), wire("job-r")).await;
    assert_eq!(first.state(), v1::JobState::Queued);
    let retried = submit(&api(&rig, true), wire("job-r")).await;
    assert_eq!(retried.state(), v1::JobState::Queued);
    assert_eq!(rig.job(&id("job-r")).state, JobState::Queued);
    assert_eq!(rig.jobs.live_allocations().unwrap(), Vec::new());
}
