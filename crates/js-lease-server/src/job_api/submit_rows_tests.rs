//! A submit to a server that did not take a host's hello: admission and the pass over the
//! queue read the host from its `node/` row. Both servers read one manual clock and one store.

use std::sync::Arc;

use js_jobs::JobState;
use js_proto::v1::{self, SubmitJobRequest};
use tonic::{Code, Request};

use super::{Ctx, submit_job};
use crate::admit::{Admit, JobAdmission};
use crate::agent_api::job_rig::{FakeAgent, Rig, clocked_rig, starts};
use crate::agent_api::peer_rig::NEVER;
use crate::agent_api::peer_rig::peer_of;
use crate::config::Role;
use crate::job_api::JobApi;
use crate::job_api::roles::Caller;
use crate::job_api::writes_rig::{CEILING, id, wire};

/// The job service over `rig`'s tables, admitting over its registry's hosts and the rows, its
/// submit poking `rig`'s placement poller.
fn api(rig: &Rig) -> JobApi {
    let admission = JobAdmission::new(
        Admit::default(),
        rig.agents.registry.clone(),
        Arc::clone(&rig.jobs),
    );
    let ctx = Ctx::served(CEILING, Arc::new(admission));
    let ctx = ctx.poking(Arc::clone(&rig.agents));
    JobApi::new(Arc::clone(&rig.jobs), rig.shared.clone()).with_submit(ctx)
}

/// The answer to submitting `spec` to `api` as an operator.
async fn submit(api: &JobApi, spec: v1::JobSpec) -> Result<v1::Job, tonic::Status> {
    let caller = Caller {
        name: js_jobs::ClientName::default_client(),
        role: Role::Operator,
    };
    let request = Request::new(SubmitJobRequest { spec: Some(spec) });
    let answer = submit_job(api, &caller, request).await?;
    Ok(answer.into_inner().job.unwrap())
}

/// Asserts the regression: a host says hello to server A, which writes its row; a job
/// submitted to server B, whose registry holds no host, is admitted (it is refused
/// `no-capable-host` before the hello, with nothing recorded) and answered `queued`, and the
/// pass of B's placement poller, which the submit poked, places it on A's host. Catches the `no-capable-host` refusal of a submit that reaches an
/// instance before its hosts said hello there, and a pass that offers only the hosts of its
/// own registry.
#[tokio::test]
async fn a_submit_to_a_server_that_took_no_hello_is_admitted_and_placed_on_the_host() {
    let mut first = clocked_rig("rows_submit_a", true, NEVER);
    let second = peer_of("rows_submit_b", &first);
    let api = api(&second);

    let refused = submit(&api, wire("job-r")).await.unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert!(second.jobs.get_job(&id("job-r")).unwrap().is_none());

    first.join("host-a", false).await;
    assert!(second.agents.registry.hosts().is_empty());
    let made = submit(&api, wire("job-r")).await.unwrap();
    assert_eq!(
        (made.state(), made.host_id.as_str()),
        (v1::JobState::Queued, "")
    );
    crate::poller::pass(&second.agents).await;
    assert_eq!(second.job(&id("job-r")).state, JobState::Placed);
}

/// Two job-service replicas over one store: replica B took the hello of `host-a`, so B holds
/// its heartbeats, and replica A holds none; the job of the tests below is submitted to A.
async fn two_replicas(name: &str) -> (Rig, Rig) {
    let mut b = clocked_rig(&format!("{name}_b"), true, NEVER);
    b.join("host-a", false).await;
    let a = peer_of(&format!("{name}_a"), &b);
    (a, b)
}

/// Asserts that a submit to replica A pokes A's own placement poller and no other: B, which
/// holds the host, has no poke waiting. A's poller then places the job on B's host from the
/// host's `node/` row, and B's next heartbeat of that host carries the start, built from the
/// job table both replicas share. It catches a poke sent to another replica, and a placement
/// that needs the poked replica to hold the host.
#[tokio::test]
async fn a_submit_to_one_replica_pokes_its_own_poller_and_the_replica_holding_the_host_starts_it() {
    let (a, b) = two_replicas("rows_replica_pokes").await;
    let made = submit(&api(&a), wire("job-r")).await.unwrap();
    assert_eq!(made.state(), v1::JobState::Queued);
    assert!(!b.agents.poller.take_poke());
    assert!(crate::poller::poked_pass(&a.agents).await);
    assert_eq!(b.job(&id("job-r")).state, JobState::Placed);
    let reply = b.beat(&FakeAgent::new("host-a")).await.unwrap();
    let started: Vec<String> = starts(&reply).into_iter().map(|s| s.job_id).collect();
    assert_eq!(started, ["job-r"]);
}

/// Asserts that a job submitted to replica A whose poller never runs (A stopped right after the
/// commit) is not lost: replica B, never poked, finds it queued on its own pass, its tick's, and
/// places it. It catches a job left queued for good when the replica that took it does not
/// place it.
#[tokio::test]
async fn a_job_whose_replica_never_places_it_is_placed_by_the_other_replicas_own_pass() {
    let (a, b) = two_replicas("rows_replica_tick").await;
    submit(&api(&a), wire("job-t")).await.unwrap();
    drop(a);
    assert!(!b.agents.poller.take_poke());
    assert_eq!(b.job(&id("job-t")).state, JobState::Queued);
    b.tick();
    assert_eq!(b.job(&id("job-t")).state, JobState::Placed);
}
