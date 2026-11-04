//! The JSON bodies of the API and the errors they turn into.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use js_agent_wire::limits;
use js_lease::{AcquireError, LeaseInfo, ReleaseError, RenewError};
use js_provider::RefusalCode;
use js_types::{
    ClassId, HostId, LeaseId, Request, RequestId, Requirements, RequirementsError, Resources,
};
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
    /// A probe: the one host the lease may be placed on, past that host's cordon. Left out
    /// (or null), the lease is placed as before.
    #[serde(default)]
    probe_host: Option<String>,
    /// The consumer's id for the request: a repeat of the acquire under it finds the
    /// lease it made, and a whole-node request waits for a host under it. Left out (or
    /// null), the acquire is answered as before.
    #[serde(default)]
    request_id: Option<String>,
}

/// The reason of the 400 for a `probe_host` that is not a host id.
const PROBE_HOST_INVALID: &str = "probe_host is not a valid host id";

/// The reason of the 400 for a `request_id` that is not a request id.
const REQUEST_ID_INVALID: &str = "request_id is not a valid request id";

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
    /// Bytes of scratch disk the lease books; above zero and at most
    /// [`limits::MAX_DISK_BYTES`] when present. Left out, the lease books none, as a body
    /// from a client that predates the field does.
    #[serde(default)]
    disk_bytes: Option<u64>,
}

impl AcquireBody {
    /// The request id the body names, if any, or the 400 for one that is not a request id
    /// (1 to 63 bytes of `a-z`, `A-Z`, `0-9`, `.`, `_` and `-`, as [`RequestId`] takes).
    pub(crate) fn request_id(&self) -> Result<Option<RequestId>, ApiError> {
        let id = self.request_id.as_deref().map(RequestId::new).transpose();
        id.map_err(|_| ApiError::bad_request(REQUEST_ID_INVALID))
    }

