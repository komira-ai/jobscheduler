//! The submit call: `SubmitJob`. The checks of a submitted job go in `validate.rs`.
//!
//! Each call answers `UNIMPLEMENTED` until this file serves it; serving it changes this file
//! alone. The group's own state is [`Ctx`], set through [`JobApi::with_submit`].

use js_proto::v1::{SubmitJobRequest, SubmitJobResponse};
use tonic::{Request, Response, Status};

use super::JobApi;

/// The state of the submit group. It holds nothing until the group is served.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ctx {
    /// A value only tests set, so that a test can tell a context it gave from the default.
    #[cfg(test)]
    pub(super) mark: u32,
}

impl JobApi {
    /// The service with `submit` as the submit group's context; every other part is kept.
    #[must_use]
    pub fn with_submit(self, submit: Ctx) -> JobApi {
        JobApi { submit, ..self }
    }
}

/// `SubmitJob`: not served yet.
pub(super) async fn submit_job(
    _api: &JobApi,
    _request: Request<SubmitJobRequest>,
) -> Result<Response<SubmitJobResponse>, Status> {
    super::unimplemented()
}
