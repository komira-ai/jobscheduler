//! The routes, the authentication in front of them, and the state they share.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use js_lease::{AcquireError, LeaseTable, ReleaseOutcome, TickReport};
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::adapt::{BoxedProvider, SharedClock};
use crate::config::Token;
use crate::wire::{
    AcquireBody, ApiError, Granted, LeaseList, Pending, Released, RenewBody, Renewed, parse,
    parse_id,
};

/// The largest request body served.
pub const BODY_LIMIT: usize = 64 * 1024;

/// The lease table over the server's provider and clock.
pub(crate) type Table = LeaseTable<BoxedProvider, SharedClock>;

/// What every request handler and the tick loop share: the table behind its one lock,
/// the token, and the count of lease-file writes that failed.
pub(crate) struct Shared {
    table: Mutex<Table>,
    token: Token,
    save_errors: AtomicU64,
}

impl Shared {
    pub(crate) fn new(table: Table, token: Token) -> Self {
        Self {
            table: Mutex::new(table),
            token,
            save_errors: AtomicU64::new(0),
        }
    }

    /// How many ticks failed to write the lease file.
    pub(crate) fn save_errors(&self) -> u64 {
        self.save_errors.load(Ordering::SeqCst)
    }

    /// Ticks the table, logs what changed and counts a failed write. The lock is held
    /// for the table call alone.
    pub(crate) async fn tick(&self) -> TickReport {
        let report = self.table.lock().await.tick();
        self.record(&report);
        report
    }

    fn record(&self, report: &TickReport) {
        if report.changed() {
            info!(
                expired = report.expired.len(),
                freed = report.freed.len(),
                confirmed = report.confirmed.len(),
                dropped = report.dropped.len(),
                pending = report.pending,
                releasing = report.releasing,
                "tick changed leases"
            );
        }
        if let Some(err) = &report.save_error {
            self.save_errors.fetch_add(1, Ordering::SeqCst);
            error!(error = %err, "cannot write the lease file");
        }
    }
}

/// The router: `/healthz` open, everything else behind the bearer token.
pub(crate) fn router(shared: Arc<Shared>) -> Router {
    let guarded = Router::new()
        .route("/v1/leases", post(acquire).get(list))
        .route("/v1/leases/{id}/renew", post(renew))
        .route("/v1/leases/{id}", delete(release))
        .fallback(no_route)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(from_fn_with_state(shared.clone(), authenticate))
        .with_state(shared);
    Router::new().route("/healthz", get(healthz)).merge(guarded)
}

/// Refuses a request that lacks `Authorization: Bearer <token>`, with a 401 that says
/// nothing else.
async fn authenticate(State(shared): State<Arc<Shared>>, request: Request, next: Next) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match given.is_some_and(|given| shared.token.matches(given.as_bytes())) {
        true => next.run(request).await,
        false => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn no_route() -> ApiError {
    ApiError::no_route()
}

fn json<T: Serialize>(status: StatusCode, body: &T) -> Response {
    (status, axum::Json(body)).into_response()
}

async fn acquire(State(shared): State<Arc<Shared>>, body: Bytes) -> Result<Response, ApiError> {
    let (request, ttl) = parse::<AcquireBody>(&body)?.into_parts()?;
    let outcome = shared.table.lock().await.acquire(request, ttl);
    match outcome {
        Ok(lease) => {
            let granted = Granted {
                id: lease.id.to_string(),
                host: lease.host.to_string(),
                expires_at_ms: lease.expires_at.millis(),
            };
            Ok(json(StatusCode::CREATED, &granted))
        }
        Err(AcquireError::Unconfirmed(id)) => {
            let pending = Pending {
                id: id.to_string(),
                state: "pending",
            };
            Ok(json(StatusCode::ACCEPTED, &pending))
        }
        Err(err) => Err(err.into()),
    }
}

async fn renew(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = parse_id(&id).ok_or_else(ApiError::unknown_lease)?;
    let ask = match body.is_empty() {
        true => RenewBody::default(),
        false => parse::<RenewBody>(&body)?,
    };
    let ttl = ask.ttl_seconds.map(std::time::Duration::from_secs);
    let expires_at = shared.table.lock().await.renew(&id, ttl)?;
    let renewed = Renewed {
        expires_at_ms: expires_at.millis(),
    };
    Ok(json(StatusCode::OK, &renewed))
}

async fn release(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id).ok_or_else(ApiError::unknown_lease)?;
    let outcome = shared.table.lock().await.release(&id)?;
    Ok(match outcome {
        ReleaseOutcome::Freed => json(StatusCode::OK, &Released { state: "freed" }),
        ReleaseOutcome::Releasing => json(StatusCode::ACCEPTED, &Released { state: "releasing" }),
    })
}

async fn list(State(shared): State<Arc<Shared>>) -> Response {
    let leases = shared.table.lock().await.list();
    let leases = leases.into_iter().map(Into::into).collect();
    json(StatusCode::OK, &LeaseList { leases })
}
