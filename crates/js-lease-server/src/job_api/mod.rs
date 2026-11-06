//! The job service (`jobscheduler.v1.JobService`, crate `js-proto`) as the server serves it:
//! the service object [`JobApi`], the bearer layer both transports share ([`auth`]), and the
//! two services a listener mounts, [`grpc`] (the generated tonic server) and [`gateway`]
//! (the HTTP/JSON routes of `js-api-gateway`). Both call the same [`JobApi`].
//!
//! Every call answers `UNIMPLEMENTED` (over the gateway, 501 `unimplemented`) until the
//! file of its group serves it; the read group (`read.rs`) and the cancel group
//! (`cancel.rs`) are served, and the submit and fleet groups' calls answer it until their
//! contexts are given the server's handles (`submit::Ctx::served`, `fleet::Ctx::served`).
//! The server builds the service from its state in one place, [`from_server`], and serves
//! both transports on the port of `--job-listen` through [`port`] (`wiring.rs`).
//!
//! # One file per call group
//!
//! Each group of calls is one file, and each call is one `pub(super) async fn` in it:
//!
//! | file | calls |
//! |---|---|
//! | `read.rs` | `GetJob`, `ListJobs`, `GetResult`, `GetLogs` |
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
//! The bearer layer ([`auth`]) admits the token of any client of the server's configuration
//! and puts that client's name in the request's extensions ([`client_of`] reads it). Every
//! client may call every method on every job: there is one trust domain. `SubmitJob` records
//! the name on a new job; a retry of the same job from another client answers the stored
//! job, which keeps the first client. Each call writes one log line, `job service call`, with
//! the method (`call`), the client (`client`), the job the request names or `SubmitJob`
//! answered (`job`, empty when there is none) and the gRPC code of the answer (`code`); no
//! line holds a token.
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
use js_jobs::ClientName;
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
use crate::config::Clients;
use crate::job_store::ServerJobs;

mod auth;
pub(crate) mod cancel;
pub(crate) mod fleet;
pub(crate) mod read;
pub(crate) mod submit;
pub(crate) mod validate;
mod wiring;

pub(crate) use auth::{Bearer, BearerLayer};
pub(crate) use wiring::{from_server, port};

#[cfg(test)]
mod clients_tests;
#[cfg(test)]
mod read_list_tests;
#[cfg(test)]
mod read_tests;
#[cfg(test)]
mod read_view_tests;
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
        }
    }
}

/// The gRPC service of `api`: the generated tonic server behind the bearer layer of
/// `clients`.
pub(crate) fn grpc(api: Arc<JobApi>, clients: Clients) -> Bearer<JobServiceServer<JobApi>> {
    grpc_over(api, clients)
}

/// The gRPC service of any implementation of the job service, behind the bearer layer of
/// `clients`.
fn grpc_over<T: JobService>(service: Arc<T>, clients: Clients) -> Bearer<JobServiceServer<T>> {
    let server =
        JobServiceServer::from_arc(service).max_decoding_message_size(js_api_gateway::BODY_LIMIT);
    BearerLayer::new(clients).layer(server)
}

/// The HTTP/JSON gateway of `api` (`js_api_gateway::router`) behind the bearer layer of
/// `clients`.
pub(crate) fn gateway(api: Arc<JobApi>, clients: Clients) -> Router {
    gateway_over(api, clients)
}

/// The gateway over any implementation of the job service, behind the bearer layer of
/// `clients`.
fn gateway_over<T: JobService>(service: Arc<T>, clients: Clients) -> Router {
    js_api_gateway::router(service).layer(BearerLayer::new(clients))
}

/// The client the bearer layer named for `request`; `None` for a request that did not pass
/// it (a test that calls the service directly).
pub(crate) fn client_of<T>(request: &Request<T>) -> Option<ClientName> {
    request.extensions().get::<ClientName>().cloned()
}

/// Writes the log line of one call: the method `call`, the `client` the bearer layer named
/// (empty when none did), the `job` the call names (empty when it names none) and the code of
/// `answer`.
fn logged<T>(
    call: &'static str,
    client: Option<&ClientName>,
    job: &str,
    answer: &Result<Response<T>, Status>,
) {
    let code = answer.as_ref().err().map_or(Code::Ok, Status::code);
    let client = client.map_or("", ClientName::as_str);
    tracing::info!(call, client, job, code = ?code, "job service call");
}

