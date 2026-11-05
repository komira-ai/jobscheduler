//! The fleet calls: `ListHosts`, `CordonHost`, `UncordonHost`, `ListPools`, `ListProviders`
//! and `GetUsage`.
//!
//! Each call answers `UNIMPLEMENTED` until this file serves it; serving it changes this file
//! alone. The group's own state is [`Ctx`], set through [`JobApi::with_fleet`].

use js_proto::v1::{
    CordonHostRequest, CordonHostResponse, GetUsageRequest, GetUsageResponse, ListHostsRequest,
    ListHostsResponse, ListPoolsRequest, ListPoolsResponse, ListProvidersRequest,
    ListProvidersResponse, UncordonHostRequest, UncordonHostResponse,
};
use tonic::{Request, Response, Status};

use super::JobApi;

/// The state of the fleet group. It holds nothing until the group is served.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ctx {
    /// A value only tests set, so that a test can tell a context it gave from the default.
    #[cfg(test)]
    pub(super) mark: u32,
}

impl JobApi {
    /// The service with `fleet` as the fleet group's context; every other part is kept.
    #[must_use]
    pub fn with_fleet(self, fleet: Ctx) -> JobApi {
        JobApi { fleet, ..self }
    }
}

/// `ListHosts`: not served yet.
pub(super) async fn list_hosts(
    _api: &JobApi,
    _request: Request<ListHostsRequest>,
) -> Result<Response<ListHostsResponse>, Status> {
    super::unimplemented()
}

/// `CordonHost`: not served yet.
pub(super) async fn cordon_host(
    _api: &JobApi,
    _request: Request<CordonHostRequest>,
) -> Result<Response<CordonHostResponse>, Status> {
    super::unimplemented()
}

/// `UncordonHost`: not served yet.
pub(super) async fn uncordon_host(
    _api: &JobApi,
    _request: Request<UncordonHostRequest>,
) -> Result<Response<UncordonHostResponse>, Status> {
    super::unimplemented()
}

/// `ListPools`: not served yet.
pub(super) async fn list_pools(
    _api: &JobApi,
    _request: Request<ListPoolsRequest>,
) -> Result<Response<ListPoolsResponse>, Status> {
    super::unimplemented()
}

/// `ListProviders`: not served yet.
pub(super) async fn list_providers(
    _api: &JobApi,
    _request: Request<ListProvidersRequest>,
) -> Result<Response<ListProvidersResponse>, Status> {
    super::unimplemented()
}

/// `GetUsage`: not served yet.
pub(super) async fn get_usage(
    _api: &JobApi,
    _request: Request<GetUsageRequest>,
) -> Result<Response<GetUsageResponse>, Status> {
    super::unimplemented()
}