    /// The request and the ttl asked for, or the 400 for a body that names an id, a
    /// GPU class or a `disk_bytes` the types refuse, or a `disk-gib`, `exec` or `isa_level`
    /// capability that is not a requirement (the registry and the scaler read them as the
    /// job's disk, mode and least ISA level).
    pub(crate) fn into_parts(self) -> Result<(Request, Option<Duration>), ApiError> {
        let class = ClassId::new(&self.class)
            .map_err(|_| ApiError::bad_request("class is not a valid class id"))?;
        let probe_host = self.probe_host.as_deref().map(HostId::new).transpose();
        let probe_host = probe_host.map_err(|_| ApiError::bad_request(PROBE_HOST_INVALID))?;
        let resources = &self.resources;
        let (cpu, memory, gpus) = (resources.cpu_millis, resources.memory_bytes, resources.gpus);
        let resources =
            Resources::new(cpu, memory, gpus, self.resources.gpu_class).map_err(|_| {
                ApiError::bad_request("gpu_class must be named exactly when gpus is above zero")
            })?;
        let resources = match self.resources.disk_bytes {
            None => resources,
            Some(disk) if (1..=limits::MAX_DISK_BYTES).contains(&disk) => {
                resources.with_disk_bytes(disk)
            }
            Some(_) => {
                return Err(ApiError::bad_request(
                    "disk_bytes must be above zero and at most 2^46 - 1",
                ));
            }
        };
        let request = Request {
            class,
            resources,
            capabilities: self.capabilities,
            max_wall: Duration::from_secs(self.max_wall_seconds),
            labels: self.labels,
            validation_run: self.validation_run,
            probe_host,
        };
        Requirements::try_from_request(&request).map_err(|why| {
            ApiError::bad_request(match why {
                RequirementsError::BadDisk(_) => {
                    "disk-gib must be a whole number of GiB below 2^34"
                }
                RequirementsError::BadExec(_) => "exec must be container or sandbox-exec",
                RequirementsError::BadIsaLevel(_) => {
                    "isa_level must be x86-64-v1 to x86-64-v4 or armv8.0-a to armv8.4-a"
                }
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
            | RefusalCode::ProbeUnsupported
            | RefusalCode::ProbeHostUnknown
            | RefusalCode::ProbeHostNotServing
            | RefusalCode::ProbeHostNotCordoned
            | RefusalCode::ProbeHostDraining
            | RefusalCode::ProbeHostMismatch
            | RefusalCode::ProbeHostTooSmall
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
            // The server's ceiling on `max_wall_seconds`: the request broke a rule of the
            // server, as a start-contract refusal does.
            AcquireError::WallAboveCeiling { .. } => {
                Self::coded(StatusCode::UNPROCESSABLE_ENTITY, "wall-time-above-ceiling")
            }
            // The table has issued its last lease id; nothing was booked. The server can
            // issue no lease until the operator restores the state file (js-lease docs).
            AcquireError::IdsExhausted => {
                Self::coded(StatusCode::SERVICE_UNAVAILABLE, "ids-exhausted")
            }
            // The request id names a lease acquired for another request: the client reused
            // an id, and the lease it names is unchanged.
            AcquireError::RequestIdConflict(_) => {
                Self::coded(StatusCode::CONFLICT, "request-id-conflict")
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

    /// Asserts that `probe_host` reaches the request as the host it names, that a body
    /// without it (or with null) is no probe, that the longest host id (63 characters) is
    /// taken, and that a name that is not a host id (empty, 64 characters, out of the
    /// alphabet) is a 400 naming the field and a value that is not a string the parser's
    /// 400. It catches a field that is read and dropped (the probe would
    /// be placed as ordinary work, anywhere), and one that is required.
    #[test]
    fn probe_host_reaches_the_request_and_is_optional() {
        let probe = |extra: &str| {
            let text = format!(
                r#"{{"class":"c","resources":{{"cpu_millis":1000}},"max_wall_seconds":60{extra}}}"#
            );
            parse::<AcquireBody>(text.as_bytes()).unwrap().into_parts()
        };
        let (request, _) = probe(r#","probe_host":"mac-07""#).unwrap();
        assert_eq!(request.probe_host, Some(HostId::new("mac-07").unwrap()));
        assert_eq!(probe("").unwrap().0.probe_host, None);
        assert_eq!(probe(r#","probe_host":null"#).unwrap().0.probe_host, None);
        // The longest host id is 63 characters; one more, an empty name and a name out
        // of the alphabet are each the 400 that names the field.
        let longest = "h".repeat(63);
        let (request, _) = probe(&format!(r#","probe_host":"{longest}""#)).unwrap();
        assert_eq!(request.probe_host, Some(HostId::new(&longest).unwrap()));
        let expected = ApiError::bad_request("probe_host is not a valid host id");
        let too_long = format!(r#","probe_host":"{}""#, "h".repeat(64));
        let empty = r#","probe_host":"""#.to_owned();
        let spaced = r#","probe_host":"Not A Host""#.to_owned();
        for bad in [spaced, empty, too_long] {
            let refused = probe(&bad).unwrap_err();
            let got = (refused.status, refused.code, &refused.reason);
            let want = (expected.status, expected.code, &expected.reason);
            assert_eq!(got, want, "{bad}");
        }
        // A value that is not a string is not this body at all: the parser's 400.
        let text = r#"{"class":"c","resources":{},"max_wall_seconds":60,"probe_host":7}"#;
        let refused = parse::<AcquireBody>(text.as_bytes()).unwrap_err();
        assert_eq!(
            (refused.status, refused.code),
            (StatusCode::BAD_REQUEST, "bad-request")
        );
        assert_eq!(
            refused.reason.as_deref(),
            Some("the body is JSON, but not what this route takes (line 1, column 64)")
        );
    }

    /// The body of `POST /v1/leases` as the release before `probe_host` parsed it: the
    /// same keys and the same refusal of a key it does not know.
    #[allow(dead_code, reason = "parsed only to see whether a body is taken")]
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PreviousAcquireBody {
        class: String,
        resources: PreviousResourcesBody,
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

    #[allow(dead_code, reason = "parsed only to see whether a body is taken")]
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PreviousResourcesBody {
        #[serde(default)]
        cpu_millis: u64,
        #[serde(default)]
        memory_bytes: u64,
        #[serde(default)]
        gpus: u64,
        #[serde(default)]
        gpu_class: Option<String>,
        #[serde(default)]
        disk_bytes: Option<u64>,
    }

    /// Asserts the field across one release in each direction. A start of a client that
    /// predates the field (no `probe_host`) is taken by this server as before, as no
    /// probe. A start of this release's client without a probe (the CLI leaves the key
    /// out) is taken by the previous server, and a probe is refused by it as a body it does
    /// not take (a 400), so a probe is never placed as ordinary work by a server that
    /// cannot judge it. It catches a field that became required, and a client that sends
    /// the key when unset.
    #[test]
    fn probe_host_across_one_release_each_way() {
        let unpinned = r#"{"class":"c","resources":{"cpu_millis":1000,"memory_bytes":0,"gpus":0,"gpu_class":null},"max_wall_seconds":60,"labels":{},"validation_run":null,"ttl_seconds":null}"#;
        let pinned = format!(
            r#"{},"probe_host":"mac-07"}}"#,
            &unpinned[..unpinned.len() - 1]
        );
        let (request, _) = parse::<AcquireBody>(unpinned.as_bytes())
            .unwrap()
            .into_parts()
            .unwrap();
        assert_eq!(request.probe_host, None);
        assert!(parse::<PreviousAcquireBody>(unpinned.as_bytes()).is_ok());
        let refused = parse::<PreviousAcquireBody>(pinned.as_bytes())
            .err()
            .unwrap();
        assert_eq!(
            (refused.status, refused.code),
            (StatusCode::BAD_REQUEST, "bad-request")
        );
        let (request, _) = parse::<AcquireBody>(pinned.as_bytes())
            .unwrap()
            .into_parts()
            .unwrap();
        assert_eq!(request.probe_host, Some(HostId::new("mac-07").unwrap()));
    }

    fn body_with_resources(resources: &str) -> AcquireBody {
        let text =
            format!(r#"{{"class":"whole-node","resources":{resources},"max_wall_seconds":60}}"#);
        parse(text.as_bytes()).unwrap()
    }

    /// Asserts that `resources.disk_bytes` reaches the request's resources as booked
    /// scratch disk, at its edges (1 and 2^46 - 1), and that a body without it (from a
    /// client that predates the field) books none. It catches a field that is read and
    /// dropped, and one that is required.
    #[test]
    fn disk_bytes_reaches_the_resources_and_is_optional() {
        let sized = |text: &str| {
            let (request, _) = body_with_resources(text).into_parts().unwrap();
            request.resources.disk_bytes()
        };
        assert_eq!(sized(r#"{"cpu_millis":1000}"#), 0);
        assert_eq!(sized(r#"{"disk_bytes":1}"#), 1);
        // An explicit null is the same as a field left out: no disk is booked.
        assert_eq!(sized(r#"{"disk_bytes":null}"#), 0);
        assert_eq!(sized(r#"{"disk_bytes":70368744177663}"#), (1 << 46) - 1);
        assert_eq!(
            sized(r#"{"cpu_millis":1,"memory_bytes":2,"disk_bytes":3}"#),
            3
        );
    }

    /// Asserts that a `disk_bytes` of zero or above 2^46 - 1 is a 400 naming the field, as
    /// is one that is not an unsigned number at all (a parse error, not a booking). It
    /// catches a bound that is missing, which lets a sum over many leases overflow, and a
    /// zero that is taken for "no disk" when the caller asked for a bound.
    #[test]
    fn disk_bytes_out_of_range_is_a_400() {
        for bad in ["0", "70368744177664", "18446744073709551615"] {
            let err = body_with_resources(&format!(r#"{{"disk_bytes":{bad}}}"#))
                .into_parts()
                .unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{bad}");
            let reason = err.reason.as_deref().unwrap();
            assert!(reason.starts_with("disk_bytes"), "{reason}");
        }
        for bad in ["-1", "1.5", "\"7\""] {
            let text = format!(
                r#"{{"class":"whole-node","resources":{{"disk_bytes":{bad}}},"max_wall_seconds":60}}"#
            );
            let err = parse::<AcquireBody>(text.as_bytes()).unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{bad}");
        }
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
            assert!(!reason.contains("lots"), "{reason}");
            assert!(!reason.contains("vm"), "{reason}");
        }
    }

    /// Asserts that an `isa_level` capability that is not a level is the 400 that names
    /// `isa_level` and never repeats the value, the same refusal as a bad `disk-gib`.
    /// Catches a level the server admits but no host can state, which would wait for ever.
    #[test]
    fn a_bad_isa_level_is_a_400_naming_it() {
        for caps in [
            r#"{"isa_level":"x86-64-v9"}"#,
            r#"{"isa_level":""}"#,
            r#"{"isa_level":"x86-64-v3,x86-64-v4"}"#,
        ] {
            let err = body(caps).into_parts().unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_REQUEST);
            assert_eq!(
                err.reason.as_deref(),
                Some("isa_level must be x86-64-v1 to x86-64-v4 or armv8.0-a to armv8.4-a"),
                "{caps}"
            );
        }
    }

    /// Asserts that the request bodies a client of the previous release sends (with and
    /// without `capabilities.arch`, `arch=x86_64`, `arch=arm64`, `disk-gib` and `exec`)
    /// parse to the requirements they did, with no ISA level; that `arch=aarch64` reads as
    /// `arm64`; and that a body naming `isa_level` among its capabilities, the key the
    /// capability matcher already reads, asks for that level. Catches a level made a
    /// required part of a body, and a body of the previous release that reads differently.
    #[test]
    fn previous_request_bodies_parse_unchanged() {
        use js_types::{ArmVersion, IsaLevel, X86Level};
        let wanted = |caps: &str| {
            let (request, _) = body(caps).into_parts().unwrap();
            Requirements::from_request(&request)
        };
        let any = wanted("{}");
        assert_eq!((any.os, any.arch, any.isa_level), (None, None, None));
        assert_eq!((any.disk_bytes, any.exec), (None, None));
        for arch in ["x86_64", "arm64"] {
            let one = wanted(&format!(r#"{{"os":"linux","arch":"{arch}"}}"#));
            assert_eq!(one.arch.as_deref(), Some(arch));
            assert_eq!(one.os.as_deref(), Some("linux"));
            assert_eq!(one.isa_level, None);
        }
        let full = wanted(r#"{"arch":"x86_64","disk-gib":"40","exec":"container"}"#);
        assert_eq!(full.disk_bytes, Some(40 << 30));
        assert_eq!(full.exec, Some(ExecMode::Container));
        assert_eq!(full.isa_level, None);
        assert_eq!(
            wanted(r#"{"arch":"aarch64"}"#).arch.as_deref(),
            Some("arm64")
        );
        assert_eq!(
            wanted(r#"{"isa_level":"x86-64-v3"}"#).isa_level,
            Some(IsaLevel::X86_64(X86Level::V3))
        );
        let arm = wanted(r#"{"arch":"arm64","isa_level":"armv8.2-a"}"#);
        assert_eq!(arm.isa_level, Some(IsaLevel::Arm64(ArmVersion::V8_2)));
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
    /// provider's own refusals (a probe's among them) and an ended lease, 422 for a broken
    /// start contract or class rule.
    #[test]
    fn refusals_have_their_status() {
        let conflict = [
            RefusalCode::NoCapacity,
            RefusalCode::NoFit,
            RefusalCode::UnsupportedClass,
            RefusalCode::LeaseEnded,
            RefusalCode::NoCapableHost,
            RefusalCode::ExclusiveNotReady,
            RefusalCode::ProbeUnsupported,
            RefusalCode::ProbeHostUnknown,
            RefusalCode::ProbeHostNotServing,
            RefusalCode::ProbeHostNotCordoned,
            RefusalCode::ProbeHostDraining,
            RefusalCode::ProbeHostMismatch,
            RefusalCode::ProbeHostTooSmall,
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

    /// Asserts that an acquire above the server's ceiling on the wall time is a 422 with
    /// the code `wall-time-above-ceiling`, which names the rule and no value of the request.
    #[test]
    fn a_wall_time_above_the_ceiling_is_a_named_422() {
        let err = ApiError::from(AcquireError::WallAboveCeiling {
            ceiling: Duration::from_secs(600),
        });
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.code, "wall-time-above-ceiling");
        assert_eq!(err.reason, None);
    }
}
