//! The cell's server: the real lease server in process ([`InProcess`]), its registry, lease
//! table, scaler and placement, as one kernel node.
//!
//! [`ServerNode`] owns the server, a current-thread tokio runtime that runs each request
//! and tick to its end, and the [`ManualClock`] behind the server's [`SharedClock`]. The
//! kernel is the only clock: before every input the node sets that clock to the input's
//! virtual time, counted from [`EPOCH_MS`]. A [`Msg::Request`] is answered by the agent
//! router, run to completion with `block_on`, and its answer queued as a [`Msg::Reply`]
//! to the sender. A kernel timer calls [`InProcess::tick`] every tick of virtual time
//! (`--tick`, 5 s), the first one tick after the node starts.
//!
//! The server runs some of its work on tokio's blocking pool (`spawn_blocking`), on
//! another thread; the node awaits each request and each tick to its end before it takes
//! its next input, so that thread's timing cannot reach a decision.
//!
//! [`ServerNode::client_call`] is how a client reaches the server: a call at an exact
//! virtual time, through the client router, which the bus never drops.
//! Every server also serves the job service, in process ([`JOBS_PATH`]): a client
//! call whose path is under it goes to the job service's router (its HTTP/JSON gateway),
//! and every other to the client router.
//!
//! # Ids and the job store's clock
//!
//! The server's job store mints its allocation ids from js-jobs's scripted source over ids
//! drawn from the cell's seed, and stamps its rows with the cell's clock, both given
//! through the seam (`Config::with_job_sources`); the node keeps the one source across a
//! restart. So a seed replays with the job service on too (module `ids`).
//!
//! The lease file, the pool file, the token files and the store file live in a [`TestDir`]
//! the node owns.
//!
//! # The store
//!
//! The server keeps its host bindings, its join tokens and its scaler's records in the store
//! `--store` names (`server.store` for an elastic pool's server), which the node makes when
//! it starts, as [`ServerSpec::store`] says (module `store`): a SQLite file in its directory,
//! or a Postgres database of its own (feature `postgres`), dropped with the node. The node
//! keeps the store across a restart, and a second server started beside it
//! ([`ServerNode::start_beside`]) is handed the same store file line. While the server is
//! down the cell may read the store's keys ([`ServerNode::stored`]).
//!
//! # An elastic pool
//!
//! A server of [`ServerSpec::elastic_pool`] reads its configuration from a file, with the
//! cell's provider kind over the pool's [`Cloud`], which the node makes at its start and
//! keeps across a restart, with the scaler's entropy drawn from the seed
//! ([`SeededEntropy`]). Before every input, beside the server's clock, the node advances
//! the cloud's clock to the input's virtual time; after every tick it reads whether the
//! pool's brake has tripped (`Scaler::state`), the one read of the scaler's state the cell
//! makes, and notes it with the cloud's calls.
//!
//! # A restart
//!
//! [`ServerNode::restart`] stops the server and starts it again over the same directory and
//! store, as an operator restarts the process: it drops the routers and the in-process
//! server ([`InProcess::shutdown`]), which unlocks the lease file and gives up the store's
//! writer lock, and starts a new one with `--boot resume` on the same clock, keeping the
//! node, its runtime, its tick and its count of calls. What the new server holds is what the
//! old one wrote. A server that refuses to start again leaves the node down: a call from an
//! agent goes unanswered, a client's call is answered 503, a tick does nothing, and the
//! listing is empty, until a restart succeeds. [`ServerNode::stop`] leaves it down the same
//! way, as a process killed between two inputs; so does a tick in which the elastic pool's
//! cloud was told to crash the server at an acquire ([`Cloud::crash_at_next_acquire`]).

use std::collections::BTreeMap;
use std::time::Duration;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{Method, Request, Response, StatusCode};
use js_jobs::ScriptedIds;
use js_lease::{Clock as _, ManualClock, Timestamp};
use js_lease_server::in_process::InProcess;
use js_lease_server::{Config, SharedClock, StartupError};
use js_registry::{HostMark, HostState, HostView, Registry, SessionSource};
use js_sim::{Event, NodeId, NodeInput, Output};
use js_testkit::dir::TestDir;
use js_types::HostId;
use js_types::sched::{Effect, FarmTime, StateMachine};
use tokio::runtime::Runtime;
use tower::ServiceExt;

