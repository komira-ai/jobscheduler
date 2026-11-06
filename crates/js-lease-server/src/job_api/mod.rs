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
//! service from its state in one place, [`from_server`], and serves both transports on the
//! port of `--job-listen` through [`port`] (`wiring.rs`).
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
//! | `fleet.rs` | `ListHosts`, `CordonHost`, `UncordonHost`, `ListPools`, `ListProviders`, `GetUsage` |
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
//! Each call writes one log line ([`logged`]), `job service call`, at info: the method
//! (`call`), the client (`client`) and its role (`role`), the job or host the request names
//! (`target`; `job` holds the job alone), the outcome (`outcome`: `ok`, `denied`, `not-found`,
//! `refused` or `failed`, [`roles::outcome`]) and the gRPC code (`code`). It is the audit line
//! of the calls that change state, `SubmitJob`, `CancelJob`, `CordonHost` and `UncordonHost`,
//! denied ones among them. No line holds a token or its digest.
//!
//! # One context per group
//!
//! [`JobApi`] holds what every group shares (the job store and the clock) and one context
//! per group, `<group>::Ctx`, defined in the group's file and built by `Default` in
//! [`JobApi::new`]. A group that needs state of its own adds fields to its `Ctx` and sets
//! them through its builder, `JobApi::with_<group>`, which is in the same file and replaces
//! that one context. A new group (a file of its own, such as one for the logs) adds its file,
//! its `mod` line, its field and its default here, and its builder in its file.
//!
//! The job service holds no handle on the lease table or the registry: the server takes the
//! table before the registry, and a call that held either while reading the store would add
//! a third lock to that order. The fleet group reaches them only through the narrow traits
//! of its context (`fleet::Hosts`), whose calls take and drop each lock in that order and
//! return before the group reads the job tables. The submit group reads the registry's hosts
//! and the providers' cached catalogs only through its admission trait (`submit::Admission`),
//! whose call takes the registry's lock alone, calls no provider, and returns before the
//! group writes the job tables.
//!
//! A request carries at most [`js_api_gateway::BODY_LIMIT`] (64 KiB) over either transport:
//! the gateway answers a larger body 413 `request-too-large`, and the gRPC service decodes
//! no larger message (`OUT_OF_RANGE`).

use std::sync::Arc;

use axum::Router;
use js_proto::v1::job_service_server::{JobService, JobServiceServer};
use js_proto::v1::{
    CancelJobRequest, CancelJobResponse, CordonHostRequest, CordonHostResponse, GetJobRequest,
    GetJobResponse, GetLogsRequest, GetLogsResponse, GetResultRequest, GetResultResponse,
    GetUsageRequest, GetUsageResponse, ListHostsRequest, ListHostsResponse, ListJobsRequest,
    ListJobsResponse, ListPoolsRequest, ListPoolsResponse, ListProvidersRequest,
    ListProvidersResponse, SubmitJobRequest, SubmitJobResponse, UncordonHostRequest,
    UncordonHostResponse,
};
use tonic::{Code, Request, Response, Status};
use tower::Layer;

use crate::adapt::SharedClock;
use crate::config::LiveClients;
use crate::fence::Fence;
use crate::job_store::ServerJobs;
use roles::{Caller, admitted, caller_of};

mod auth;
pub(crate) mod cancel;
pub(crate) mod fleet;
pub(crate) mod logs;
pub(crate) mod read;
pub(crate) mod roles;
pub(crate) mod submit;
pub(crate) mod validate;
mod wiring;

pub(crate) use auth::{Bearer, BearerLayer};
pub(crate) use wiring::{from_server, port};

#[cfg(test)]
mod access_tests;
#[cfg(test)]
mod clients_tests;
#[cfg(test)]
mod conformance_tests;
#[cfg(test)]
#[path = "fenced_tests.rs"]
mod fenced_tests;
#[cfg(test)]
mod logs_follow_tests;
#[cfg(test)]
mod logs_rig;
#[cfg(test)]
mod logs_tests;
#[cfg(test)]
mod read_list_tests;
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

/// The job service: the job store, the clock, and the context of each call group.
pub struct JobApi {
    jobs: Arc<ServerJobs>,
    clock: SharedClock,
    read: read::Ctx,
    submit: submit::Ctx,
    cancel: cancel::Ctx,
    fleet: fleet::Ctx,
    logs: logs::Ctx,
    /// Trips when the job tables answer that this process is no longer their writer; with
    /// the server it is the agent side's ([`crate::fence`]), so the server stops.
    fence: Fence,
}

impl JobApi {
    /// The job service over the job tables `jobs`, reading time from `clock`, with every
    /// group's context at its default.
    #[must_use]
    pub fn new(jobs: Arc<ServerJobs>, clock: SharedClock) -> JobApi {
        JobApi {
            jobs,
            clock,
            read: read::Ctx::default(),
            submit: submit::Ctx::default(),
            cancel: cancel::Ctx::default(),
            fleet: fleet::Ctx::default(),
            logs: logs::Ctx::default(),
            fence: Fence::new(),
        }
    }

    /// The service tripping `fence` when the job tables say this process is no longer their
    /// writer, and answering `UNAVAILABLE` `state-fenced` to that call.
    #[must_use]
    pub(crate) fn with_fence(self, fence: Fence) -> JobApi {
        JobApi { fence, ..self }
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

/// The line of [`logged`], for the status a call failed with (`None`: it answered). Not
/// generic, so every call writes its line through the same code.
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
    tracing::info!(call, client, role, job, target, outcome, code = ?code, "job service call");
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
