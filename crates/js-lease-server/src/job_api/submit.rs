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
//!    retry of a job the ceiling now refuses is refused, not answered from the job. A job
//!    that asks for a completion callback is refused `callback-not-enabled` unless the context
//!    takes callbacks ([`Ctx::accepting_callbacks`], which only a test sets until the release
//!    that accepts one); one that does has its URL's form, an address literal in its URL
//!    against the address rule, and its key's length checked, and the key is never repeated.
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
//!    launches, could ever serve the job; the server's source reads the registry's hosts, the
//!    `node/` rows of the job store (hosts that said hello to another server) and the catalogs as the server's catalog cache holds them, calling no provider, and a
//!    catalog the cache has not read may offer. A job with host isolation is offered only the
//!    hosts that reported they run host jobs, and no catalog. A job nothing could serve is the
//!    refusal `no-capable-host` (`FAILED_PRECONDITION`, 422) and nothing is recorded: no row,
//!    no id.
//! 4. **Commit before the answer.** The job is inserted as queued in one transaction of the
//!    store ([`js_jobs::JobStore::insert_queued_after_get`], handed what step 2 read, so the
//!    id is read once), with the name of the client the bearer layer named (never a value of
//!    the request) and the store's clock as that transaction reads it as its submit time;
//!    only after it returns is the answer built, from the record it returns, with its
//!    callback's status when it has one (read in the same call of the tables; a status that
//!    cannot be read is left out). A job step 2 found is answered from what step 2 read. A
//!    store that fails is `INTERNAL` with the message
//!    `state-write-failed` (500 `state-write-failed`), and a store too busy to take the call
//!    ([`js_state::StoreError::Unavailable`]: no connection came free, or a statement waited
//!    past its limit) is `UNAVAILABLE` with the same message (503 `state-write-failed`), which
//!    a caller may retry; the cause is logged and never answered, and nothing happened: the handler holds nothing outside the store, so there is
//!    nothing to roll back. A second writer that recorded a different job under the id
//!    between the lookup and the commit is `job-id-conflict`.
//!
//! 5. **Poke the poller, answer queued.** When the service places jobs ([`Ctx::poking`]), the
//!    call tells the placer of the job the commit recorded ([`Agents::note_inserted`]: when the
//!    commit is the only write of the job store since the placer last read the job tables, the
//!    next pass takes the job added to those tables rather than reading all four again; while a
//!    pass holds those tables, reading them again, the call does not wait for it and tells the
//!    placer nothing, and that pass or the next reads the job from the store), pokes
//!    the placement poller ([`crate::poller::Poller::poke`]) and answers. It never places the
//!    job itself: the answer for a new job is the job as the commit recorded it, `queued` with
//!    no host, always. The poller's pass, the tick's or a heartbeat's places it
//!    (the pass claims the host's room in the store's own transaction, so it stays right with
//!    several servers); a placement wakes the host's heartbeat this server holds open
//!    ([`crate::held_beat`]), whose reply carries the start at once. A client that wants to know
//!    where and when its job starts reads or watches the job. A retry of a job already held is
//!    answered with that job as it is now, in whatever state (step 2); it pokes the poller too,
//!    whose pass then finds nothing new to place.
//!
//! Every read and write of the job tables runs on the pool for blocking calls
//! ([`JobApi::on_tables`]), so a slow store holds up this call and not the server's workers.
//!
//! The call calls no provider.

use std::sync::Arc;
use std::time::Duration;

use js_callback::AddressRule;
use js_jobs::{Inserted, Isolation, JobRecord, JobStoreError, NewJob};
use js_proto::refusal::refusal_status;
use js_proto::v1::{self, RefusalCode, SubmitJobRequest, SubmitJobResponse};
use js_state::StoreError;
use js_types::Requirements;
use tonic::{Request, Response, Status};

use super::JobApi;
use super::fleet::Handle;
use super::read::{statuses, view};
use super::roles::Caller;
use super::validate::{self, Accepts, Fault};
use crate::agents::Agents;
use crate::util::blocking;

/// The ceiling on a job's wall limit when `--max-job-wall` (or `server.max_job_wall`) is not
/// given: 30 days, the longest `server.max_lease_wall` accepts (it has no default of its own).
pub const DEFAULT_MAX_JOB_WALL: Duration = Duration::from_secs(30 * 24 * 3600);

/// The message of the status a failed store answers.
pub(super) const STATE_WRITE_FAILED: &str = "state-write-failed";