use crate::bus::{Msg, Route};
use crate::elastic::{Cloud, SeededEntropy};
use crate::observe::{ServerHost, ServerView};

mod ids;
mod spec;
mod store;

pub use ids::{SCRIPT_LEN, StoreClock, seeded};
pub use spec::{
    ARCH, ARCH_KEY, CLIENT_TOKEN, JOIN_TOKEN, POOL_CLASS, ServerSpec, Shape, WHOLE_CLASS,
};
use store::StoreAt;
pub use store::{SQLITE_FILE, STORE_LINE_FILE, Store};

/// The wall time, in milliseconds since the Unix epoch, that virtual time zero stands
/// for: 2027-01-01 00:00 UTC. Every clock of the cell counts from it.
pub const EPOCH_MS: u64 = 1_798_761_600_000;

/// The wall time virtual time `at` stands for.
#[must_use]
pub fn wall_time(at: FarmTime) -> Timestamp {
    Timestamp::from_millis(EPOCH_MS.saturating_add(at.as_millis()))
}

/// The virtual time wall time `millis` (on the server's clock) stands for; a wall time
/// before [`EPOCH_MS`] reads as virtual time zero.
#[must_use]
pub fn virtual_time(millis: u64) -> FarmTime {
    FarmTime::from_millis(millis.saturating_sub(EPOCH_MS))
}

/// The path the job service's routes are under: a client call to it goes to the job
/// service's router.
pub const JOBS_PATH: &str = "/v1/jobs";

/// A kind of call the server answered: who called, on which route, and the status.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CallKey {
    from: NodeId,
    route: Route,
    status: u16,
}

/// The server while it runs: its routers (the job service's, while it is on) and the
/// in-process server they serve.
struct Up {
    agent: Router,
    client: Router,
    job: Option<Router>,
    server: InProcess,
}

impl Up {
    /// Starts the server `config` describes on `clock`, inside `runtime`, its job store
    /// minting from `ids` and stamping with `clock`.
    fn start(
        config: Config,
        clock: &ManualClock,
        ids: &Arc<ScriptedIds>,
        runtime: &Runtime,
    ) -> Result<Self, StartupError> {
        let store_clock = Arc::new(StoreClock(clock.clone()));
        let config = config.with_job_sources(store_clock, ids.clone());
        let server = {
            let _entered = runtime.enter();
            InProcess::start(config, SharedClock::new(clock.clone()))?
        };
        Ok(Self {
            agent: server.agent_router(),
            client: server.client_router(),
            job: server.job_router(),
            server,
        })
    }

    /// Drops the routers, then the server: the lease file and the job store are unlocked
    /// once this returns.
    fn shutdown(self) {
        let Self {
            agent,
            client,
            job,
            server,
        } = self;
        drop((agent, client, job));
        server.shutdown();
    }

    /// The router a client call to `path` goes to: the job service's for a path under
    /// [`JOBS_PATH`] while it is on, else the client router.
    fn router_for(&self, path: &str) -> Router {
        let job = self.job.as_ref().filter(|_| path.starts_with(JOBS_PATH));
        job.unwrap_or(&self.client).clone()
    }
}

/// The real lease server as a kernel node.
pub struct ServerNode {
    /// The server, or `None` while it is down: after a restart it refused.
    up: Option<Up>,
    runtime: Runtime,
    clock: ManualClock,
    tick: Duration,
    out: Vec<Output<Msg>>,
    calls: BTreeMap<CallKey, u64>,
    /// The ids the job store mints, kept across a restart.
    ids: Arc<ScriptedIds>,
    /// The elastic pool's cloud, for a server of [`ServerSpec::elastic_pool`], kept across
    /// a restart.
    cloud: Option<Cloud>,
    /// What the scaler draws its acquire ids from, kept across a restart.
    entropy: Arc<dyn SessionSource>,
    /// The store the server keeps its state in, kept across a restart; dropped after the
    /// server, which holds it.
    store: StoreAt,
    /// Dropped last: the server's files, which a restart starts over.
    dir: TestDir,
}

