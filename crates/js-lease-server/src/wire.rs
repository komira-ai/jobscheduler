//! The JSON bodies of the API and the errors they turn into.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use js_lease::{AcquireError, LeaseInfo, ReleaseError, RenewError};
use js_provider::RefusalCode;
use js_types::{ClassId, LeaseId, Request, Resources};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::error;

/// The body of `POST /v1/leases`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcquireBody {
    class: String,
    resources: ResourcesBody,
    #[serde(default)]
    capabilities: BTreeMap<String, String>,
    max_wall_seconds: u64,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    validation_run: Option<String>,
    #[serde(default)]
    ttl_seconds: Option<u64>,
}

/// The resources of an [`AcquireBody`]; a dimension left out is zero.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourcesBody {
    #[serde(default)]
    cpu_millis: u64,
    #[serde(default)]
    memory_bytes: u64,
    #[serde(default)]
    gpus: u64,
    #[serde(default)]
    gpu_class: Option<String>,
}

impl AcquireBody {
    /// The request and the ttl asked for, or the 400 for a body that names an id or a
    /// GPU class the types refuse.
    pub(crate) fn into_parts(self) -> Result<(Request, Option<Duration>), ApiError> {
        let class = ClassId::new(&self.class).map_err(ApiError::bad_request)?;
        let resources = &self.resources;
        let (cpu, memory, gpus) = (resources.cpu_millis, resources.memory_bytes, resources.gpus);
        let resources = Resources::new(cpu, memory, gpus, self.resources.gpu_class)
            .map_err(ApiError::bad_request)?;
        let request = Request {
            class,
            resources,
            capabilities: self.capabilities,
            max_wall: Duration::from_secs(self.max_wall_seconds),
            labels: self.labels,
            validation_run: self.validation_run,
        };
        Ok((request, self.ttl_seconds.map(Duration::from_secs)))
    }
}

/// The body of `POST /v1/leases/{id}/renew`; the body itself may be left out.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenewBody {
    #[serde(default)]
    pub(crate) ttl_seconds: Option<u64>,
}

/// Parses a request body as `T`. The error carries the parser's reason, which names a
/// field or a position and never repeats the body.
pub(crate) fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|err| ApiError::bad_request(format!("invalid JSON body: {err}")))
}

/// A lease that was granted: the 201 body.
#[derive(Debug, Serialize)]
pub(crate) struct Granted {
    pub(crate) id: String,
    pub(crate) host: String,
    pub(crate) expires_at_ms: u64,
}

/// A lease whose start the provider has not confirmed: the 202 body.
#[derive(Debug, Serialize)]
pub(crate) struct Pending {
    pub(crate) id: String,
    pub(crate) state: &'static str,
}

/// The 200 body of a renew.
#[derive(Debug, Serialize)]
pub(crate) struct Renewed {
    pub(crate) expires_at_ms: u64,
}

/// The body of a release: `freed` or `releasing`.
#[derive(Debug, Serialize)]
pub(crate) struct Released {
    pub(crate) state: &'static str,
}

/// One lease of the list.
#[derive(Debug, Serialize)]
pub(crate) struct LeaseView {
    id: String,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    acquired_at_ms: u64,
    expires_at_ms: u64,
    ends_at_ms: u64,
    labels: BTreeMap<String, String>,
}

impl From<LeaseInfo> for LeaseView {
    fn from(info: LeaseInfo) -> Self {
        Self {
            id: info.id.to_string(),
            state: info.state.to_string(),
            host: info.host.map(|host| host.to_string()),
            acquired_at_ms: info.acquired_at.millis(),
            expires_at_ms: info.expires_at.millis(),
            ends_at_ms: info.ends_at.millis(),
            labels: info
                .labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
}

/// The body of `GET /v1/leases`.
#[derive(Debug, Serialize)]
pub(crate) struct LeaseList {
    pub(crate) leases: Vec<LeaseView>,
}

/// An error answer: a status and `{"error": "<code>"}`, with a `reason` for a 400.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    reason: Option<String>,
}

impl ApiError {
    fn coded(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            reason: None,
        }
    }

