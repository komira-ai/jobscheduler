//! The job service (`jobscheduler.v1.JobService`, crate `js-proto`) as the server serves it:
//! the service object [`JobApi`], the bearer layer both transports share ([`auth`]), and the
//! two services a listener mounts, [`grpc`] (the generated tonic server) and [`gateway`]
//! (the HTTP/JSON routes of `js-api-gateway`). Both call the same [`JobApi`].
//!
//! Every call answers `UNIMPLEMENTED` (over the gateway, 501 `unimplemented`) until the
//! file of its group serves it; the fleet group's calls do until its context is given the
//! server's handles (`fleet::Ctx::served`). Nothing mounts the two services yet: the
//! listener does.
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
//! The [`JobService`] implementation below is one line per method, which hands the call to
//! its group's function. Serving a call replaces the body of that function and changes
//! nothing outside its file.
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
//! The job service reads no provider catalog and holds no handle on the lease table or the
//! registry: the server takes the table before the registry, and a call that held either
//! while reading the store would add a third lock to that order. The fleet group reaches
//! them only through the narrow traits of its context (`fleet::Hosts`), whose calls take and
//! drop each lock in that order and return before the group reads the job tables.

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
use tonic::{Request, Response, Status};
use tower::Layer;

use crate::adapt::SharedClock;
use crate::config::Token;
use crate::job_store::ServerJobs;

mod auth;
pub(crate) mod cancel;
pub(crate) mod fleet;
pub(crate) mod read;
pub(crate) mod submit;
pub(crate) mod validate;

pub(crate) use auth::{Bearer, BearerLayer};

#[cfg(test)]
mod scan_tests;
#[cfg(test)]
mod tests;

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

/// The gRPC service of `api`: the generated tonic server behind the bearer layer of `token`.
pub(crate) fn grpc(api: Arc<JobApi>, token: Token) -> Bearer<JobServiceServer<JobApi>> {
    grpc_over(api, token)
}

/// The gRPC service of any implementation of the job service, behind the bearer layer of
/// `token`.
fn grpc_over<T: JobService>(service: Arc<T>, token: Token) -> Bearer<JobServiceServer<T>> {
    BearerLayer::new(token).layer(JobServiceServer::from_arc(service))
}

/// The HTTP/JSON gateway of `api` (`js_api_gateway::router`) behind the bearer layer of
/// `token`.
pub(crate) fn gateway(api: Arc<JobApi>, token: Token) -> Router {
    gateway_over(api, token)
}

/// The gateway over any implementation of the job service, behind the bearer layer of
/// `token`.
fn gateway_over<T: JobService>(service: Arc<T>, token: Token) -> Router {
    js_api_gateway::router(service).layer(BearerLayer::new(token))
}

#[tonic::async_trait]
impl JobService for JobApi {
    async fn submit_job(
        &self,
        request: Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        submit::submit_job(self, request).await
    }

    async fn get_job(
        &self,
        request: Request<GetJobRequest>,
    ) -> Result<Response<GetJobResponse>, Status> {
        read::get_job(self, request).await
    }

    async fn list_jobs(
        &self,
        request: Request<ListJobsRequest>,
    ) -> Result<Response<ListJobsResponse>, Status> {
        read::list_jobs(self, request).await
    }

    async fn cancel_job(
        &self,
        request: Request<CancelJobRequest>,
    ) -> Result<Response<CancelJobResponse>, Status> {
        cancel::cancel_job(self, request).await
    }

    async fn get_result(
        &self,
        request: Request<GetResultRequest>,
    ) -> Result<Response<GetResultResponse>, Status> {
        read::get_result(self, request).await
    }

    async fn get_logs(
        &self,
        request: Request<GetLogsRequest>,
    ) -> Result<Response<GetLogsResponse>, Status> {
        read::get_logs(self, request).await
    }

    async fn list_hosts(
        &self,
        request: Request<ListHostsRequest>,
    ) -> Result<Response<ListHostsResponse>, Status> {
        fleet::list_hosts(self, request).await
    }

    async fn cordon_host(
        &self,
        request: Request<CordonHostRequest>,
    ) -> Result<Response<CordonHostResponse>, Status> {
        fleet::cordon_host(self, request).await
    }

    async fn uncordon_host(
        &self,
        request: Request<UncordonHostRequest>,
    ) -> Result<Response<UncordonHostResponse>, Status> {
        fleet::uncordon_host(self, request).await
    }

    async fn list_pools(
        &self,
        request: Request<ListPoolsRequest>,
    ) -> Result<Response<ListPoolsResponse>, Status> {
        fleet::list_pools(self, request).await
    }

    async fn list_providers(
        &self,
        request: Request<ListProvidersRequest>,
    ) -> Result<Response<ListProvidersResponse>, Status> {
        fleet::list_providers(self, request).await
    }

    async fn get_usage(
        &self,
        request: Request<GetUsageRequest>,
    ) -> Result<Response<GetUsageResponse>, Status> {
        fleet::get_usage(self, request).await
    }
}