#[tonic::async_trait]
impl JobService for JobApi {
    async fn submit_job(
        &self,
        request: Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        let client = client_of(&request);
        let asked = request
            .get_ref()
            .spec
            .as_ref()
            .map(|spec| spec.job_id.clone());
        let answer = submit::submit_job(self, request).await;
        let answered = answer
            .as_ref()
            .ok()
            .and_then(|response| response.get_ref().job.as_ref());
        let job = answered.map_or(asked.unwrap_or_default(), |job| job.id.clone());
        logged("SubmitJob", client.as_ref(), &job, &answer);
        answer
    }

    async fn get_job(
        &self,
        request: Request<GetJobRequest>,
    ) -> Result<Response<GetJobResponse>, Status> {
        let client = client_of(&request);
        let job = request.get_ref().job_id.clone();
        let answer = read::get_job(self, request).await;
        logged("GetJob", client.as_ref(), &job, &answer);
        answer
    }

    async fn list_jobs(
        &self,
        request: Request<ListJobsRequest>,
    ) -> Result<Response<ListJobsResponse>, Status> {
        let client = client_of(&request);
        let answer = read::list_jobs(self, request).await;
        logged("ListJobs", client.as_ref(), "", &answer);
        answer
    }

    async fn cancel_job(
        &self,
        request: Request<CancelJobRequest>,
    ) -> Result<Response<CancelJobResponse>, Status> {
        let client = client_of(&request);
        let job = request.get_ref().job_id.clone();
        let answer = cancel::cancel_job(self, request).await;
        logged("CancelJob", client.as_ref(), &job, &answer);
        answer
    }

    async fn get_result(
        &self,
        request: Request<GetResultRequest>,
    ) -> Result<Response<GetResultResponse>, Status> {
        let client = client_of(&request);
        let job = request.get_ref().job_id.clone();
        let answer = read::get_result(self, request).await;
        logged("GetResult", client.as_ref(), &job, &answer);
        answer
    }

    async fn get_logs(
        &self,
        request: Request<GetLogsRequest>,
    ) -> Result<Response<GetLogsResponse>, Status> {
        let client = client_of(&request);
        let job = request.get_ref().job_id.clone();
        let answer = read::get_logs(self, request).await;
        logged("GetLogs", client.as_ref(), &job, &answer);
        answer
    }

    async fn list_hosts(
        &self,
        request: Request<ListHostsRequest>,
    ) -> Result<Response<ListHostsResponse>, Status> {
        let client = client_of(&request);
        let answer = fleet::list_hosts(self, request).await;
        logged("ListHosts", client.as_ref(), "", &answer);
        answer
    }

    async fn cordon_host(
        &self,
        request: Request<CordonHostRequest>,
    ) -> Result<Response<CordonHostResponse>, Status> {
        let client = client_of(&request);
        let answer = fleet::cordon_host(self, request).await;
        logged("CordonHost", client.as_ref(), "", &answer);
        answer
    }

    async fn uncordon_host(
        &self,
        request: Request<UncordonHostRequest>,
    ) -> Result<Response<UncordonHostResponse>, Status> {
        let client = client_of(&request);
        let answer = fleet::uncordon_host(self, request).await;
        logged("UncordonHost", client.as_ref(), "", &answer);
        answer
    }

    async fn list_pools(
        &self,
        request: Request<ListPoolsRequest>,
    ) -> Result<Response<ListPoolsResponse>, Status> {
        let client = client_of(&request);
        let answer = fleet::list_pools(self, request).await;
        logged("ListPools", client.as_ref(), "", &answer);
        answer
    }

    async fn list_providers(
        &self,
        request: Request<ListProvidersRequest>,
    ) -> Result<Response<ListProvidersResponse>, Status> {
        let client = client_of(&request);
        let answer = fleet::list_providers(self, request).await;
        logged("ListProviders", client.as_ref(), "", &answer);
        answer
    }

    async fn get_usage(
        &self,
        request: Request<GetUsageRequest>,
    ) -> Result<Response<GetUsageResponse>, Status> {
        let client = client_of(&request);
        let answer = fleet::get_usage(self, request).await;
        logged("GetUsage", client.as_ref(), "", &answer);
        answer
    }
}
