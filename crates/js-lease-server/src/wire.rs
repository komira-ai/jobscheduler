//! The JSON bodies of the API and the errors they turn into.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use js_lease::{AcquireError, LeaseInfo, ReleaseError, RenewError};
use js_provider::RefusalCode;
use js_types::{ClassId, LeaseId, Request, Requirements, RequirementsError, Resources};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::error::Category;
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
    /// GPU class the types refuse, or a `disk-gib` or `exec` capability that is not a
    /// requirement (the registry and the scaler read them as the job's disk and mode).
    pub(crate) fn into_parts(self) -> Result<(Request, Option<Duration>), ApiError> {
        let class = ClassId::new(&self.class)
            .map_err(|_| ApiError::bad_request("class is not a valid class id"))?;
        let resources = &self.resources;
        let (cpu, memory, gpus) = (resources.cpu_millis, resources.memory_bytes, resources.gpus);
        let resources =
            Resources::new(cpu, memory, gpus, self.resources.gpu_class).map_err(|_| {
                ApiError::bad_request("gpu_class must be named exactly when gpus is above zero")
            })?;
        let request = Request {
            class,
            resources,
            capabilities: self.capabilities,
            max_wall: Duration::from_secs(self.max_wall_seconds),
            labels: self.labels,
            validation_run: self.validation_run,
        };
        Requirements::try_from_request(&request).map_err(|why| {
            ApiError::bad_request(match why {
                RequirementsError::BadDisk(_) => {
                    "disk-gib must be a whole number of GiB below 2^34"
                }
                RequirementsError::BadExec(_) => "exec must be container or sandbox-exec",
            })
        })?;
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

/// Parses a request body as `T`. The 400 says which kind of fault it is and where the
/// parser stopped; it never repeats the body or a value or field name in it, since the
/// parser's own message does.
pub(crate) fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|err| {
        let fault = match err.classify() {
            Category::Data => "the body is JSON, but not what this route takes",
            Category::Syntax | Category::Eof | Category::Io => "the body is not valid JSON",
        };
        ApiError::bad_request(format!(
            "{fault} (line {}, column {})",
            err.line(),
            err.column()
        ))
    })
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
    /// Whether the host the lease holds is set aside for a waiting whole-node request.
    /// Left out when false, so a body from a host that is not reserved reads as before.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) host_reserved: bool,
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
    pub(crate) status: StatusCode,
    pub(crate) code: &'static str,
    reason: Option<String>,
}

impl ApiError {
    pub(crate) fn coded(status: StatusCode, code: &'static str) -> Self {
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
            | RefusalCode::NoCapableHost
            | RefusalCode::ExclusiveNotReady
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
            // The table has issued its last lease id; nothing was booked. The server can
            // issue no lease until the operator restores the state file (js-lease docs).
            AcquireError::IdsExhausted => {
                Self::coded(StatusCode::SERVICE_UNAVAILABLE, "ids-exhausted")
            }
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
    use js_types::ExecMode;

    fn body(capabilities: &str) -> AcquireBody {
        let text = format!(
            r#"{{"class":"whole-node","resources":{{"cpu_millis":1000}},"capabilities":{capabilities},"max_wall_seconds":60}}"#
        );
        parse(text.as_bytes()).unwrap()
    }

    /// Asserts that a request body with `disk-gib` and `exec` capabilities reaches the
    /// job's requirements as bytes and a mode, and that a body without them still asks
    /// for any disk and either mode.
    #[test]
    fn disk_and_exec_reach_the_requirements() {
        let (request, _) = body(r#"{"disk-gib":"40","exec":"sandbox-exec","os":"macos"}"#)
            .into_parts()
            .unwrap();
        let wanted = Requirements::from_request(&request);
        assert_eq!(wanted.disk_bytes, Some(40 << 30));
        assert_eq!(wanted.exec, Some(ExecMode::SandboxExec));
        assert_eq!(wanted.os.as_deref(), Some("macos"));
        let (request, _) = body(r#"{"os":"macos"}"#).into_parts().unwrap();
        let wanted = Requirements::from_request(&request);
        assert_eq!((wanted.disk_bytes, wanted.exec), (None, None));
    }

    /// Asserts that a disk or mode that is not a requirement is a 400 that says which
    /// capability is at fault, before the request reaches the table.
    #[test]
    fn a_bad_disk_or_exec_is_a_400() {
        for (caps, said) in [
            (r#"{"disk-gib":"lots"}"#, "disk-gib"),
            (r#"{"disk-gib":"-1"}"#, "disk-gib"),
            (r#"{"exec":"vm"}"#, "exec"),
        ] {
            let err = body(caps).into_parts().unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_REQUEST);
            let reason = err.reason.as_deref().unwrap();
            assert!(reason.starts_with(said), "{caps}");
            assert!(
                !reason.contains("lots") && !reason.contains("vm"),
                "{reason}"
            );
        }
    }

    /// Asserts the renewal body: without a reservation it is the old body byte for byte,
    /// with one it adds `host_reserved`, and a client that reads only `expires_at_ms`
    /// parses both. It catches a field that is sent when false, and one renamed.
    #[test]
    fn the_renewal_body_adds_host_reserved_only_when_true() {
        let plain = Renewed {
            expires_at_ms: 5,
            host_reserved: false,
        };
        let marked = Renewed {
            expires_at_ms: 5,
            host_reserved: true,
        };
        let (plain, marked) = (
            serde_json::to_string(&plain).unwrap(),
            serde_json::to_string(&marked).unwrap(),
        );
        assert_eq!(plain, r#"{"expires_at_ms":5}"#);
        assert_eq!(marked, r#"{"expires_at_ms":5,"host_reserved":true}"#);
        #[derive(serde::Deserialize)]
        struct OldClient {
            expires_at_ms: u64,
        }
        for body in [plain, marked] {
            let read: OldClient = serde_json::from_str(&body).unwrap();
            assert_eq!(read.expires_at_ms, 5);
        }
    }

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
            RefusalCode::NoCapableHost,
            RefusalCode::ExclusiveNotReady,
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
