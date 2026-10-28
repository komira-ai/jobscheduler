//! The route that mints one-time join tokens for launched instances.
//!
//! `POST /v1/agent/join-tokens` is behind the client token, like the lease routes and the
//! run routes: the server has no operator credential of its own, and whoever may take
//! leases (the scheduler, the tooling that launches the instances) is who may admit the
//! machines they launch. It exists only when agents register hosts.
//!
//! ```text
//! POST /v1/agent/join-tokens
//! {"provider": "lambdalabs", "instance_id": "i-0abc", "ttl_seconds": 1800}
//!
//! 201 {"token": "<64 hex digits>", "expires_at_ms": 1700000000000}
//! ```
//!
//! `ttl_seconds` is optional (30 minutes) and at most a day. The token is in this answer
//! and nowhere else: the server keeps its digest, and its log lines name the instance and
//! the expiry, never the token. What the token admits is in [`crate::join_tokens`].

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use js_agent_wire::InstanceIdentity;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::agents::Agents;
use crate::api::{BODY_LIMIT, Shared, authenticate};
use crate::join_tokens::{DEFAULT_TTL, MAX_TTL, MintError};
use crate::util::blocking;
use crate::wire::{ApiError, parse};

/// The route, behind the client token and the body cap of every other route.
pub(crate) fn router(shared: Arc<Shared>, agents: Arc<Agents>) -> Router {
    Router::new()
        .route("/v1/agent/join-tokens", post(mint))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(from_fn_with_state(shared, authenticate))
        .with_state(agents)
}

/// The body of the request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MintBody {
    provider: String,
    instance_id: String,
    #[serde(default)]
    ttl_seconds: Option<u64>,
}

/// The 201 body: the only place the token is ever shown.
#[derive(Serialize)]
struct Minted {
    token: String,
    expires_at_ms: u64,
}

/// How long the token lasts: what was asked, or the default; positive and at most
/// [`MAX_TTL`].
fn lifetime(asked: Option<u64>) -> Result<Duration, ApiError> {
    let ttl = asked.map_or(DEFAULT_TTL, Duration::from_secs);
    match (ttl.is_zero(), ttl > MAX_TTL) {
        (true, _) => Err(ApiError::bad_request("ttl_seconds must be positive")),
        (false, true) => Err(ApiError::bad_request("ttl_seconds is above one day")),
        (false, false) => Ok(ttl),
    }
}

/// The answer to a mint that made no token: 503 when no random bytes were to be had, 429
/// when too many tokens wait to be used, 500 when the file could not be written.
fn refusal(err: MintError) -> ApiError {
    match err {
        MintError::NoRandom => ApiError::coded(StatusCode::SERVICE_UNAVAILABLE, "token-unavailable"),
        MintError::Full => ApiError::coded(StatusCode::TOO_MANY_REQUESTS, "too-many-join-tokens"),
        MintError::Unsaved => {
            ApiError::coded(StatusCode::INTERNAL_SERVER_ERROR, "state-write-failed")
        }
    }
}

async fn mint(State(agents): State<Arc<Agents>>, body: Bytes) -> Result<Response, ApiError> {
    let body: MintBody = parse(&body)?;
    let ttl = lifetime(body.ttl_seconds)?;
    let instance = InstanceIdentity {
        provider: body.provider,
        instance_id: body.instance_id,
    };
    instance.validate().map_err(|_| {
        ApiError::bad_request("provider or instance_id is not a valid provider id or instance id")
    })?;
    let named = (instance.provider.clone(), instance.instance_id.clone());
    let made = blocking(move || agents.joins.mint(instance, ttl)).await;
    let minted = made.map_err(|err| {
        let answer = refusal(err);
        warn!(code = answer.code, "join token not minted");
        answer
    })?;
    info!(
        provider = %named.0,
        instance = %named.1,
        expires_at_ms = minted.expires_at_ms,
        "join token minted"
    );
    let reply = Minted {
        token: minted.token,
        expires_at_ms: minted.expires_at_ms,
    };
    Ok((StatusCode::CREATED, Json(reply)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lifetime is the default when none is asked, what is asked inside the bounds,
    /// and a 400 for zero and for more than a day. It catches a token that never expires
    /// or expires at once.
    #[test]
    fn the_lifetime_is_bounded() {
        assert_eq!(lifetime(None).unwrap(), DEFAULT_TTL);
        assert_eq!(lifetime(Some(1)).unwrap(), Duration::from_secs(1));
        assert_eq!(lifetime(Some(86_400)).unwrap(), MAX_TTL);
        assert!(lifetime(Some(0)).is_err());
        assert!(lifetime(Some(86_401)).is_err());
    }

    /// Each reason a token is not made has its own status and code, so a caller can tell
    /// a full queue (wait) from a server that cannot write its file (page someone).
    #[test]
    fn each_failure_has_its_status_and_code() {
        let cases = [
            (MintError::NoRandom, 503, "token-unavailable"),
            (MintError::Full, 429, "too-many-join-tokens"),
            (MintError::Unsaved, 500, "state-write-failed"),
        ];
        for (err, status, code) in cases {
            let answer = refusal(err);
            assert_eq!(answer.status.as_u16(), status, "{code}");
            assert_eq!(answer.code, code);
        }
    }
}
