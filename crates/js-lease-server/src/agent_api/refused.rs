//! The error answers of the agent routes: a status and `{"error": "<code>"}`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use js_agent_wire::ProtocolRange;
use js_registry::Refusal;
use serde::Serialize;

use crate::join_tokens::JoinRefusal;

/// An error answer: a status and `{"error": "<code>"}`, with the majors the server
/// speaks when the code is `unsupported-protocol`.
#[derive(Debug, Serialize)]
pub(crate) struct Refused {
    #[serde(skip)]
    pub(super) status: StatusCode,
    pub(super) error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) supported: Option<ProtocolRange>,
    /// The capability at fault, for the two refusals of the host contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) field: Option<&'static str>,
    /// Which condition failed, for `shape-not-accepted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reason: Option<&'static str>,
}

impl Refused {
    pub(super) fn new(status: StatusCode, error: &'static str) -> Self {
        Self {
            status,
            error,
            supported: None,
            field: None,
            reason: None,
        }
    }

    /// The answer to a refused one-time token: 503 when the use could not be written down or
    /// the credential could not be made, 403 otherwise. The code names the fault and
    /// nothing of the token.
    pub(super) fn join(refusal: JoinRefusal) -> Self {
        let status = match refusal {
            JoinRefusal::Unsaved | JoinRefusal::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::FORBIDDEN,
        };
        Self::new(status, refusal.code())
    }

