//! The cancel call: `CancelJob`.
//!
//! Each call answers `UNIMPLEMENTED` until this file serves it; serving it changes this file
//! alone. The group's own state is [`Ctx`], set through [`JobApi::with_cancel`].

use js_proto::v1::{CancelJobRequest, CancelJobResponse};
use tonic::{Request, Response, Status};

use super::JobApi;

/// The state of the cancel group. It holds nothing until the group is served.
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

/// `CancelJob`: not served yet.
pub(super) async fn cancel_job(
    _api: &JobApi,
    _request: Request<CancelJobRequest>,
) -> Result<Response<CancelJobResponse>, Status> {
    super::unimplemented()
}
