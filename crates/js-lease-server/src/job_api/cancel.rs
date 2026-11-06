//! The cancel call: `CancelJob` (design section 5.3, "Cancel"). The group needs nothing but
//! the job tables, so its context ([`Ctx`], set through [`JobApi::with_cancel`]) holds
//! nothing, and the call is served whatever the context.
//!
//! The job is read first, and a caller of the role `client` may cancel only its own jobs: a
//! job of another client is answered as one with no row (`unknown-job`, 404), and nothing is
//! committed ([`super::roles`]). A job read as ended is answered as read: a cancel commits
//! nothing for it, and so a job swept and submitted again under its id after the read is
//! never cancelled for the wrong client. Any other job is cancelled by one call of the tables,
//! [`js_jobs::JobStore::request_cancel`], in one transaction (a job that has not ended is
//! never swept, so it is still the job read), and the answer is the job as that call leaves
//! it:
//!
//! | the job | what is committed | the answer |
//! |---|---|---|
//! | queued | cancelled at once, with a result of reason cancelled and its retention end: it has no allocation | the job, cancelled, with `cancel_requested` false: it has ended |
//! | placed, starting or running | the cancel intent (`cancel_requested`), and nothing else | the job, in its state, with the intent |
//! | ended | nothing | the job, unchanged |
//! | its cancel already asked for | nothing: a second cancel is the first | the job as the first left it |
//! | no row (never submitted, or swept) | nothing | `NOT_FOUND`, `unknown-job` (404) |
//!
//! A placed job with the intent is never sent its start, and the stop rides on its host's
//! next heartbeat reply until the host stops listing the job: both are the per-job protocol's
//! rule over the committed row (`per_job::stops_for` and the start filter), so nothing here
//! touches the start path. The call answers after the commit; a store that fails (the read
//! or the commit) is `INTERNAL` with the message `state-write-failed` (500
//! `state-write-failed`) and the job is as it was. So is a row that is gone at the commit
//! although the read found it, which only a store that lost the row can do: a job that has
//! not ended is never swept. A `job_id` that is not a job id is `INVALID_ARGUMENT`, `field: jobId` (400).

use js_jobs::JobId;
use js_proto::v1::{CancelJobRequest, CancelJobResponse};
use tonic::{Request, Response, Status};

use super::JobApi;
use super::read::view;
use super::roles::{Caller, hidden};
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

/// The answer to a job id with no row.
fn unknown_job() -> Status {
    Status::not_found("unknown-job")
}

/// `CancelJob`: the job read, `caller`'s right to it checked, the cancel committed, then the
/// job as it now stands.
pub(super) async fn cancel_job(
    api: &JobApi,
    caller: &Caller,
    request: Request<CancelJobRequest>,
) -> Result<Response<CancelJobResponse>, Status> {
    let id = JobId::parse(&request.into_inner().job_id)
        .map_err(|_| Status::invalid_argument("field: jobId"))?;
    let read = api.jobs.get_job(&id).map_err(not_written(api))?;
    let read = read.ok_or_else(unknown_job)?;
    if !caller.owns(&read) {
        return Err(hidden());
    }
    let job = match read.state.is_ended() {
        true => read,
        false => api.jobs.request_cancel(&id).map_err(not_written(api))?,
    };
    Ok(Response::new(CancelJobResponse {
        job: Some(view::job(&job)),
    }))
}

#[cfg(test)]
#[path = "cancel_tests.rs"]
mod tests;