    /// The answer to a registry refusal: 400 for a message that breaks the wire's rules,
    /// 409 for a session the agent should say hello again for (or wait out), 403 for
    /// what the credential or the pool does not allow, 503 when no session secret could
    /// be made.
    pub(super) fn of(refusal: &Refusal) -> Self {
        let status = match refusal {
            Refusal::Invalid(_) | Refusal::BadHostId(_) | Refusal::BadHeartbeatHostId(_) => {
                StatusCode::BAD_REQUEST
            }
            Refusal::HostLive | Refusal::StaleEpoch | Refusal::UnknownHost => StatusCode::CONFLICT,
            Refusal::UnknownPool
            | Refusal::HostBound
            | Refusal::HostListed
            | Refusal::ShapeNotAccepted(_)
            | Refusal::HostContract { .. }
            | Refusal::ExecutionMismatch { .. }
            | Refusal::PoolFull
            | Refusal::SessionMismatch => StatusCode::FORBIDDEN,
            Refusal::SessionUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        let code = refusal.code();
        let unsupported = code == js_agent_wire::refusal::UNSUPPORTED_PROTOCOL;
        Self {
            supported: unsupported.then_some(ProtocolRange::THIS),
            field: refusal.field(),
            reason: refusal.reason(),
            ..Self::new(status, code)
        }
    }
}

impl From<Refusal> for Refused {
    fn from(refusal: Refusal) -> Self {
        Self::of(&refusal)
    }
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        (self.status, Json(&self)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use js_agent_wire::Phase;
    use js_registry::ShapeFault;
    use js_types::HostId;
    use serde_json::json;

    use super::*;
    use crate::join_tokens::JoinRefusal;
    use js_agent_wire::{Heartbeat, Hello};

    /// Every refusal of the registry has the status and the wire code the API documents:
    /// 400 for a message that breaks the wire's rules, 409 for the refusals that tell an
    /// agent to say hello again or wait, 403 for what the credential or the pool does not
    /// allow, 503 when no session secret could be made; and only `unsupported-protocol`
    /// carries the majors served. It catches a new refusal that falls into the wrong class
    /// of status, and the 503, which no transport test can provoke because the registry
    /// draws its secrets from the operating system.
    #[test]
    fn every_refusal_has_its_status_and_code() {
        let unnamed = Heartbeat {
            seq: None,
            host_id: "UPPER".into(),
            epoch: 0,
            session: String::new(),
            phase: Phase::Serving,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            taint_cleared: None,
            exclusive_ready: None,
            exclusive_refused: None,
            load: None,
            disk_gib: None,
            jobs: Vec::new(),
            retired_jobs: Vec::new(),
            log_grant_state: None,
        };
        let hello: Hello = serde_json::from_value(json!({
            "protocol": { "min": 2, "max": 3 },
            "host_id": "h",
            "pool": "p",
            "agent_version": "1",
            "shape": { "cpu_millis": 0, "memory_bytes": 0, "gpus": 0 },
            "phase": "serving",
        }))
        .unwrap();
        let bad_id = HostId::new("").unwrap_err();
        let cases = [
            (
                Refusal::Invalid(unnamed.validate().unwrap_err()),
                400,
                "invalid-heartbeat",
            ),
            (
                Refusal::Invalid(hello.validate().unwrap_err()),
                400,
                "unsupported-protocol",
            ),
            (Refusal::BadHostId(bad_id.clone()), 400, "invalid-hello"),
            (
                Refusal::BadHeartbeatHostId(bad_id),
                400,
                "invalid-heartbeat",
            ),
            (Refusal::HostLive, 409, "host-live"),
            (Refusal::StaleEpoch, 409, "stale-epoch"),
            (Refusal::UnknownHost, 409, "unknown-host"),
            (Refusal::UnknownPool, 403, "unknown-pool"),
            (Refusal::HostBound, 403, "host-bound"),
            (Refusal::HostListed, 403, "host-listed"),
            (
                Refusal::ShapeNotAccepted(ShapeFault::BelowFloor),
                403,
                "shape-not-accepted",
            ),
            (
                Refusal::ShapeNotAccepted(ShapeFault::QuotaDefaultUnfit),
                403,
                "shape-not-accepted",
            ),
            (
                Refusal::ShapeNotAccepted(ShapeFault::QuotaWithoutPoolDisk),
                403,
                "shape-not-accepted",
            ),
            (
                Refusal::HostContract { field: "runtime" },
                403,
                "host-contract",
            ),
            (
                Refusal::ExecutionMismatch { field: "exec" },
                403,
                "execution-mismatch",
            ),
            (Refusal::PoolFull, 403, "pool-full"),
            (Refusal::SessionMismatch, 403, "session-mismatch"),
            (Refusal::SessionUnavailable, 503, "session-unavailable"),
        ];
        for (refusal, status, code) in cases {
            let answer = Refused::of(&refusal);
            assert_eq!(answer.status.as_u16(), status, "{code}");
            assert_eq!(answer.error, code);
            let says_majors = code == "unsupported-protocol";
            assert_eq!(answer.supported.is_some(), says_majors, "{code}");
            assert_eq!(answer.field, refusal.field(), "{code}");
            assert_eq!(answer.reason, refusal.reason(), "{code}");
        }
        let reasons = [
            (ShapeFault::BelowFloor, "below-floor"),
            (ShapeFault::QuotaDefaultUnfit, "quota-default-unfit"),
            (ShapeFault::QuotaWithoutPoolDisk, "quota-without-pool-disk"),
        ];
        for (fault, reason) in reasons {
            let answer = Refused::of(&Refusal::ShapeNotAccepted(fault));
            assert_eq!(
                serde_json::to_value(&answer).unwrap(),
                json!({ "error": "shape-not-accepted", "reason": reason })
            );
        }
        let plain = serde_json::to_value(Refused::of(&Refusal::HostLive)).unwrap();
        assert_eq!(plain, json!({ "error": "host-live" }));
    }

    /// Every refusal of a one-time token has its status and code: 503 for the use that
    /// could not be written down (the agent may try again), 403 for the rest.
    #[test]
    fn every_join_refusal_has_its_status_and_code() {
        let cases = [
            (JoinRefusal::Expired, 403, "join-token-expired"),
            (JoinRefusal::Used, 403, "join-token-used"),
            (JoinRefusal::WrongInstance, 403, "join-token-instance"),
            (JoinRefusal::NotAdmitted, 403, "join-token-not-admitted"),
            (JoinRefusal::Unsaved, 503, "join-token-unsaved"),
            (JoinRefusal::Unavailable, 503, "join-token-unavailable"),
            (
                JoinRefusal::CredentialRequired,
                403,
                "join-token-credential-required",
            ),
        ];
        for (refusal, status, code) in cases {
            let answer = Refused::join(refusal);
            assert_eq!(answer.status.as_u16(), status, "{code}");
            assert_eq!(answer.error, code);
            assert!(answer.supported.is_none());
        }
    }
}
