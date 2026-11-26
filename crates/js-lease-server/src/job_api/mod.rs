//! The job service (`jobscheduler.v1.JobService`, crate `js-proto`) as the server serves it:
//! the service object [`JobApi`], the bearer layer both transports share ([`auth`]), and the
//! two services a listener mounts, [`grpc`] (the generated tonic server) and [`gateway`]
//! (the HTTP/JSON routes of `js-api-gateway`). Both call the same [`JobApi`].
//!
//! Every call answers `UNIMPLEMENTED` (over the gateway, 501 `unimplemented`) until the
//! file of its group serves it; the read group (`read.rs`) and the cancel group
//! (`cancel.rs`) are served, and the submit and fleet groups' calls answer it until their
//! contexts are given the server's handles (`submit::Ctx::served`, `fleet::Ctx::served`), as
//! the server's own service's are: it answers no call `UNIMPLEMENTED`. The server builds the
//! service from its state in one place, [`from_server`]; [`SERVED_CALLS`] names the calls it
//! serves. It serves both transports on the port of `--job-listen` through [`port`]
//! (`wiring.rs`).
//!
//! # One file per call group
//!
//! Each group of calls is one file, and each call is one `pub(super) async fn` in it:
//!
//! | file | calls |
//! |---|---|
//! | `read.rs` | `GetJob`, `ListJobs`, `GetResult`, `GetLogs` |
//! | `logs.rs` | `GetLogs` with a stream, which `read.rs` hands it after reading the job's row |
//! | `submit.rs` | `SubmitJob` (with `validate.rs`, the checks of a submitted job) |
//! | `cancel.rs` | `CancelJob` |
//! | `fleet.rs` | `ListHosts`, `CordonHost`, `UncordonHost`, `ClearHostTaint`, `RetireHost`, `ListPools`, `ListProviders`, `GetUsage` |
//!
//! The [`JobService`] implementation below hands each call to its group's function and
//! writes the call's log line ([`logged`]). Serving a call replaces the body of that function
//! and changes nothing outside its file, except the group's one `with_<group>` line in
//! [`from_server`] when its context needs handles of the server.
//!
//! # Clients
//!
//! The bearer layer ([`auth`]) admits the token of any client in force (the set the tick
//! reloads, [`LiveClients`]) and puts that client's name and role in the request's extensions
//! ([`roles::caller_of`] reads them). A call is made as that caller, by the rules of
//! [`roles`]: a `client` submits, and reads, lists and cancels only its own jobs (another
//! client's job is answered `unknown-job`); only an `operator` makes the fleet calls (a client
//! is answered `PERMISSION_DENIED` `operator-only`); a call no bearer layer admitted is
//! `UNAUTHENTICATED`. The fleet gate is here, before the group's function; the ownership
//! checks are in the read, cancel and submit groups, which are handed the caller.
//! `SubmitJob` records the caller's name on a new job.
//!
//! Each call writes one log line ([`logged`]), `job service call`: the method (`call`), the
//! client (`client`) and its role (`role`), the job or host the request names (`target`; `job`
//! holds the job alone), the outcome (`outcome`: `ok`, `denied`, `not-found`, `refused` or
//! `failed`, [`roles::outcome`]) and the gRPC code (`code`). It is the audit line of the calls
//! that change state, `SubmitJob`, `CancelJob`, `CordonHost`, `UncordonHost`,
//! `ClearHostTaint` and `RetireHost`, denied ones among them. The line is at info, except for
//! a read call ([`READ_CALLS`]) that answered: that line is at debug, since its clients poll
//! it hundreds of times a second and an info line each would bury everything else in the log.
//! Every read call that is refused or fails still writes its line at info. No line holds a
//! token or its digest.
//!
//! # One context per group
//!
//! [`JobApi`] holds what every group shares (the job store and the server's clock, which the
//! log pages read; a time of the job tables is read from the store's clock) and one context
//! per group, `<group>::Ctx`, defined in the group's file and built by `Default` in
//! [`JobApi::new`]. A group that needs state of its own adds fields to its `Ctx` and sets
//! them through its builder, `JobApi::with_<group>`, which is in the same file and replaces
//! that one context. A new group (a file of its own, such as one for the logs) adds its file,
//! its `mod` line, its field and its default here, and its builder in its file.
//!
//! The job service holds no handle on the registry: a call that held it while reading the store
//! would add a second lock to the server's order. The fleet group reaches it only through
//! the narrow traits of its context (`fleet::Hosts`), whose calls take and drop the lock
//! and return before the group reads the job tables. The submit group reads the registry's hosts,
//! the job store's `node/` rows and the providers' cached catalogs only through its admission
//! trait (`submit::Admission`), whose call takes the registry's lock (and reads the rows), calls no provider, and returns before the
//! group writes the job tables.
//!
//! # The job tables are read and written off the runtime's workers
//!
//! The store's calls block: a call waits on the database for as long as the database takes
//! (up to its statement timeout). Every call of a group on the job tables therefore goes
//! through [`JobApi::on_tables`], which runs it on the runtime's pool for blocking calls, so
//! a slow database holds up the calls that wait on it and nothing else: the workers go on
//! serving the connections, and a client's HTTP/2 connection is read while its calls wait.
//! A call whose client gives up (cancels the stream) is dropped at its next wait; a store
//! call it started runs to its end, and what it wrote stands, as a call whose answer was
//! lost (a resubmit under the same id is answered with that job).
//!
//! At most [`STORE_CALLS`] such calls run at once (four times the Postgres store's default
//! pool of 8 connections: more would only wait for a connection inside the store). A call
//! waits at most [`STORE_CALL_WAIT`] for one of them, and past it answers as a store too busy
//! to take it (`UNAVAILABLE`: `state-write-failed` for a write, `store-unavailable` for a
//! read), which a client retries. A call that is still waiting when its client gives up is
//! dropped and never runs; one that runs keeps its place to its end. So a stalled store
//! holds at most [`STORE_CALLS`] threads of the blocking pool, however many calls and
//! retries arrive.
//!
//! A request carries at most [`js_api_gateway::BODY_LIMIT`] (64 KiB) over either transport:
//! the gateway answers a larger body 413 `request-too-large`, and the gRPC service decodes
//! no larger message (`OUT_OF_RANGE`).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use js_proto::v1::job_service_server::{JobService, JobServiceServer};
use js_proto::v1::{
    CancelJobRequest, CancelJobResponse, ClearHostTaintRequest, ClearHostTaintResponse,
    CordonHostRequest, CordonHostResponse, GetJobRequest, GetJobResponse, GetLogsRequest,
    GetLogsResponse, GetResultRequest, GetResultResponse, GetUsageRequest, GetUsageResponse,
    ListHostsRequest, ListHostsResponse, ListJobsRequest, ListJobsResponse, ListPoolsRequest,
    ListPoolsResponse, ListProvidersRequest, ListProvidersResponse, RetireHostRequest,
    RetireHostResponse, SubmitJobRequest, SubmitJobResponse, UncordonHostRequest,
    UncordonHostResponse,
};
use tonic::{Code, Request, Response, Status};
use tower::Layer;

