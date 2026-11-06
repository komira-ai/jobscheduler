//! The cancel call: `CancelJob` (design section 5.3, "Cancel"). The group needs nothing but
//! the job tables, so its context ([`Ctx`], set through [`JobApi::with_cancel`]) holds
//! nothing, and the call is served whatever the context.
//!
//! The cancel is one call of the tables, [`js_jobs::JobStore::request_cancel`], in one
//! transaction, and the answer is the job as that call leaves it:
//!
//! | the job | what is committed | the answer |
//! |---|---|---|
//! | queued | cancelled at once, with a result of reason cancelled and its retention end: it has no allocation | the job, cancelled |
//! | placed, starting or running | the cancel intent (`cancel_requested`), and nothing else | the job, in its state, with the intent |
//! | ended | nothing | the job, unchanged |
//! | its cancel already asked for | nothing: a second cancel is the first | the job as the first left it |
//! | no row (never submitted, or swept) | nothing | `NOT_FOUND`, `unknown-job` (404) |
//!
//! A placed job with the intent is never sent its start, and the stop rides on its host's
//! next heartbeat reply until the host stops listing the job: both are the per-job protocol's
//! rule over the committed row (`per_job::stops_for` and the start filter), so nothing here
//! touches the start path. The call answers after the commit; a store that fails is
//! `INTERNAL` with the message `state-write-failed` (500 `state-write-failed`) and the job is
//! as it was. A `job_id` that is not a job id is `INVALID_ARGUMENT`, `field: jobId` (400).

use js_jobs::{JobId, JobStoreError};
use js_proto::v1::{CancelJobRequest, CancelJobResponse};
use tonic::{Request, Response, Status};

use super::JobApi;
use super::read::view;
use super::submit::not_written;

/// The state of the cancel group. The call needs none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ctx {
    /// A value only tests set, so that a test can tell a context it gave from the default.
    #[cfg(test)]
    pub(super) mark: u32,
}

impl JobApi {
    /// The service with `cancel` as the cancel group's context; every other part is kept.
    #[must_use]
    pub fn with_cancel(self, cancel: Ctx) -> JobApi {
        JobApi { cancel, ..self }
    }
}

/// `CancelJob`: the cancel committed, then the job as it now stands.
pub(super) async fn cancel_job(
    api: &JobApi,
    request: Request<CancelJobRequest>,
) -> Result<Response<CancelJobResponse>, Status> {
    let id = JobId::parse(&request.into_inner().job_id)
        .map_err(|_| Status::invalid_argument("field: jobId"))?;
    let job = api.jobs.request_cancel(&id).map_err(|error| match error {
        JobStoreError::NotFound { .. } => Status::not_found("unknown-job"),
        error => not_written(api)(error),
    })?;
    Ok(Response::new(CancelJobResponse {
        job: Some(view::job(&job)),
    }))
}

#[cfg(test)]
#[path = "cancel_tests.rs"]
mod tests;