/// Whether anything could ever serve a job (design section 5.6, step 2).
#[tonic::async_trait]
pub(crate) trait Admission: Send + Sync {
    /// `false` only when no host and no catalog could ever serve `wanted`: a busy or silent
    /// host counts, a tainted one too (its taint ends), a departing one does not, and a catalog not yet read may
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
    /// The agent side whose placement poller each recorded job pokes (step 5); `None`: the call
    /// only commits, and the tick's pass finds the queued row.
    poking: Option<Handle<Agents>>,
    /// The fields this release refuses until it accepts them, of which it takes these: none,
    /// but in tests and simulations (`Ctx::accepting_retry`).
    accepts: Accepts,
    /// The address rule a job's callback is judged by (step 1); `None`: the call refuses a job
    /// that asks for a callback (`callback-not-enabled`), until the release that accepts one.
    callbacks: Option<Handle<AddressRule>>,
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
                poking: None,
                accepts: Accepts::default(),
                callbacks: None,
            }),
        }
    }

    /// This context, poking the placement poller of `agents` for each job it records (step 5 of
    /// the module docs). Nothing for a context that is not served.
    #[must_use]
    pub(crate) fn poking(mut self, agents: Arc<Agents>) -> Ctx {
        self.served = self.served.map(|served| Served {
            poking: Some(Handle(agents)),
            ..served
        });
        self
    }
}

impl Ctx {
    /// This context, taking a job's completion callback and judging its address by `rule` (step
    /// 1 of the module docs), where it refused every one. Nothing for a context that is not
    /// served.
    #[must_use]
    pub(crate) fn accepting_callbacks(mut self, rule: AddressRule) -> Ctx {
        self.served = self.served.map(|served| Served {
            callbacks: Some(Handle(Arc::new(rule))),
            ..served
        });
        self
    }
}

impl Ctx {
    /// The default context with `mark` set, for a test that tells one context from another.
    #[cfg(test)]
    pub(super) fn marked(mark: u32) -> Ctx {
        Ctx { mark, served: None }
    }

    /// This context, taking a job's retry policy: what the release that accepts a policy will
    /// do, which a test or a simulation does now. Nothing for a context that is not served.
    pub(crate) fn accepting_retry(mut self) -> Ctx {
        self.served = self.served.map(|served| Served {
            accepts: Accepts { retry: true },
            ..served
        });
        self
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
        Fault::Wire(field, code) => Status::invalid_argument(format!("field: {field}: {code}")),
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
/// names another job, else `state-write-failed`, `UNAVAILABLE` when the store was only too
/// busy to take it and `INTERNAL` otherwise, the cause logged and never answered.
pub(super) fn not_written() -> impl Fn(JobStoreError) -> Status {
    move |error| match error {
        JobStoreError::JobIdConflict { .. } => id_conflict(),
        JobStoreError::Store(StoreError::Unavailable(why)) => {
            tracing::warn!(%why, "a write of the job tables did not happen: the store is busy");
            Status::unavailable(STATE_WRITE_FAILED)
        }
        error => {
            tracing::warn!(%error, "a write of the job tables did not happen");
            Status::internal(STATE_WRITE_FAILED)
        }
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
    let callbacks = served.callbacks.as_ref().map(|rule| rule.0.as_ref());
    let valid = validate::job(
        request.spec.as_ref(),
        served.ceiling,
        served.accepts,
        callbacks,
    )
    .map_err(refused)?;
    let stored = match valid.id.clone() {
        Some(id) => api
            .on_tables(move |jobs| jobs.get_job(&id))
            .await
            .map_err(not_written())?,
        None => None,
    };
    let isolation = valid.spec.constraints.isolation;
    if stored.is_none()
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
    let (inserted, statuses) = api
        .on_tables(move |jobs| {
            let inserted = jobs.insert_queued_after_get(new, stored)?;
            // The job is recorded: a status that cannot be read is left out of the answer.
            let asked = statuses::asking([&inserted.record]);
            let statuses = statuses::read(jobs, &asked).unwrap_or_default();
            Ok((inserted, statuses))
        })
        .await
        .map_err(not_written())?;
    if !caller.owns(&inserted.record) {
        return Err(id_conflict());
    }
    let record = match &served.poking {
        Some(Handle(agents)) => poke(agents, inserted).await,
        None => inserted.record,
    };
    let status = statuses.get(&record.id).map(view::callback_status);
    Ok(Response::new(SubmitJobResponse {
        job: Some(v1::Job {
            callback_status: status,
            ..view::job(&record)
        }),
    }))
}

/// Tells the placer of `agents` of the job just recorded, `inserted`, so that the next pass
/// does not read the job tables again for it ([`Agents::note_inserted`], which never waits for
/// a pass that holds them), and pokes the
/// placement poller ([`crate::poller::Poller::poke`]); the job's record as the commit recorded
/// it. The poke comes after the commit, so the pass it brings reads the job.
async fn poke(agents: &Arc<Agents>, inserted: Inserted) -> JobRecord {
    let noting = Arc::clone(agents);
    let record = blocking(move || {
        noting.note_inserted(&inserted);
        inserted.record
    })
    .await;
    agents.poller.poke();
    record
}

#[cfg(test)]
#[path = "submit_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "submit_poke_tests.rs"]
mod poke_tests;

#[cfg(test)]
#[path = "submit_rows_tests.rs"]
mod rows_tests;

#[cfg(test)]
#[path = "submit_reads_tests.rs"]
mod reads_tests;
