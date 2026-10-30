//! The route that mints one-time join tokens for launched instances.
//!
//! `POST /v1/agent/join-tokens` is behind the client token, like the lease routes and the
//! run routes: the server has no operator credential of its own, and whoever may take
//! leases (the scheduler, the tooling that launches the instances) is who may admit the
//! machines they launch. It exists only when agents register hosts. The scheduler's own
//! launch path does the same in process ([`crate::agents::Agents::mint_for`] and
//! `revoke_for`) and shares the token file with these routes. The `instance_id` is an id
//! the minting side chose, not necessarily the provider's.
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
//! the expiry, never the token. The 201 carries `Cache-Control: no-store`, so no cache on
//! the way keeps the token. What the token admits is in [`crate::join_tokens`].
//!
//! The scheduler's terminate path revokes the tokens of an instance it terminates, which
//! closes the window in which a used token would otherwise stay good for its host id
//! until a day of silence. The route is gated exactly like the mint (client token only; a
//! pool join token gets 401):
//!
//! ```text
//! DELETE /v1/agent/join-tokens
//! {"provider": "lambdalabs", "instance_id": "i-0abc"}
//!
//! 204
//! ```
//!
//! Every token bound to that provider and instance is dropped, used or unused, and the
//! drop is written like a use (owner-only file, whole-file replace, fsync, under the
//! same lock). The token is unknown from then on: its hello and heartbeat get the 401 of
//! a token that never existed. Revoking is idempotent: an instance with no token (never
//! minted, or already revoked) is also a 204 and writes nothing, so a terminate that
//! is retried needs no care. An unused token past its expiry stays in memory until the
//! next mint drops it, so revoking it is a write, and also a 204. A host id the registry
//! bound to a revoked token stays bound to it (a hello for it with a new token is refused
//! `host-bound`), so a fresh launch uses a fresh instance and host id. A file that cannot be written is a 503 `state-write-failed`
//! and the tokens stay as they were. The body is validated like the mint's (400).

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use js_agent_wire::InstanceIdentity;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::agents::Agents;
use crate::api::{BODY_LIMIT, Shared, authenticate};
use crate::join_tokens::{DEFAULT_TTL, MAX_TTL, MintError, RevokeError};
use crate::util::blocking;
use crate::wire::{ApiError, parse};

/// The route, behind the client token and the body cap of every other route.
pub(crate) fn router(shared: Arc<Shared>, agents: Arc<Agents>) -> Router {
    Router::new()
        .route("/v1/agent/join-tokens", post(mint).delete(revoke))
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

/// The body of a revoke request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeBody {
    provider: String,
    instance_id: String,
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
        MintError::NoRandom => {
            ApiError::coded(StatusCode::SERVICE_UNAVAILABLE, "token-unavailable")
        }
        MintError::Full => ApiError::coded(StatusCode::TOO_MANY_REQUESTS, "too-many-join-tokens"),
        MintError::Unsaved => {
            ApiError::coded(StatusCode::INTERNAL_SERVER_ERROR, "state-write-failed")
        }
    }
}

/// The instance a request names, or a 400 when the ids are not valid.
fn instance_of(provider: String, instance_id: String) -> Result<InstanceIdentity, ApiError> {
    let instance = InstanceIdentity {
        provider,
        instance_id,
    };
    instance.validate().map_err(|_| {
        ApiError::bad_request("provider or instance_id is not a valid provider id or instance id")
    })?;
    Ok(instance)
}

/// A 400 naming `provider` when the pool admits one provider and `asked` is another: a
/// token for a host the pool would name differently never admits it.
fn check_provider(only: Option<&str>, asked: &str) -> Result<(), ApiError> {
    match only {
        Some(pool) if pool != asked => Err(ApiError::bad_request(format!(
            "provider must be the pool's name, {pool:?}"
        ))),
        _ => Ok(()),
    }
}

async fn mint(State(agents): State<Arc<Agents>>, body: Bytes) -> Result<Response, ApiError> {
    let body: MintBody = parse(&body)?;
    let ttl = lifetime(body.ttl_seconds)?;
    check_provider(agents.mint_provider.as_deref(), &body.provider)?;
    let instance = instance_of(body.provider, body.instance_id)?;
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
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((StatusCode::CREATED, no_store, Json(reply)).into_response())
}

async fn revoke(State(agents): State<Arc<Agents>>, body: Bytes) -> Result<Response, ApiError> {
    let body: RevokeBody = parse(&body)?;
    let instance = instance_of(body.provider, body.instance_id)?;
    let named = (instance.provider.clone(), instance.instance_id.clone());
    let revoked = blocking(move || agents.joins.revoke(&instance)).await;
    let dropped = revoked.map_err(|RevokeError::Unsaved| {
        warn!("join tokens not revoked");
        ApiError::coded(StatusCode::SERVICE_UNAVAILABLE, "state-write-failed")
    })?;
    info!(provider = %named.0, instance = %named.1, dropped, "join tokens revoked");
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool that names its provider accepts that name only, and a 400 says `provider`;
    /// a server with no such pool accepts any. It catches a mint for another pool's name.
    #[test]
    fn the_provider_must_be_the_pools_name_when_it_has_one() {
        assert!(check_provider(Some("burst"), "burst").is_ok());
        assert!(check_provider(None, "lambdalabs").is_ok());
        let err = check_provider(Some("burst"), "lambdalabs").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(format!("{err:?}").contains("provider must be"));
    }

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
