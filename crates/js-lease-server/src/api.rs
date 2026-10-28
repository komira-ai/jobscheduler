//! The routes, the authentication in front of them, and the state they share.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use js_lease::{AcquireError, LeaseTable, ReleaseOutcome, TickReport};
use js_provider::Provider;
use js_registry::{Registry, WholeCounts};
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::adapt::{BoxedProvider, SharedClock};
use crate::agent_api;
use crate::agents::Agents;
use crate::config::Token;
use crate::hosts::HostList;
use crate::run_api;
use crate::wire::{
    AcquireBody, ApiError, Granted, LeaseList, Pending, Released, RenewBody, Renewed, parse,
    parse_id,
};

/// The largest request body served.
pub const BODY_LIMIT: usize = 64 * 1024;

/// The lease table over the server's provider and clock.
pub(crate) type Table = LeaseTable<BoxedProvider, SharedClock>;

/// What every request handler and the tick loop share: the table behind its one lock,
/// the token, the count of lease-file writes that failed, and the agent side when agents
/// register hosts.
pub(crate) struct Shared {
    table: Mutex<Table>,
    token: Token,
    agents: Option<Arc<Agents>>,
    save_errors: AtomicU64,
    ticking: AtomicBool,
    /// The whole-node counts the last tick logged.
    counted: std::sync::Mutex<WholeCounts>,
}

impl Shared {
    pub(crate) fn new(table: Table, token: Token, agents: Option<Arc<Agents>>) -> Self {
        Self {
            table: Mutex::new(table),
            token,
            agents,
            save_errors: AtomicU64::new(0),
            ticking: AtomicBool::new(true),
            counted: std::sync::Mutex::new(WholeCounts::default()),
        }
    }

    /// The lease table behind its lock. A handler that also needs the registry takes this
    /// first and the registry's lock second, as every caller does.
    pub(crate) fn table(&self) -> &Mutex<Table> {
        &self.table
    }

    /// A handle on the agent registry; `None` without agent flags.
    pub(crate) fn registry(&self) -> Option<Registry> {
        self.agents.as_ref().map(|agents| agents.registry.clone())
    }

    /// How many times the agent side could not read or write its files; 0 without agents.
    pub(crate) fn agent_errors(&self) -> u64 {
        self.agents.as_ref().map_or(0, |agents| agents.errors())
    }

    /// Whether the tick loop is running.
    pub(crate) fn ticking(&self) -> bool {
        self.ticking.load(Ordering::SeqCst)
    }

    /// Records that the tick loop has ended.
    pub(crate) fn stop_ticking(&self) {
        self.ticking.store(false, Ordering::SeqCst);
    }

    /// How many ticks failed to write the lease file.
    pub(crate) fn save_errors(&self) -> u64 {
        self.save_errors.load(Ordering::SeqCst)
    }

    /// Ticks the table, then advances the agent registry (when the server has one) so its
    /// reservations and prepare deadlines act on this tick, logs what changed and counts
    /// a failed write. The lock is held for the table call alone.
    pub(crate) async fn tick(&self) -> TickReport {
        let report = self.table.lock().await.tick();
        let counts = match &self.agents {
            Some(agents) => {
                // The marks are brought up to this tick, so the counts and the next
                // renewal read what the tick left.
                agents.registry.advance();
                agents.registry.whole_counts()
            }
            None => WholeCounts::default(),
        };
        self.record(&report, counts);
        if let Some(agents) = &self.agents {
            agents.refresh().await;
        }
        report
    }

    fn record(&self, report: &TickReport, counts: WholeCounts) {
        let moved = std::mem::replace(
            &mut *self.counted.lock().unwrap_or_else(PoisonError::into_inner),
            counts,
        ) != counts;
        if report.changed() || moved {
            info!(
                expired = report.expired.len(),
                freed = report.freed.len(),
                confirmed = report.confirmed.len(),
                dropped = report.dropped.len(),
                pending = report.pending,
                releasing = report.releasing,
                reserved = counts.reserved,
                held = counts.held,
                queued = counts.queued,
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
        .route("/v1/hosts", get(hosts))
        .fallback(no_route)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(from_fn_with_state(shared.clone(), authenticate))
        .with_state(shared.clone());
    let open = Router::new()
        .route("/healthz", get(healthz))
        .with_state(shared.clone());
    let app = open.merge(guarded);
    match &shared.agents {
        Some(agents) => app
            .merge(agent_api::router(agents.clone()))
            .merge(run_api::router(shared.clone(), agents.clone())),
        None => app,
    }
}

/// Refuses a request that lacks `Authorization: Bearer <token>`, with a 401 that says
/// nothing else.
pub(crate) async fn authenticate(
    State(shared): State<Arc<Shared>>,
    request: Request,
    next: Next,
) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer);
    match given.is_some_and(|given| shared.token.matches(given.as_bytes())) {
        true => next.run(request).await,
        false => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// The token of an `Authorization` value of the form `Bearer <token>`; the scheme is
/// matched without regard to case (RFC 9110 section 11.1).
pub(crate) fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("Bearer").then_some(token)
}

/// 200 `ok` while the tick loop runs; 503 once it has stopped, since a server that no
/// longer ticks expires nothing and a load balancer should stop sending it work.
async fn healthz(State(shared): State<Arc<Shared>>) -> Response {
    match shared.ticking() {
        true => "ok".into_response(),
        false => (StatusCode::SERVICE_UNAVAILABLE, "tick loop stopped").into_response(),
    }
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
    // The table's lock is held while the registry is asked, as the lock order says.
    let mut table = shared.table.lock().await;
    let expires_at = table.renew(&id, ttl)?;
    let host_reserved = shared.agents.as_ref().is_some_and(|agents| {
        let host = agents.registry.host_of(&id);
        host.is_some_and(|host| agents.registry.is_reserved(&host))
    });
    drop(table);
    let renewed = Renewed {
        expires_at_ms: expires_at.millis(),
        host_reserved,
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

/// Every host the server leases: the static pool's, and the registered ones with the
/// state of their session. The table's lock is taken first and dropped, then the
/// registry's; the order of the two never reverses.
async fn hosts(State(shared): State<Arc<Shared>>) -> Response {
    let capacity = shared.table.lock().await.provider().capacity();
    let list = match &shared.agents {
        Some(agents) => {
            let reserved = agents.registry.reserved();
            HostList::of(capacity, agents.registry.hosts(), Some(&reserved))
        }
        None => HostList::of(capacity, Vec::new(), None),
    };
    json(StatusCode::OK, &list)
}

#[cfg(test)]
mod whole_view_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that the Bearer scheme is read in any letter case, that the token is
    /// what follows the first space, and that another scheme or no space is no token.
    #[test]
    fn the_bearer_scheme_is_case_insensitive() {
        for scheme in ["Bearer", "bearer", "BEARER", "bEaReR"] {
            assert_eq!(bearer(&format!("{scheme} abc")), Some("abc"));
        }
        assert_eq!(bearer("Bearer a b"), Some("a b"));
        assert_eq!(bearer("Bearer "), Some(""));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearerabc"), None);
        assert_eq!(bearer(""), None);
    }
}