use js_jobs::JobStoreError;
use js_state::StoreError;
use tokio::sync::Semaphore;

use crate::clock::SharedClock;
use crate::config::LiveClients;
use crate::job_store::ServerJobs;
use roles::{Caller, admitted, caller_of};

mod auth;
mod callback_check;
pub(crate) mod cancel;
pub(crate) mod fleet;
pub(crate) mod logs;
pub(crate) mod read;
pub(crate) mod roles;
mod rootfs;
pub(crate) mod submit;
pub(crate) mod validate;
mod wiring;

pub(crate) use auth::{Bearer, BearerLayer};
pub use wiring::SERVED_CALLS;
pub(crate) use wiring::{from_server, port};

#[cfg(test)]
mod access_pages_tests;
#[cfg(test)]
mod access_tests;
#[cfg(test)]
mod blocking_tests;
#[cfg(test)]
mod callback_tests;
#[cfg(test)]
mod clients_tests;
#[cfg(test)]
mod conformance_tests;
#[cfg(test)]
mod logs_attempt_tests;
#[cfg(test)]
mod logs_follow_tests;
#[cfg(test)]
mod logs_rig;
#[cfg(test)]
mod logs_tests;
#[cfg(test)]
mod read_attempts_tests;
#[cfg(test)]
mod read_list_tests;
#[cfg(test)]
mod read_queue_tests;
#[cfg(test)]
mod read_tests;
#[cfg(test)]
mod read_view_tests;
#[cfg(test)]
mod reload_tests;
#[cfg(test)]
mod scan_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod writes_rig;

