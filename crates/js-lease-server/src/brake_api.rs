//! The operator's reset of a pool's failure brake.
//!
//! ```text
//! POST /v1/pools/{pool}/brake/reset
//!
//! 202 {"pool": "gpu", "reset": "requested"}
//! ```
//!
//! After enough failed boots in a row (`boot_fail_trip_after`) the scaler puts a pool in
//! `plan`, raises the alert `brake:<pool>` and keeps it there until a person asks for this.
//! Time does not clear it. The route is behind the client token, like the lease routes and
//! the join-token routes: the server has no operator credential of its own, and whoever may
//! admit machines may re-enable the pool that launches them. A pool the server does not have
//! is a 404.
//!
//! The reset clears the pool's failure count, its trip and its cooldown at the scaler's next
//! tick, and that tick writes the records. The 202 means the request is queued, not done:
//! `GET /v1/pools` shows `blocked` no longer `brake` once it has been applied. Asking again
//! is harmless (a reset of a pool with no brake changes nothing), so a request lost to a
//! restart between the 202 and the tick is made good by repeating it.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Serialize;
use tracing::info;

use crate::api::{Shared, authenticate};
use crate::scaler::Scaler;
use crate::wire::ApiError;

/// The route, behind the client token.
pub(crate) fn router(shared: Arc<Shared>, scaler: Scaler) -> Router {
    Router::new()
        .route("/v1/pools/{pool}/brake/reset", post(reset))
        .layer(from_fn_with_state(shared, authenticate))
        .with_state(scaler)
}

/// The 202 body.
#[derive(Serialize)]
struct Requested {
    pool: String,
    reset: &'static str,
}

async fn reset(
    State(scaler): State<Scaler>,
    Path(pool): Path<String>,
) -> Result<Response, ApiError> {
    if scaler.settings().pool != pool {
        return Err(ApiError::coded(StatusCode::NOT_FOUND, "unknown-pool"));
    }
    scaler.reset_brake();
    info!(%pool, "an operator asked to reset the failure brake");
    let body = Requested {
        pool,
        reset: "requested",
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}