impl ServerNode {
    /// Starts the server `spec` describes, its files in `dir`, at virtual time zero, its
    /// job store minting the ids of seed 0.
    ///
    /// # Errors
    ///
    /// When a file cannot be written, the runtime cannot be built, or the server refuses
    /// its configuration, naming the flag at fault.
    pub fn start(spec: &ServerSpec, dir: TestDir) -> Result<Self, StartupError> {
        Self::start_seeded(spec, dir, 0)
    }

    /// [`ServerNode::start`], its job store minting the ids of `seed` ([`seeded`]).
    ///
    /// # Errors
    ///
    /// As [`ServerNode::start`].
    pub fn start_seeded(spec: &ServerSpec, dir: TestDir, seed: u64) -> Result<Self, StartupError> {
        let store = StoreAt::make(spec.kept_in(), dir.path());
        Self::boot(spec, dir, seed, store, FarmTime::default())
    }

    /// A second server node, as [`ServerNode::start_seeded`] starts one, its files in `dir`,
    /// over this node's store and at this node's virtual time: a second process an operator
    /// starts on the same store. The store's writer lock decides whether it starts.
    ///
    /// # Errors
    ///
    /// As [`ServerNode::start`]: while this node's server holds the store's writer lock, the
    /// refusal of `--store` that names the lock.
    pub fn start_beside(
        &self,
        spec: &ServerSpec,
        dir: TestDir,
        seed: u64,
    ) -> Result<Self, StartupError> {
        let now = virtual_time(self.clock.now().millis());
        Self::boot(spec, dir, seed, self.store.clone(), now)
    }

    /// Starts the server `spec` describes over `store`, its files in `dir`, at virtual time
    /// `now`, its job store minting the ids of `seed`.
    fn boot(
        spec: &ServerSpec,
        dir: TestDir,
        seed: u64,
        store: StoreAt,
        now: FarmTime,
    ) -> Result<Self, StartupError> {
        let clock = ManualClock::new(wall_time(now));
        let cloud = spec.elastic().map(|pool| Cloud::new(pool, spec.shape()));
        let entropy: Arc<dyn SessionSource> = Arc::new(SeededEntropy::new(seed));
        let config = configure(spec, dir.path(), &clock, cloud.as_ref(), &entropy, &store)?;
        let tick = config.tick_period();
        let mut build = tokio::runtime::Builder::new_current_thread();
        let runtime = build.build().map_err(StartupError::because("runtime"))?;
        let ids = Arc::new(seeded(seed));
        let up = Up::start(config, &clock, &ids, &runtime)?;
        Ok(Self {
            up: Some(up),
            runtime,
            clock,
            tick,
            out: Vec::new(),
            calls: BTreeMap::new(),
            ids,
            cloud,
            entropy,
            store,
            dir,
        })
    }

    /// Stops the server and starts it again over the same directory, with `spec`'s
    /// flags and `--boot resume`, on the node's clock (see the module doc). Restarting a
    /// node that is down only starts it.
    ///
    /// # Errors
    ///
    /// When the new server refuses to start, naming the flag at fault; the node is then
    /// down.
    pub fn restart(&mut self, spec: &ServerSpec) -> Result<(), StartupError> {
        self.stop();
        let spec = spec.clone().resuming();
        let (cloud, dir) = (self.cloud.as_ref(), self.dir.path());
        let config = configure(&spec, dir, &self.clock, cloud, &self.entropy, &self.store)?;
        self.up = Some(Up::start(config, &self.clock, &self.ids, &self.runtime)?);
        Ok(())
    }