    /// A 400 with a short reason.
    pub(crate) fn bad_request(reason: impl ToString) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad-request",
            reason: Some(reason.to_string()),
        }
    }

    /// The 404 of an unknown route.
    pub(crate) fn no_route() -> Self {
        Self::coded(StatusCode::NOT_FOUND, "not-found")
    }

    /// The 404 of a lease the table does not know.
    pub(crate) fn unknown_lease() -> Self {
        Self::coded(StatusCode::NOT_FOUND, "unknown-lease")
    }

    /// The 500 of a lease file that could not be written. The path stays in the log.
    fn state_write(cause: &dyn std::error::Error) -> Self {
        error!(error = %cause, "request failed");
        Self::coded(StatusCode::INTERNAL_SERVER_ERROR, "state-write-failed")
    }
}

/// The status of a refusal: 409 when the request was sound and the provider has no room
/// or the lease ended, 422 when the request broke a rule of the start contract or of its
/// class.
pub(crate) fn refusal_status(code: RefusalCode) -> StatusCode {
    let conflict = matches!(
        code,
        RefusalCode::NoCapacity
            | RefusalCode::NoFit
            | RefusalCode::UnsupportedClass
            | RefusalCode::LeaseEnded
    );
    match conflict {
        true => StatusCode::CONFLICT,
        false => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

impl From<AcquireError> for ApiError {
    fn from(err: AcquireError) -> Self {
        match err {
            AcquireError::TtlNotPositive => Self::bad_request("ttl_seconds must be positive"),
            AcquireError::Refused(code) => Self::coded(refusal_status(code), code.as_str()),
            // A pending start is answered by the caller before this conversion; the arm
            // is shared so that no path invents an answer for it.
            AcquireError::Unconfirmed(_) | AcquireError::Save(_) => Self::state_write(&err),
        }
    }
}

impl From<RenewError> for ApiError {
    fn from(err: RenewError) -> Self {
        match err {
            RenewError::TtlNotPositive => Self::bad_request("ttl_seconds must be positive"),
            RenewError::Unknown(_) => Self::unknown_lease(),
            RenewError::NotHeld { .. } => Self::coded(StatusCode::CONFLICT, "not-held"),
            RenewError::Expired(_) => Self::coded(StatusCode::CONFLICT, "expired"),
            RenewError::Save(_) => Self::state_write(&err),
        }
    }
}

impl From<ReleaseError> for ApiError {
    fn from(err: ReleaseError) -> Self {
        match err {
            ReleaseError::Unknown(_) => Self::unknown_lease(),
            ReleaseError::Save(_) => Self::state_write(&err),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = match self.reason {
            None => serde_json::json!({ "error": self.code }),
            Some(reason) => serde_json::json!({ "error": self.code, "reason": reason }),
        };
        (self.status, Json(body)).into_response()
    }
}

/// The lease id a path segment names, if it is exactly what [`LeaseId`] prints.
pub(crate) fn parse_id(text: &str) -> Option<LeaseId> {
    let (term, sequence) = text.split_once('-')?;
    let id = LeaseId::new(term.parse().ok()?, sequence.parse().ok()?);
    (id.to_string() == text).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a lease id in a path is read only in the exact form the table
    /// prints, so no two paths name one lease.
    #[test]
    fn ids_parse_only_in_canonical_form() {
        assert_eq!(parse_id("3-12"), Some(LeaseId::new(3, 12)));
        assert_eq!(parse_id("0-0"), Some(LeaseId::new(0, 0)));
        for text in [
            "", "3", "3-", "-3", "a-1", "1-b", "01-2", "1-02", "+1-2", "1-+2", "1-2-3", " 1-2",
            "1--2",
        ] {
            assert_eq!(parse_id(text), None, "{text:?}");
        }
    }

    /// Asserts that each refusal code has the status the API documents: 409 for the
    /// provider's own refusals and an ended lease, 422 for a broken start contract or
    /// class rule.
    #[test]
    fn refusals_have_their_status() {
        let conflict = [
            RefusalCode::NoCapacity,
            RefusalCode::NoFit,
            RefusalCode::UnsupportedClass,
            RefusalCode::LeaseEnded,
        ];
        let unprocessable = [
            RefusalCode::WallTimeNotPositive,
            RefusalCode::ReservedLabel,
            RefusalCode::InvalidLabel,
            RefusalCode::UnknownClass,
            RefusalCode::BelowClassMinimum,
        ];
        for code in conflict {
            assert_eq!(
                refusal_status(code),
                StatusCode::CONFLICT,
                "{}",
                code.as_str()
            );
        }
        for code in unprocessable {
            assert_eq!(
                refusal_status(code),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{}",
                code.as_str()
            );
        }
    }
}
