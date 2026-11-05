//! The read calls: `GetJob`, `ListJobs`, `GetResult` and `GetLogs`.
//!
//! Each call answers `UNIMPLEMENTED` until this file serves it; serving it changes this file
//! alone. The group's own state is [`Ctx`], set through [`JobApi::with_read`].

use js_proto::v1::{
    GetJobRequest, GetJobResponse, GetLogsRequest, GetLogsResponse, GetResultRequest,
    GetResultResponse, ListJobsRequest, ListJobsResponse,
};
use tonic::{Request, Response, Status};

use super::JobApi;

/// The state of the read group. It holds nothing until the group is served.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ctx {
    /// A value only tests set, so that a test can tell a context it gave from the default.
    #[cfg(test)]
    pub(super) mark: u32,
}

impl JobApi {
    /// The service with `read` as the read group's context; every other part is kept.
    #[must_use]
    pub fn with_read(self, read: Ctx) -> JobApi {
        JobApi { read, ..self }
    }
}

/// `GetJob`: not served yet.
pub(super) async fn get_job(
    _api: &JobApi,
    _request: Request<GetJobRequest>,
) -> Result<Response<GetJobResponse>, Status> {
    super::unimplemented()
}

/// `ListJobs`: not served yet.
pub(super) async fn list_jobs(
    _api: &JobApi,
    _request: Request<ListJobsRequest>,
) -> Result<Response<ListJobsResponse>, Status> {
    super::unimplemented()
}

/// `GetResult`: not served yet.
pub(super) async fn get_result(
    _api: &JobApi,
    _request: Request<GetResultRequest>,
) -> Result<Response<GetResultResponse>, Status> {
    super::unimplemented()
}

/// `GetLogs`: not served yet.
pub(super) async fn get_logs(
    _api: &JobApi,
    _request: Request<GetLogsRequest>,
) -> Result<Response<GetLogsResponse>, Status> {
    super::unimplemented()
}