    /// Stops the server, as a process killed between two inputs: it is dropped with no last
    /// tick ([`InProcess::shutdown`]), so what its files and its store keep is what its last
    /// tick or request wrote, and the node is down (as after a refused restart) until
    /// [`ServerNode::restart`]. Stopping a node that is down does nothing.
    pub fn stop(&mut self) {
        if let Some(up) = self.up.take() {
            up.shutdown();
        }
    }

    /// Every key of the store under `prefix` with its value, read while the server is down,
    /// with a store the node opens for the read and closes before this returns (a SQLite
    /// store on the node's clock).
    ///
    /// # Errors
    ///
    /// While the server is up (it holds the store), or when the store cannot be opened or
    /// read, as text.
    pub fn stored(&self, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
        match &self.up {
            Some(_) => Err("the server is up, and holds its store".to_owned()),
            None => self.store.scan(prefix, &self.clock),
        }
    }

    /// The name of the node's Postgres database, for a server of [`Store::Postgres`].
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn database(&self) -> Option<&str> {
        self.store.database()
    }

    /// Whether the server runs: `false` after a restart it refused.
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.up.is_some()
    }

    /// One call through the client router at virtual time `now`, run to its end, or
    /// through the job service's router for a path under [`JOBS_PATH`] while the job
    /// service is on. The answer's body is read as text. A server that is down answers 503
    /// with no body.
    pub fn client_call(&mut self, now: FarmTime, request: Request<Body>) -> Response<String> {
        self.set_clocks(now);
        let Some(up) = &self.up else {
            return down();
        };
        let router = up.router_for(request.uri().path());
        self.runtime.block_on(call(router, request))
    }

    /// The elastic pool's cloud, for a server of [`ServerSpec::elastic_pool`].
    #[must_use]
    pub fn cloud(&self) -> Option<&Cloud> {
        self.cloud.as_ref()
    }

    /// Asks the server's scaler to reset the brake of the elastic pool, as an operator
    /// does, and notes the reset with the cloud's calls; the scaler applies it at its
    /// next tick. `false`, and nothing recorded, for a server that is down or has no
    /// elastic pool.
    pub fn reset_brake(&self) -> bool {
        let scaler = self.server().and_then(InProcess::scaler);
        let (Some(cloud), Some(scaler)) = (&self.cloud, scaler) else {
            return false;
        };
        let reset = scaler.reset_brake(cloud.pool().name());
        cloud.note_reset();
        reset
    }

    /// Sets the server's clock, and the cloud's, to virtual time `now`.
    fn set_clocks(&self, now: FarmTime) {
        self.clock.set(wall_time(now));
        if let Some(cloud) = &self.cloud {
            cloud.advance_to(now);
        }
    }

    /// Notes whether the elastic pool's brake reads tripped, from the scaler's state,
    /// when that changed: read after every tick.
    fn read_brake(&self) {
        let scaler = self.server().and_then(InProcess::scaler);
        if let (Some(cloud), Some(scaler)) = (&self.cloud, scaler) {
            let tripped = scaler.state().records.tripped(cloud.pool().name());
            cloud.note_brake(tripped);
        }
    }

    /// The server: its registry, scaler and counts; `None` while it is down.
    #[must_use]
    pub fn server(&self) -> Option<&InProcess> {
        self.up.as_ref().map(|up| &up.server)
    }

    /// The agent registry; `None` while the server is down, and for a server without
    /// registration, which a cell's server never is.
    #[must_use]
    pub fn registry(&self) -> Option<Registry> {
        self.server().and_then(InProcess::registry)
    }

    /// Every host the registry lists, in id order.
    #[must_use]
    pub fn hosts(&self) -> Vec<HostView> {
        self.registry()
            .map(|registry| registry.hosts())
            .unwrap_or_default()
    }

    /// The registry's listing as the cell observes it, in the registry's order, with the
    /// request each reserved host is set aside for and the end of its tail
    /// ([`Registry::reservations`]).
    #[must_use]
    pub fn view(&self) -> ServerView {
        let reservations = self.registry().map(|r| r.reservations());
        let tails: BTreeMap<HostId, Option<u64>> = reservations
            .unwrap_or_default()
            .into_iter()
            .map(|reservation| (reservation.host, reservation.tail_end))
            .collect();
        let hosts = self.hosts().into_iter().map(|host| {
            let tail_end = tails.get(&host.id).copied().flatten();
            let listed = ServerHost {
                lost: host.state == HostState::Lost,
                leases: host.leases,
                set_aside: host.mark.is_some(),
                leaving: matches!(host.mark, Some(HostMark::Draining | HostMark::Releasing)),
                reserved_for: host.reserved_for.map(|request| request.to_string()),
                tail_end: tail_end.map(virtual_time),
            };
            (host.id, listed)
        });
        ServerView::from_listing(hosts)
    }

    /// How many calls from `from` on `route` were answered `status`.
    #[must_use]
    pub fn answered(&self, from: &NodeId, route: Route, status: u16) -> u64 {
        let key = CallKey {
            from: from.clone(),
            route,
            status,
        };
        self.calls.get(&key).copied().unwrap_or(0)
    }

    /// Answers an agent's call; anything but a call, and any call while the server is
    /// down, is ignored.
    fn on_message(&mut self, from: NodeId, msg: Msg) {
        let (
            Some(up),
            Msg::Request {
                id,
                route,
                bearer,
                body,
            },
        ) = (&self.up, msg)
        else {
            return;
        };
        let request = Request::builder()
            .method(Method::POST)
            .uri(route.path())
            .header(AUTHORIZATION, format!("Bearer {bearer}"))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap_or_default();
        let answer = self.runtime.block_on(call(up.agent.clone(), request));
        let status = answer.status().as_u16();
        let header = answer.headers().get(RETRY_AFTER);
        let seconds = header.and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
        let retry_after = seconds.map(Duration::from_secs);
        let key = CallKey {
            from: from.clone(),
            route,
            status,
        };
        *self.calls.entry(key).or_insert(0) += 1;
        let msg = Msg::Reply {
            id,
            status,
            retry_after,
            body: answer.into_body(),
        };
        self.out.push(Output::Send { to: from, msg });
    }

    /// Arms the next tick.
    fn arm(&mut self) {
        let after = self.tick;
        self.out.push(Output::Timer { after, tag: 0 });
    }
}

