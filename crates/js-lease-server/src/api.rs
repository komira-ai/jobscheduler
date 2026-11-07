//! The routes, the authentication in front of them, and the state they share.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};

use axum::Extension;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use js_lease::{AcquireError, LeaseTable, ReleaseOutcome, TickReport, Timings};
use js_provider::RefusalCode;
use js_registry::{Registry, WholeCounts};
use serde::Serialize;
use tokio::sync::{Mutex, MutexGuard};
use tracing::{error, info};

use crate::adapt::{BoxedPool, SharedClock};
use crate::admit::{Admit, Decision};
use crate::agent_api;
use crate::agents::Agents;
use crate::config::{Token, is_private_address};
use crate::hosts::listing;
use crate::join_api;
use crate::reservations::{self, Handed};
use crate::run_api;
use crate::scaler::Scaler;
use crate::status::{self, Status};
use crate::store::{LockLost, ServerStore};
use crate::store_health::{StoreHealth, TickStore};
use crate::usage::ProfileCurves;
use crate::usage_api;
use crate::usage_hub::{Usage, UsageCounters};
use crate::wire::{
    AcquireBody, ApiError, Granted, LeaseList, Pending, Released, RenewBody, Renewed, parse,
    parse_id,
};

/// The largest request body served.
pub const BODY_LIMIT: usize = 64 * 1024;

/// The lease table over the server's provider and clock.
pub(crate) type Table = LeaseTable<BoxedPool, SharedClock>;

/// What every request handler and the tick loop share: the table behind its one lock,
/// the token, the count of lease-file writes that failed, the health of the store paths, and
/// the agent side when agents register hosts.
pub(crate) struct Shared {
    table: Mutex<Table>,
    /// How long the table's saves and the handlers' waits for its lock took.
    timings: Timings,
    token: Token,
    agents: Option<Arc<Agents>>,
    /// The elastic pool's scaler, for the operator's reset of its failure brake.
    scaler: Option<Scaler>,
    /// What `/v1/pools` and `/status` read; `None` when the listen address is not a
    /// private one and the routes answer as if they did not exist. A request whose peer
    /// is not a private address is answered so too.
    status: Option<Arc<Status>>,
    /// The status of the pools that the job service's `ListPools` and `ListProviders` read,
    /// whatever the listen address: the job port takes a client's token. `None` when the
    /// server was given none.
    pools: Option<Arc<Status>>,
    save_errors: AtomicU64,
    ticking: AtomicBool,
    /// How the tick's store paths fared ([`crate::store_health`]); `None` only in a test
    /// that builds the state without it, where nothing records or reads it.
    store_health: Option<Arc<StoreHealth>>,
    /// The whole-node counts the last tick logged.
    counted: std::sync::Mutex<WholeCounts>,
    /// The reservation ledger the last tick handed the table.
    handed: Handed,
    /// Admission of a lease request: the pool's launching machine providers.
    admit: Admit,
    /// The store, under the writer lock the server holds for its life (`--store`).
    store: Option<ServerStore>,
}

impl Shared {
    pub(crate) fn new(table: Table, token: Token, agents: Option<Arc<Agents>>) -> Self {
        if let Some(agents) = &agents {
            reservations::restore(&agents.registry, &table);
        }
        let handed = Handed::opened_with(&table);
        Self {
            timings: table.timings(),
            table: Mutex::new(table),
            token,
            agents,
            scaler: None,
            status: None,
            pools: None,
            save_errors: AtomicU64::new(0),
            ticking: AtomicBool::new(true),
            store_health: None,
            counted: std::sync::Mutex::new(WholeCounts::default()),
            handed,
            admit: Admit::default(),
            store: None,
        }
    }

    /// The same state admitting lease requests over `admit`'s launching providers as well
    /// as the hosts; without it only the hosts can admit.
    #[must_use]
    pub(crate) fn with_admit(mut self, admit: Admit) -> Self {
        self.admit = admit;
        self
    }

    /// The admission of lease requests: the pool's launching providers, which a submitted
    /// job is admitted over too.
    pub(crate) fn admit(&self) -> &Admit {
        &self.admit
    }