/// The message of the `UNIMPLEMENTED` status every call answers until its group serves it.
const UNIMPLEMENTED: &str = "unimplemented";

/// The answer of a call that is not served yet: `UNIMPLEMENTED`.
fn unimplemented<T>() -> Result<Response<T>, Status> {
    Err(Status::unimplemented(UNIMPLEMENTED))
}

/// The most calls on the job tables that run at once ([`JobApi::on_tables`]).
pub(crate) const STORE_CALLS: usize = 32;

/// How long a call on the job tables waits for one of the [`STORE_CALLS`] to come free.
pub(crate) const STORE_CALL_WAIT: Duration = Duration::from_secs(10);

/// The places of the calls on the job tables, and how long a call waits for one.
struct Gate {
    slots: Arc<Semaphore>,
    size: usize,
    wait: Duration,
}

impl Gate {
    fn new(size: usize, wait: Duration) -> Gate {
        Gate {
            slots: Arc::new(Semaphore::new(size)),
            size,
            wait,
        }
    }
}

/// The job service: the job store, the clock, and the context of each call group.
pub struct JobApi {
    jobs: Arc<ServerJobs>,
    gate: Gate,
    clock: SharedClock,
    read: read::Ctx,
    submit: submit::Ctx,
    cancel: cancel::Ctx,
    fleet: fleet::Ctx,
    logs: logs::Ctx,
}

impl JobApi {
    /// The job service over the job tables `jobs`, with `clock` the server's (the log pages'
    /// time; the times of the job tables are the store's), and every group's context at its
    /// default.
    #[must_use]
    pub fn new(jobs: Arc<ServerJobs>, clock: SharedClock) -> JobApi {
        JobApi {
            jobs,
            gate: Gate::new(STORE_CALLS, STORE_CALL_WAIT),
            clock,
            read: read::Ctx::default(),
            submit: submit::Ctx::default(),
            cancel: cancel::Ctx::default(),
            fleet: fleet::Ctx::default(),
            logs: logs::Ctx::default(),
        }
    }
}

impl JobApi {
    /// The service with at most `size` calls on the job tables at once, each waiting `wait`
    /// at most for a place; every other part is kept.
    #[cfg(test)]
    fn with_store_calls(self, size: usize, wait: Duration) -> JobApi {
        JobApi {
            gate: Gate::new(size, wait),
            ..self
        }
    }

