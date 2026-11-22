//! A submit to a server that did not take a host's hello: admission and the pass over the
//! queue read the host from its `node/` row. Both servers read one manual clock and one store.

use std::sync::Arc;

use js_jobs::JobState;
use js_proto::v1::{self, SubmitJobRequest};
use tonic::{Code, Request};

use super::{Ctx, submit_job};
use crate::admit::{Admit, JobAdmission};
use crate::agent_api::job_rig::{Rig, clocked_rig};
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
