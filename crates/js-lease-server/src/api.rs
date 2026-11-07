//! The routes, the authentication in front of them, and the state they share.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};

use axum::Extension;
use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use js_lease::{LeaseTable, TickReport, Timings};
use js_registry::{Registry, WholeCounts};
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::adapt::{BoxedPool, PoolHosts, SharedClock};
use crate::admit::Admit;
use crate::agent_api;
use crate::agents::Agents;
use crate::config::{Token, is_private_address};
use crate::hosts::listing;
use crate::join_api;
use crate::reservations::{self, Handed};
use crate::scaler::Scaler;
use crate::status::{self, Status};
use crate::store::{LockLost, ServerStore};
use crate::store_health::{StoreHealth, TickStore};
use crate::usage::ProfileCurves;
use crate::usage_api;
use crate::usage_hub::{Usage, UsageCounters};
use crate::wire::ApiError;

/// The largest request body served.
pub const BODY_LIMIT: usize = 64 * 1024;

/// The lease table over the server's provider and clock.
pub(crate) type Table = LeaseTable<BoxedPool, SharedClock>;

/// What every request handler and the tick loop share: the lease table behind its one lock
/// (the restored leases the tick still ends), the token, the count of lease-file writes that
/// failed, the health of the store paths, and the agent side when agents register hosts.
pub(crate) struct Shared {
    table: Mutex<Table>,
    /// The hosts of the table's pool, which the host listing reads without the table's lock.
    hosts: PoolHosts,
    /// How long the table's saves took.
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
    /// The pool's launching machine providers, which a submitted job is admitted over.
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
            hosts: table.provider().hosts(),
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

    /// The same state admitting jobs over `admit`'s launching providers as well as the
    /// hosts; without it only the hosts can admit.
    #[must_use]
    pub(crate) fn with_admit(mut self, admit: Admit) -> Self {
        self.admit = admit;
        self
    }

    /// The pool's launching providers, which a submitted job is admitted over.
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

    /// The lease table behind its lock. A caller that also needs the registry takes this
    /// first and the registry's lock second.
    #[cfg(test)]
    pub(crate) fn table(&self) -> &Mutex<Table> {
        &self.table
    }

    /// The hosts of the table's pool, read without the table's lock: what the host listing
    /// reads ([`crate::hosts::listing`]).
    pub(crate) fn hosts(&self) -> &PoolHosts {
        &self.hosts
    }

    /// The record of the table's saves and of the lock waits.
    pub(crate) fn timings(&self) -> Timings {
        self.timings.clone()
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
    /// over the queued jobs ([`Agents::place_jobs`]), after which the binding of each host a
    /// host job tainted is written ([`Agents::keep_taints`]): the order of design section 6.1. The
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
                // answer read what the tick left.
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
            let tainted = crate::util::blocking(move || placing.place_jobs()).await;
            // A host job the pass placed has tainted its host; the binding is written now.
            agents.keep_taints(tainted).await;
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

/// The router of the host listing, join, brake, usage and status routes: `/healthz` open,
/// everything else behind the bearer token. With `with_agents` the agent routes are served too (one port
/// for everything, as without `server.agent_listen`); without it they are served by
/// [`agent_router`] on a port of their own and answer 404 here.
pub(crate) fn router(shared: Arc<Shared>, with_agents: bool) -> Router {
    let guarded = Router::new()
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
            let app = app.merge(join_api::router(shared.clone(), agents.clone()));
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
/// serves nothing else: a host listing or operator route answers 404 here. Without agent
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

/// Every host the server leases: the static pool's, and the registered ones with the
/// state of their session. The table's lock is not taken: the pool's is taken first and
/// dropped, then the registry's; the order of the two never reverses.
async fn hosts(State(shared): State<Arc<Shared>>) -> Response {
    json(StatusCode::OK, &listing(&shared).await.0)
}

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

    /// Asserts that `GET /v1/hosts`, a route behind the bearer token, answers 401 with no
    /// header, with the pool's join token, with another token and with a header that is
    /// no bearer, and 200 with the client token. It catches a guarded route that trusts an
    /// agent credential.
    #[tokio::test]
    async fn a_guarded_route_takes_the_client_token_alone() {
        use axum::body::Body;
        use tower::ServiceExt;

        let rig = whole_view_tests::Rig::new("guarded_token").await;
        let client = "c".repeat(40);
        let other = "d".repeat(40);
        let ask = |authorization: Option<String>| {
            let rig = &rig;
            async move {
                let mut request = axum::http::Request::get("/v1/hosts");
                if let Some(value) = authorization {
                    request = request.header(header::AUTHORIZATION, value);
                }
                router(rig.shared.clone(), true)
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status()
            }
        };
        for refused in [
            None,
            Some("Bearer t".to_owned()),
            Some(format!("Bearer {other}")),
            Some(format!("Basic {client}")),
            Some(client.clone()),
        ] {
            let status = ask(refused.clone()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused:?}");
        }
        assert_eq!(ask(Some(format!("Bearer {client}"))).await, StatusCode::OK);
    }
}