    /// The same state serving the brake reset route for `scaler`.
    #[must_use]
    pub(crate) fn with_scaler(mut self, scaler: Option<Scaler>) -> Self {
        self.scaler = scaler;
        self
    }

    /// The same state holding `store` and its writer lock.
    #[must_use]
    pub(crate) fn with_store(mut self, store: Option<ServerStore>) -> Self {
        self.store = store;
        self
    }

    /// The same state recording the tick's store paths in `health`, which `/healthz` and the
    /// status routes read.
    #[must_use]
    pub(crate) fn with_store_health(mut self, health: Arc<StoreHealth>) -> Self {
        self.store_health = Some(health);
        self
    }

    /// The health of the store paths; `None` when the state was built without it.
    pub(crate) fn store_health(&self) -> Option<&Arc<StoreHealth>> {
        self.store_health.as_ref()
    }

    /// The store the server holds; `None` without a store setting.
    pub(crate) fn server_store(&self) -> Option<&ServerStore> {
        self.store.as_ref()
    }

    /// Whether the store's writer lock is still held; always without a store.
    pub(crate) async fn check_store(&self) -> Result<(), LockLost> {
        match &self.store {
            Some(store) => store.check().await,
            None => Ok(()),
        }
    }

    /// The same state serving the status routes from `status`.
    #[must_use]
    pub(crate) fn with_status(mut self, status: Option<Arc<Status>>) -> Self {
        self.status = status;
        self
    }

    /// The same state with `pools` as the status of the pools the job service reads.
    #[must_use]
    pub(crate) fn with_pools(mut self, pools: Arc<Status>) -> Self {
        self.pools = Some(pools);
        self
    }

    /// The status of the pools the job service reads; `None` when the server was given none.
    pub(crate) fn pools(&self) -> Option<&Arc<Status>> {
        self.pools.as_ref()
    }

    /// The lease table behind its lock. A handler that also needs the registry takes this
    /// first and the registry's lock second, as every caller does.
    pub(crate) fn table(&self) -> &Mutex<Table> {
        &self.table
    }

    /// The record of the table's saves and of the lock waits.
    pub(crate) fn timings(&self) -> Timings {
        self.timings.clone()
    }

