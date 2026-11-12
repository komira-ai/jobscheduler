//! The submit call: `SubmitJob` (design section 5.3, steps 1 to 4). The checks of a
//! submitted job are in `validate.rs`. The group's own state is [`Ctx`], set through
//! [`JobApi::with_submit`]: the ceiling on a job's wall limit and where admission asks
//! whether anything could ever serve a job. A context without them ([`Ctx::default`])
//! answers `UNIMPLEMENTED`.
//!
//! # Steps, in this order
//!
//! 1. **Validate** ([`validate::job`]): the job is checked field by field, with no I/O, and
//!    the first fault is the answer: `INVALID_ARGUMENT` naming the field (400
//!    `invalid-argument` with `field`), never repeating a value of the request. A wall limit
//!    that is absent or 0 is the refusal `wall-time-not-positive`, one above the ceiling
//!    (`--max-job-wall`, [`DEFAULT_MAX_JOB_WALL`] unless set) `wall-time-above-ceiling`,
//!    both `INVALID_ARGUMENT` with the `Refusal` detail (400, `field`
//!    `spec.wallLimitSeconds`). Every check runs before the job's id is looked up, so a
//!    retry of a job the ceiling now refuses is refused, not answered from the job.
//! 2. **Idempotency.** For a job with an id, the tables are read first. An existing job with
//!    the same id and the same job (every field but the priority, compared on the decoded
//!    job [`js_jobs::JobSpec`]; for an ended job, whose files hold only their SHA-256, each
//!    submitted file by its digest, with the same names) is the answer, as it is now, in
//!    whatever state, with its own priority and the client that recorded it: the same shape as a new job's (gRPC `OK`, 201
//!    over the gateway), so a retry cannot tell. A different job is the refusal
//!    `job-id-conflict` (`ALREADY_EXISTS`, 409). Job ids are one namespace: an operator's
//!    retry of any client's job is answered with that job, while a caller of the role
//!    `client` is answered `job-id-conflict` for an id another client's job holds, whatever
//!    the job, so it never reads a job it does not own ([`super::roles`]). The check is on
//!    the record the commit returns, which is the stored one when nothing was recorded.
//!    An id is held as long as the job's row is: until the retention sweep deletes it once
//!    the retention window has passed. A job without an id has the store mint one, so a
//!    repeated submit without an id makes a second job.
//! 3. **Admission** (design section 5.6, step 2), only for a job that would be created:
//!    [`Admission::can_ever_serve`] asks whether a host, or a catalog of a provider that
//!    launches, could ever serve the job; the server's source reads the registry's hosts and
//!    the catalogs as the server's catalog cache holds them, calling no provider, and a
//!    catalog the cache has not read may offer. A job with host isolation is offered only the
//!    hosts that reported they run host jobs, and no catalog. A job nothing could serve is the
//!    refusal `no-capable-host` (`FAILED_PRECONDITION`, 422) and nothing is recorded: no row,
//!    no id.
//! 4. **Commit before the answer.** The job is inserted as queued in one transaction of the
//!    store ([`js_jobs::JobStore::insert_queued`]), with the name of the client the bearer
//!    layer named (never a value of the request); only after it returns is the answer
//!    built, from the record it returns. A store that fails is `INTERNAL` with the message
//!    `state-write-failed` (500 `state-write-failed`), the cause logged and never answered,
//!    and nothing happened: the handler holds nothing outside the store, so there is
//!    nothing to roll back. A second writer that recorded a different job under the id
//!    between the lookup and the commit is `job-id-conflict`.
//!
//! The call never waits for placement: after the commit it calls no engine, scheduler or
//! provider. The engine finds the queued row on its own pass.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::{Isolation, JobStoreError, NewJob};
use js_proto::refusal::refusal_status;
use js_proto::v1::{RefusalCode, SubmitJobRequest, SubmitJobResponse};
use js_types::Requirements;
use tonic::{Request, Response, Status};

use super::JobApi;
use super::fleet::Handle;
use super::read::view;
use super::roles::Caller;
use super::validate::{self, Fault};
use crate::fence::STATE_FENCED;

/// The ceiling on a job's wall limit when `--max-job-wall` (or `server.max_job_wall`) is not
/// given: 30 days, the longest `server.max_lease_wall` accepts (it has no default of its own).
pub const DEFAULT_MAX_JOB_WALL: Duration = Duration::from_secs(30 * 24 * 3600);

/// The message of the status a failed store answers.
pub(super) const STATE_WRITE_FAILED: &str = "state-write-failed";

/// Whether anything could ever serve a job (design section 5.6, step 2).
#[tonic::async_trait]
pub(crate) trait Admission: Send + Sync {
    /// `false` only when no host and no catalog could ever serve `wanted`: a busy or silent
    /// host counts, a tainted or departing one does not, and a catalog not yet read may
    /// offer. A job that asks for host `isolation` can only be served by a host that reported
    /// it runs host jobs; a catalog is never offered it, as a machine not launched yet has
    /// reported nothing.
    async fn can_ever_serve(&self, wanted: &Requirements, isolation: Isolation) -> bool;
}

/// The state of the submit group: the ceiling and the admission source, once served.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ctx {
    /// `None`: the call is not served.
    served: Option<Served>,
    /// A value only tests set, so that a test can tell a context it gave from the default.
    #[cfg(test)]
    pub(super) mark: u32,
}