    /// Runs `call` over the job tables on the runtime's pool for blocking calls and waits for
    /// it without holding up a worker, once one of the gate's places is free (see the module's
    /// notes). With no place free within the gate's wait it runs nothing and answers
    /// [`StoreError::Unavailable`]. The place goes with `call` and is given back when `call`
    /// ends, even if the caller is gone by then. A panic in `call` is raised again in the
    /// caller.
    async fn on_tables<T: Send + 'static>(
        &self,
        call: impl FnOnce(&ServerJobs) -> Result<T, JobStoreError> + Send + 'static,
    ) -> Result<T, JobStoreError> {
        let place = match Arc::clone(&self.gate.slots).try_acquire_owned() {
            Ok(place) => place,
            // Only a call that finds every place taken waits on the runtime's timer (a runtime
            // without one, as the simulation's, never meets it while there is room).
            Err(_) => {
                let slots = Arc::clone(&self.gate.slots);
                let waited = tokio::time::timeout(self.gate.wait, slots.acquire_owned()).await;
                let Ok(Ok(place)) = waited else {
                    return Err(JobStoreError::Store(StoreError::Unavailable(format!(
                        "all {} calls on the job tables stayed busy for {:?}",
                        self.gate.size, self.gate.wait
                    ))));
                };
                place
            }
        };
        let jobs = Arc::clone(&self.jobs);
        crate::util::blocking(move || {
            let done = call(&jobs);
            drop(place);
            done
        })
        .await
    }
}

/// The gRPC service of `api`: the generated tonic server behind the bearer layer of
/// `clients`.
pub(crate) fn grpc(api: Arc<JobApi>, clients: LiveClients) -> Bearer<JobServiceServer<JobApi>> {
    grpc_over(api, clients)
}

/// The gRPC service of any implementation of the job service, behind the bearer layer of
/// `clients`.
fn grpc_over<T: JobService>(service: Arc<T>, clients: LiveClients) -> Bearer<JobServiceServer<T>> {
    let server =
        JobServiceServer::from_arc(service).max_decoding_message_size(js_api_gateway::BODY_LIMIT);
    BearerLayer::new(clients).layer(server)
}

/// The HTTP/JSON gateway of `api` (`js_api_gateway::router`) behind the bearer layer of
/// `clients`.
pub(crate) fn gateway(api: Arc<JobApi>, clients: LiveClients) -> Router {
    gateway_over(api, clients)
}

/// The gateway over any implementation of the job service, behind the bearer layer of
/// `clients`.
fn gateway_over<T: JobService>(service: Arc<T>, clients: LiveClients) -> Router {
    js_api_gateway::router(service).layer(BearerLayer::new(clients))
}