    /// The table's lock, with the wait for it timed under `operation`.
    async fn lock(&self, operation: &'static str) -> MutexGuard<'_, Table> {
        let waiting = self.timings.begin(operation);
        let table = self.table.lock().await;
        waiting.finish();
        table
    }

    /// The agent side; `None` without agent flags.
    pub(crate) fn agents(&self) -> Option<&Arc<Agents>> {
        self.agents.as_ref()
    }

    /// A handle on the agent registry; `None` without agent flags.
    pub(crate) fn registry(&self) -> Option<Registry> {
        self.agents.as_ref().map(|agents| agents.registry.clone())
    }

    /// The usage side; `None` without agents or with `--usage-off`.
    pub(crate) fn usage(&self) -> Option<&Arc<Usage>> {
        self.agents
            .as_ref()
            .and_then(|agents| agents.usage.as_ref())
    }

    /// The learned curves; `None` without a usage side or with a sink that learns none.
    pub(crate) fn usage_curves(&self) -> Option<Arc<ProfileCurves>> {
        self.usage().and_then(|usage| usage.curves())
    }

    /// What the usage side has counted; all zero without one.
    pub(crate) fn usage_counters(&self) -> UsageCounters {
        self.usage()
            .map_or_else(UsageCounters::default, |usage| usage.counters())
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

    /// Whether the state store has said this process is no longer its writer.
    pub(crate) fn is_fenced(&self) -> bool {
        self.agents
            .as_ref()
            .is_some_and(|agents| agents.fence.is_tripped())
    }

    /// Completes once the state store has said this process is no longer its writer
    /// ([`crate::fence`]); never without agents, which hold the only store.
    pub(crate) async fn fenced(&self) {
        match &self.agents {
            Some(agents) => agents.fence.tripped().await,
            None => std::future::pending().await,
        }
    }

    /// How many ticks failed to write the lease file.
    pub(crate) fn save_errors(&self) -> u64 {
        self.save_errors.load(Ordering::SeqCst)
    }

    /// Ticks the table, which writes the leases the tick changed and, when it changed, the
    /// registry's reservation ledger as the tick's stops and presentations left it, in one replace;
    /// then runs the job pass, which ends the jobs that ran out of time, deletes those past their
    /// retention and settles the allocations of hosts read absent ([`Agents::job_pass`]); then
    /// advances the agent registry (when the server has one) so its reservations and prepare
    /// deadlines act on this tick, logs what changed and counts a failed write; then runs a pass
    /// over the queued jobs ([`Agents::place_jobs`]): the order of design section 6.1. The
    /// table's lock is held for the tick alone. How the lease file's write and the job pass
    /// went is recorded as one reading of the store's health ([`StoreHealth::record`]).
    pub(crate) async fn tick(&self) -> TickReport {
        let report = {
            let mut table = self.table.lock().await;
            // The registry's reservations, as the tick's stops and presentations leave
            // them, are written in the same replace as the leases of this tick.
            let handed = &self.handed;
            let agents = self.agents.as_ref();
            table.tick_then(|| agents.and_then(|agents| handed.changed(&agents.registry)))
        };
        let pass = match &self.agents {
            // The job pass reads and writes the job tables, so it runs on the pool for
            // blocking calls; it holds no lock of the table or the registry while it does.
            Some(agents) => {
                let passing = Arc::clone(agents);
                crate::util::blocking(move || passing.job_pass()).await
            }
            None => None,
        };
        let store = TickStore {
            lease_file_failed: report.save_error.is_some(),
            pass,
        };
        self.store_health
            .iter()
            .for_each(|health| health.record(store));
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
            // The tick is a pass over the queued jobs too, whatever else freed room; it reads
            // and writes the job tables, so it runs on the pool for blocking calls.
            let placing = Arc::clone(agents);
            crate::util::blocking(move || placing.place_jobs()).await;
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

/// The router of the client, operator and status routes: `/healthz` open, everything else
/// behind the bearer token. With `with_agents` the agent routes are served too (one port
/// for everything, as without `server.agent_listen`); without it they are served by
/// [`agent_router`] on a port of their own and answer 404 here.
pub(crate) fn router(shared: Arc<Shared>, with_agents: bool) -> Router {
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
        .route("/v1/pools", get(pools))
        .route("/status", get(status_page))
        .with_state(shared.clone());
    let app = open.merge(guarded);
    let app = match &shared.agents {
        Some(agents) => {
            let app = app
                .merge(join_api::router(shared.clone(), agents.clone()))
                .merge(run_api::router(shared.clone(), agents.clone()));
            match with_agents {
                true => app.merge(agent_api::router(agents.clone())),
                false => app,
            }
        }
        None => app,
    };
    let app = match &shared.scaler {
        Some(scaler) => app.merge(crate::brake_api::router(shared.clone(), scaler.clone())),
        None => app,
    };
    match shared.usage() {
        Some(usage) => app.merge(usage_api::router(shared.clone(), usage.clone())),
        None => app,
    }
}

/// The router of the agent routes alone, for the port `server.agent_listen` names. It
/// serves nothing else: a client or operator route answers 404 here. Without agent
/// registration it serves nothing at all.
pub(crate) fn agent_router(shared: &Shared) -> Router {
    match &shared.agents {
        Some(agents) => agent_api::router(agents.clone()),
        None => Router::new(),
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

/// 200 `ok` while the tick loop runs and the store answers; 503 once the loop has stopped
/// (`tick loop stopped`), since a server that no longer ticks expires nothing, or while the
/// store has been failing for the window ([`crate::store_health`]: `store unreachable`),
/// since its heartbeats and job calls fail. A load balancer should stop sending it work. The
/// body's first word names the cause; a stopped loop is named first.
async fn healthz(State(shared): State<Arc<Shared>>) -> Response {
    let unreachable = shared
        .store_health
        .as_ref()
        .is_some_and(|health| health.unreachable());
    match (shared.ticking(), unreachable) {
        (true, false) => "ok".into_response(),
        (false, _) => (StatusCode::SERVICE_UNAVAILABLE, "tick loop stopped").into_response(),
        (true, true) => (StatusCode::SERVICE_UNAVAILABLE, "store unreachable").into_response(),
    }
}

/// The peer of the connection a request came in on, as the listener sets it. Absent when
/// the listener could not name the peer; a status route then refuses.
pub(crate) type Peer = Option<Extension<ConnectInfo<SocketAddr>>>;

/// Whether a peer may read the status pages, which take no token: only a private address
/// ([`is_private_address`]; an IPv4-mapped IPv6 address counts as its IPv4 form). An
/// unknown peer may not.
pub(crate) fn peer_may_read_status(peer: &Peer) -> bool {
    peer.as_ref()
        .is_some_and(|Extension(ConnectInfo(addr))| is_private_address(addr.ip()))
}

impl Shared {
    /// What the status routes read for `peer`: `None` when the listen address is not a
    /// private one or the peer is not.
    fn status_for(&self, peer: &Peer) -> Option<&Arc<Status>> {
        self.status.as_ref().filter(|_| peer_may_read_status(peer))
    }
}

/// `GET /v1/pools`: the elastic pools as JSON. No token: who may reach it is the
/// network's to say (see [`crate::status`]). Not served, and a 404 like any unknown route,
/// when the server listens on an address that is not private or the peer is not a
/// private address.
async fn pools(State(shared): State<Arc<Shared>>, peer: Peer) -> Response {
    let served = |status: &Arc<Status>| json(StatusCode::OK, &status.view());
    shared.status_for(&peer).map_or_else(not_found, served)
}

/// `GET /status`: the same data as a page.
async fn status_page(State(shared): State<Arc<Shared>>, peer: Peer) -> Response {
    shared
        .status_for(&peer)
        .map_or_else(not_found, page_response)
}

fn page_response(status: &Arc<Status>) -> Response {
    let page = status::page(&status.view(), status.now());
    let headers = [
        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        (header::CACHE_CONTROL, "no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
    ];
    (StatusCode::OK, headers, page).into_response()
}

/// The answer of a status route that is not served: that of a route that does not exist.
pub(crate) fn not_found() -> Response {
    ApiError::no_route().into_response()
}

async fn no_route() -> ApiError {
    ApiError::no_route()
}

fn json<T: Serialize>(status: StatusCode, body: &T) -> Response {
    (status, axum::Json(body)).into_response()
}

async fn acquire(State(shared): State<Arc<Shared>>, body: Bytes) -> Result<Response, ApiError> {
    let body = parse::<AcquireBody>(&body)?;
    let request_id = body.request_id()?;
    let (request, ttl) = body.into_parts()?;
    let registry = shared.registry();
    // Admission first, under the table lock: the hosts, and the catalogs as the cache holds
    // them, which calls no provider (see `admit`).
    let mut table = shared.lock("wait-acquire").await;
    let decision = shared
        .admit
        .decide(&table, registry.as_ref(), request_id.as_ref(), &request);
    let outcome = match decision {
        Decision::Start => match request_id {
            Some(id) => table.acquire_with_id(id, request, ttl),
            None => table.acquire(request, ttl),
        },
        Decision::Refuse => Err(AcquireError::Refused(RefusalCode::NoCapableHost)),
    };
    drop(table);
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
    let mut table = shared.lock("wait-renew").await;
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
    let outcome = {
        let mut table = shared.lock("wait-release").await;
        // A release of a waiting lease cancels its request in the registry: the lease's
        // end and the ledger the stop left are written in one replace, before the answer.
        match &shared.agents {
            Some(agents) => {
                let handed = &shared.handed;
                table.release_then(&id, || Some(handed.take(&agents.registry)))?
            }
            None => table.release(&id)?,
        }
    };
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
    json(StatusCode::OK, &listing(&shared).await.0)
}

#[cfg(test)]
mod probe_tests;
#[cfg(test)]
mod request_id_tests;
#[cfg(test)]
mod status_gate_tests;
#[cfg(test)]
mod status_logs_tests;
#[cfg(test)]
mod whole_restart_tests;
#[cfg(test)]
mod whole_sched_tests;
#[cfg(test)]
pub(crate) mod whole_view_tests;

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