/// What a served submit reads.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Served {
    /// The most a job's wall limit may be.
    ceiling: Duration,
    /// Where admission is asked.
    admission: Handle<dyn Admission>,
}

impl Ctx {
    /// The context of a served submit: jobs up to `ceiling`, admitted by `admission`.
    #[must_use]
    pub(crate) fn served(ceiling: Duration, admission: Arc<dyn Admission>) -> Ctx {
        Ctx {
            #[cfg(test)]
            mark: 0,
            served: Some(Served {
                ceiling,
                admission: Handle(admission),
            }),
        }
    }
}

impl Ctx {
    /// The default context with `mark` set, for a test that tells one context from another.
    #[cfg(test)]
    pub(super) fn marked(mark: u32) -> Ctx {
        Ctx { mark, served: None }
    }

    /// The ceiling of a served context; `None` when the call is not served.
    #[cfg(test)]
    pub(super) fn ceiling(&self) -> Option<Duration> {
        self.served.as_ref().map(|served| served.ceiling)
    }
}

impl JobApi {
    /// The service with `submit` as the submit group's context; every other part is kept.
    #[must_use]
    pub fn with_submit(self, submit: Ctx) -> JobApi {
        JobApi { submit, ..self }
    }
}

/// The answer to a submitted job that `fault` refuses.
fn refused(fault: Fault) -> Status {
    match fault {
        Fault::Field(field) => Status::invalid_argument(format!("field: {field}")),
        Fault::Port(at, field) => {
            Status::invalid_argument(format!("field: spec.ports[{at}].{field}"))
        }
        Fault::Entry(list, at, field) => {
            Status::invalid_argument(format!("field: spec.{list}[{at}].{field}"))
        }
        Fault::NoSource(list, at) => Status::invalid_argument(format!(
            "field: spec.{list}[{at}]: no source this server knows"
        )),
        Fault::WallNotPositive => refusal_status(
            RefusalCode::WallTimeNotPositive,
            "the job has no wall limit, or one of 0",
        ),
        Fault::WallAboveCeiling => refusal_status(
            RefusalCode::WallTimeAboveCeiling,
            "the job's wall limit is above the service's ceiling",
        ),
    }
}

/// The answer to a job nothing could ever serve.
fn no_capable_host() -> Status {
    refusal_status(
        RefusalCode::NoCapableHost,
        "no host and no catalog could ever serve the job",
    )
}

/// The refusal of a job id that names a different job, or another client's job.
fn id_conflict() -> Status {
    refusal_status(
        RefusalCode::JobIdConflict,
        "the job id names a different job",
    )
}

/// The answer to a store operation that did not happen: `job-id-conflict` for an id that
/// names another job, `UNAVAILABLE` `state-fenced` when the store says this process is no
/// longer its writer (which also trips `api`'s fence, so the server stops), else
/// `state-write-failed`, the cause logged and never answered.
pub(super) fn not_written(api: &JobApi) -> impl Fn(JobStoreError) -> Status + '_ {
    move |error| match error {
        JobStoreError::JobIdConflict { .. } => id_conflict(),
        error => {
            tracing::warn!(%error, "a write of the job tables did not happen");
            unavailable_if_fenced(api, &error, Status::internal(STATE_WRITE_FAILED))
        }
    }
}

/// `UNAVAILABLE` `state-fenced` when `error` is the store saying this process is no longer its
/// writer (which trips `api`'s fence), else `otherwise`. The agents' heartbeats answer the same
/// as a 503.
pub(super) fn unavailable_if_fenced(
    api: &JobApi,
    error: &JobStoreError,
    otherwise: Status,
) -> Status {
    match api.fence.note(error) {
        true => Status::unavailable(STATE_FENCED),
        false => otherwise,
    }
}

/// `SubmitJob`: validate, look the id up, admit a new job, and commit it before answering.
pub(super) async fn submit_job(
    api: &JobApi,
    caller: &Caller,
    request: Request<SubmitJobRequest>,
) -> Result<Response<SubmitJobResponse>, Status> {
    let Some(served) = &api.submit.served else {
        return super::unimplemented();
    };
    let client = Some(caller.name.clone());
    let request = request.into_inner();
    let valid = validate::job(request.spec.as_ref(), served.ceiling).map_err(refused)?;
    let held = match &valid.id {
        Some(id) => api.jobs.get_job(id).map_err(not_written(api))?.is_some(),
        None => false,
    };
    let isolation = valid.spec.constraints.isolation;
    if !held
        && !served
            .admission
            .0
            .can_ever_serve(&valid.wanted, isolation)
            .await
    {
        return Err(no_capable_host());
    }
    let new = NewJob {
        id: valid.id,
        spec: valid.spec,
        priority: valid.priority,
        client,
    };
    let inserted = api.jobs.insert_queued(new).map_err(not_written(api))?;
    if !caller.owns(&inserted.record) {
        return Err(id_conflict());
    }
    Ok(Response::new(SubmitJobResponse {
        job: Some(view::job(&inserted.record)),
    }))
}

#[cfg(test)]
#[path = "submit_tests.rs"]
mod tests;