impl StateMachine for ServerNode {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        self.set_clocks(input.now);
        match input.event {
            Event::Start => self.arm(),
            Event::Timer { .. } => {
                if let Some(up) = &self.up {
                    self.runtime.block_on(up.server.tick());
                }
                self.read_brake();
                if self.cloud.as_ref().is_some_and(Cloud::take_crashed) {
                    self.stop();
                }
                self.arm();
            }
            Event::Message { from, msg } => self.on_message(from, msg),
        }
        Vec::new()
    }
}

impl ServerNode {
    /// Takes the outputs queued by earlier inputs, oldest first.
    pub(crate) fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }
}

/// The configuration of the server `spec` describes, its files in `dir`, its state in
/// `store`, on `clock`: over `cloud` and drawing the scaler's acquire ids from `entropy` for
/// a node with an elastic pool's cloud, else from the spec's command line.
fn configure(
    spec: &ServerSpec,
    dir: &std::path::Path,
    clock: &ManualClock,
    cloud: Option<&Cloud>,
    entropy: &Arc<dyn SessionSource>,
    store: &StoreAt,
) -> Result<Config, StartupError> {
    match cloud {
        Some(cloud) => spec.elastic_config(dir, clock, cloud, entropy.clone(), store),
        None => spec.config_over(dir, clock, store),
    }
}

/// The answer of a server that is down: 503, with no body.
fn down() -> Response<String> {
    let mut answer = Response::new(String::new());
    *answer.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    answer
}

/// `request` answered by `router`, called as a service, with the body read as text.
async fn call(router: Router, request: Request<Body>) -> Response<String> {
    let Ok(response) = router.oneshot(request).await;
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    Response::from_parts(parts, String::from_utf8_lossy(&bytes).into_owned())
}