/// What a call names: a job (by the request's text), a host, or nothing.
#[derive(Clone, Copy)]
enum Target<'a> {
    /// The job the request names, or `SubmitJob` answered; empty when there is none.
    Job(&'a str),
    /// The host a cordon or an uncordon names.
    Host(&'a str),
    /// Nothing: a listing, or a fleet read.
    None,
}

/// Writes the one log line of a call, `job service call`: the method `call`; the `client`
/// and the `role` the bearer layer named (empty when none did); the `target`, the job or the
/// host the call names (empty when it names none), with `job` the job alone; the `outcome`
/// ([`roles::outcome`]); and the gRPC `code` of `answer`. No line holds a token or a digest.
fn logged<T>(
    call: &'static str,
    caller: Option<&Caller>,
    target: Target<'_>,
    answer: &Result<Response<T>, Status>,
) {
    line(call, caller, target, answer.as_ref().err());
}

/// The calls that change nothing: the job reads and the fleet reads. Their line is at debug
/// when they answered ([`line`]). A call not named here is logged at info.
const READ_CALLS: [&str; 8] = [
    "GetJob",
    "ListJobs",
    "GetResult",
    "GetLogs",
    "ListHosts",
    "ListPools",
    "ListProviders",
    "GetUsage",
];

/// The line of [`logged`], for the status a call failed with (`None`: it answered): at debug
/// for a read call that answered, else at info. Not generic, so every call writes its line
/// through the same code.
fn line(call: &'static str, caller: Option<&Caller>, target: Target<'_>, failed: Option<&Status>) {
    let code = failed.map_or(Code::Ok, Status::code);
    let client = caller.map_or("", |caller| caller.name.as_str());
    let role = caller.map_or("", |caller| caller.role.as_str());
    let (job, target) = match target {
        Target::Job(job) => (job, job),
        Target::Host(host) => ("", host),
        Target::None => ("", ""),
    };
    let outcome = roles::outcome(failed);
    match failed.is_none() && READ_CALLS.contains(&call) {
        true => {
            tracing::debug!(call, client, role, job, target, outcome, code = ?code, "job service call");
        }
        false => {
            tracing::info!(call, client, role, job, target, outcome, code = ?code, "job service call");
        }
    }
}

#[tonic::async_trait]
impl JobService for JobApi {
    async fn submit_job(
        &self,
        request: Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        let caller = caller_of(&request);
        let asked = request
            .get_ref()
            .spec
            .as_ref()
            .map(|spec| spec.job_id.clone());
        let answer = match admitted(caller.as_ref()) {
            Ok(caller) => submit::submit_job(self, caller, request).await,
            Err(status) => Err(status),
        };
        let answered = answer
            .as_ref()
            .ok()
            .and_then(|response| response.get_ref().job.as_ref());
        let job = answered.map_or(asked.unwrap_or_default(), |job| job.id.clone());
        logged("SubmitJob", caller.as_ref(), Target::Job(&job), &answer);
        answer
    }

    async fn get_job(
        &self,
        request: Request<GetJobRequest>,
    ) -> Result<Response<GetJobResponse>, Status> {
        let caller = caller_of(&request);
        let job = request.get_ref().job_id.clone();
        let answer = match admitted(caller.as_ref()) {
            Ok(caller) => read::get_job(self, caller, request).await,
            Err(status) => Err(status),
        };
        logged("GetJob", caller.as_ref(), Target::Job(&job), &answer);
        answer
    }

    async fn list_jobs(
        &self,
        request: Request<ListJobsRequest>,
    ) -> Result<Response<ListJobsResponse>, Status> {
        let caller = caller_of(&request);
        let answer = match admitted(caller.as_ref()) {
            Ok(caller) => read::list_jobs(self, caller, request).await,
            Err(status) => Err(status),
        };
        logged("ListJobs", caller.as_ref(), Target::None, &answer);
        answer
    }

    async fn cancel_job(
        &self,
        request: Request<CancelJobRequest>,
    ) -> Result<Response<CancelJobResponse>, Status> {
        let caller = caller_of(&request);
        let job = request.get_ref().job_id.clone();
        let answer = match admitted(caller.as_ref()) {
            Ok(caller) => cancel::cancel_job(self, caller, request).await,
            Err(status) => Err(status),
        };
        logged("CancelJob", caller.as_ref(), Target::Job(&job), &answer);
        answer
    }

    async fn get_result(
        &self,
        request: Request<GetResultRequest>,
    ) -> Result<Response<GetResultResponse>, Status> {
        let caller = caller_of(&request);
        let job = request.get_ref().job_id.clone();
        let answer = match admitted(caller.as_ref()) {
            Ok(caller) => read::get_result(self, caller, request).await,
            Err(status) => Err(status),
        };
        logged("GetResult", caller.as_ref(), Target::Job(&job), &answer);
        answer
    }

    async fn get_logs(
        &self,
        request: Request<GetLogsRequest>,
    ) -> Result<Response<GetLogsResponse>, Status> {
        let caller = caller_of(&request);
        let job = request.get_ref().job_id.clone();
        let answer = match admitted(caller.as_ref()) {
            Ok(caller) => read::get_logs(self, caller, request).await,
            Err(status) => Err(status),
        };
        logged("GetLogs", caller.as_ref(), Target::Job(&job), &answer);
        answer
    }

    async fn list_hosts(
        &self,
        request: Request<ListHostsRequest>,
    ) -> Result<Response<ListHostsResponse>, Status> {
        let caller = caller_of(&request);
        let answer = match operator(caller.as_ref()) {
            Ok(()) => fleet::list_hosts(self, request).await,
            Err(status) => Err(status),
        };
        logged("ListHosts", caller.as_ref(), Target::None, &answer);
        answer
    }

    async fn cordon_host(
        &self,
        request: Request<CordonHostRequest>,
    ) -> Result<Response<CordonHostResponse>, Status> {
        let caller = caller_of(&request);
        let host = request.get_ref().host_id.clone();
        let answer = match operator(caller.as_ref()) {
            Ok(()) => fleet::cordon_host(self, request).await,
            Err(status) => Err(status),
        };
        logged("CordonHost", caller.as_ref(), Target::Host(&host), &answer);
        answer
    }

    async fn uncordon_host(
        &self,
        request: Request<UncordonHostRequest>,
    ) -> Result<Response<UncordonHostResponse>, Status> {
        let caller = caller_of(&request);
        let host = request.get_ref().host_id.clone();
        let answer = match operator(caller.as_ref()) {
            Ok(()) => fleet::uncordon_host(self, request).await,
            Err(status) => Err(status),
        };
        logged(
            "UncordonHost",
            caller.as_ref(),
            Target::Host(&host),
            &answer,
        );
        answer
    }

    async fn clear_host_taint(
        &self,
        request: Request<ClearHostTaintRequest>,
    ) -> Result<Response<ClearHostTaintResponse>, Status> {
        let caller = caller_of(&request);
        let host = request.get_ref().host_id.clone();
        let answer = match operator(caller.as_ref()) {
            Ok(()) => fleet::clear_host_taint(self, request).await,
            Err(status) => Err(status),
        };
        logged(
            "ClearHostTaint",
            caller.as_ref(),
            Target::Host(&host),
            &answer,
        );
        answer
    }

    async fn retire_host(
        &self,
        request: Request<RetireHostRequest>,
    ) -> Result<Response<RetireHostResponse>, Status> {
        let caller = caller_of(&request);
        let host = request.get_ref().host_id.clone();
        let answer = match operator(caller.as_ref()) {
            Ok(()) => fleet::retire_host(self, request).await,
            Err(status) => Err(status),
        };
        logged("RetireHost", caller.as_ref(), Target::Host(&host), &answer);
        answer
    }

    async fn list_pools(
        &self,
        request: Request<ListPoolsRequest>,
    ) -> Result<Response<ListPoolsResponse>, Status> {
        let caller = caller_of(&request);
        let answer = match operator(caller.as_ref()) {
            Ok(()) => fleet::list_pools(self, request).await,
            Err(status) => Err(status),
        };
        logged("ListPools", caller.as_ref(), Target::None, &answer);
        answer
    }

    async fn list_providers(
        &self,
        request: Request<ListProvidersRequest>,
    ) -> Result<Response<ListProvidersResponse>, Status> {
        let caller = caller_of(&request);
        let answer = match operator(caller.as_ref()) {
            Ok(()) => fleet::list_providers(self, request).await,
            Err(status) => Err(status),
        };
        logged("ListProviders", caller.as_ref(), Target::None, &answer);
        answer
    }

    async fn get_usage(
        &self,
        request: Request<GetUsageRequest>,
    ) -> Result<Response<GetUsageResponse>, Status> {
        let caller = caller_of(&request);
        let answer = match operator(caller.as_ref()) {
            Ok(()) => fleet::get_usage(self, request).await,
            Err(status) => Err(status),
        };
        logged("GetUsage", caller.as_ref(), Target::None, &answer);
        answer
    }
}

/// `Ok` for a fleet call of an operator; else the refusal: `UNAUTHENTICATED` for a request
/// no bearer layer admitted, `PERMISSION_DENIED` `operator-only` for a client.
fn operator(caller: Option<&Caller>) -> Result<(), Status> {
    admitted(caller)?.operator()
}
