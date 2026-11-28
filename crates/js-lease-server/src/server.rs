//! Starting the server, running it, and stopping it.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::admit::{Admit, Launching};
use crate::agents::{Agents, assemble_in};
use crate::api::{Shared, agent_router, router};
use crate::callbacks::{self, Deliverers};
use crate::catalogs::{Cached, Catalogs};
use crate::checks::Checker;
use crate::clock::SharedClock;
use crate::config::{
    BootMode, Config, JobListen, Limits, LiveClients, MetricsListen, ReadyListen, StoreSetting,
    TlsFiles, Token,
};
use crate::error::StartupError;
use crate::file::Launch;
use crate::hosts::listing;
use crate::job_api;
use crate::job_sizes;
use crate::listener::{Slots, serve, serve_both};
use crate::listening::{self, Listening};
use crate::metrics::{self, Meter, Recorder};
use crate::pool::{Pool, Setup};
use crate::ready::{self, Gate, Phase};
use crate::scaler::{Parts, Scaler, build};
use crate::status::Status;
use crate::store::ServerStore;
use crate::store_health::{LockState, StoreHealth};
use crate::ticker::{IntervalTicker, Tick, Ticker};
use crate::tls;
use crate::usage::ProfileCurves;
use crate::usage_hub::UsageCounters;
use crate::util::{blocking, or_pending};

#[path = "server_tasks.rs"]
mod tasks;
use tasks::{Checking, Polling, Ticking};

/// A running server. Dropping it stops serving and ticking; [`Running::shutdown`] stops
/// it the orderly way (requests in flight finish, one last tick).
pub struct Running {
    addr: SocketAddr,
    agent: Option<Port>,
    job: Option<Port>,
    metrics: Option<Port>,
    /// The readiness listener (`--ready-listen`), the last listener to close.
    ready: Option<Port>,
    /// The phase the readiness route answers.
    gate: Gate,
    shared: Arc<Shared>,
    stop: Stop,
    slots: Arc<Slots>,
    serving: JoinHandle<()>,
    ticking: Ticking,
    /// The placement poller ([`crate::poller`]), which each submit and heartbeat pokes.
    polling: Polling,
    _checking: Checking,
    /// What other servers of a shared store tell this one ([`crate::listening`]).
    _listening: Option<Listening>,
    scaler: Option<Scaler>,
    tls: Option<tls::Tls>,
    /// How long [`Running::hold`] keeps serving (`--drain-hold`).
    drain_hold: Duration,
    /// The teaching of the job sizes from the ended jobs ([`crate::job_sizes`]), which the
    /// start does not wait for; a stop ends its read at the next page and waits for it, unless
    /// it was taken to be awaited before.
    sizes: Option<JoinHandle<usize>>,
    /// The threads that deliver jobs' completion callbacks ([`crate::callbacks`]), while the
    /// server holds the job tables.
    delivering: Option<Deliverers>,
}

/// A listener beside the one of `--listen`: the agent routes alone (`server.agent_listen`),
/// the job service (`--job-listen`), the metrics (`--metrics-listen`), or the readiness route
/// (`--ready-listen`).
struct Port {
    addr: SocketAddr,
    stop: Stop,
    serving: JoinHandle<()>,
}

impl Port {
    /// Tells the listener to stop; the task to await for its drain.
    fn stop(self) -> JoinHandle<()> {
        self.stop.send();
        self.serving
    }
}

/// The stop of one listener: no new connection from the first [`Stop::send`] on, and each
/// open one closed once it has answered the request it serves ([`crate::listener`]). Sent
/// through a shared reference, so the drain stops the agent routes' listener early
/// ([`Running::hold`]) and the stop after it finds that listener stopping already.
struct Stop(std::sync::Mutex<Option<oneshot::Sender<()>>>);

impl Stop {
    fn new(stop: oneshot::Sender<()>) -> Self {
        Self(std::sync::Mutex::new(Some(stop)))
    }

    /// Whether the listener was told to stop.
    #[cfg(test)]
    fn sent(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
    }

    /// Tells the listener to stop; nothing when it was told before.
    fn send(&self) {
        let mut stop = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(stop) = stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Binds `addr`, or takes over `bound` (a listener the caller bound) when there is one,
/// refusing the start naming `flag` when it cannot.
async fn bind(
    addr: SocketAddr,
    flag: &'static str,
    bound: Option<std::net::TcpListener>,
) -> Result<(TcpListener, SocketAddr), StartupError> {
    let (opened, flag) = match bound {
        Some(std) => (
            std.set_nonblocking(true)
                .and_then(|()| TcpListener::from_std(std)),
            "the listener given by the caller",
        ),
        None => (TcpListener::bind(addr).await, flag),
    };
    let bound = opened.and_then(|listener| listener.local_addr().map(|addr| (listener, addr)));
    bound.map_err(|err| StartupError::new(flag, err))
}

/// Serves the readiness route on `at` (or on `bound`, the listener the caller bound): plain
/// HTTP, answering what `gate` says.
async fn ready_port(
    at: ReadyListen,
    bound: Option<std::net::TcpListener>,
    limits: Limits,
    gate: Gate,
) -> Result<Port, StartupError> {
    let (listener, addr) = bind(at.addr, at.flag, bound).await?;
    let (stop, stopped) = oneshot::channel::<()>();
    let slots = Arc::new(Slots::new(limits.max_connections));
    let app = ready::router(gate);
    let serving = tokio::spawn(serve(listener, None, app, limits, slots, stopped));
    info!(%addr, "serving the readiness route");
    Ok(Port {
        addr,
        stop: Stop::new(stop),
        serving,
    })
}

/// Starts a server: puts the agent side together, binds the
/// listener and begins serving and ticking. The checks of
/// an elastic pool run on a timer of `--tick`; see [`start_with_checks`].
///
/// Time comes from `clock` and ticks from `ticker`, so a test decides both. Each
/// failure to start names the flag at fault.
pub async fn start<T: Ticker>(
    config: Config,
    clock: SharedClock,
    ticker: T,
) -> Result<Running, StartupError> {
    let checks = IntervalTicker::new(config.tick_period());
    start_with_checks(config, clock, ticker, checks).await
}

/// How often a server that waits for a SQLite store's file lock (`--ready-listen`) tries it
/// again.
const LOCK_RETRY: Duration = Duration::from_secs(1);

/// [`start`] with the ticker of the pool checks given too. At each tick of `checks` the
/// check loop of the elastic pool (if the configuration has one) asks the injected clock
/// whether a pass is due, runs it if so, and finishes the tick when the pass is done, so a
/// test steps the checks as it steps the server's tick. Without an elastic pool `checks` is
/// dropped.
pub async fn start_with_checks<T: Ticker, C: Ticker>(
    config: Config,
    clock: SharedClock,
    ticker: T,
    checks: C,
) -> Result<Running, StartupError> {
    let retry = IntervalTicker::new(LOCK_RETRY);
    start_waiting(config, clock, ticker, checks, retry).await
}

/// [`start_with_checks`] with the ticker of the retries of a SQLite store's file lock given
/// too. With `--ready-listen` (`server.ready_listen`) the readiness listener is bound first,
/// before the lock; while another process holds the lock the start waits, logging once that
/// it does and trying again at each tick of `retry`, and the tick is finished when the
/// attempt it asked for is done, so a test steps the retries as it steps the server's tick.
/// A lock that cannot be tried at all still refuses the start. Without `--ready-listen` the
/// lock is tried once and a held lock refuses the start, and `retry` is dropped. A Postgres
/// store has no lock: it opens at once.
pub async fn start_waiting<T: Ticker, C: Ticker, R: Ticker>(
    config: Config,
    clock: SharedClock,
    ticker: T,
    checks: C,
    retry: R,
) -> Result<Running, StartupError> {
    start_held(config, clock, (ticker, checks, retry), None).await
}

/// [`start_waiting`], whose teaching of the job sizes ([`crate::job_sizes`]) waits for `hold`
/// (a test's) to be sent or dropped before it reads; `None` reads at once.
async fn start_held<T: Ticker, C: Ticker, R: Ticker>(
    mut config: Config,
    clock: SharedClock,
    (ticker, checks, retry): (T, C, R),
    hold: Option<oneshot::Receiver<()>>,
) -> Result<Running, StartupError> {
    let logs = config.logs().sink();
    let log_reads = job_api::logs::Ctx::from_config(config.logs(), &clock);
    // The readiness listener answers before anything else is done, so a supervisor sees the
    // server wait for a SQLite store's file lock. It says so only when there is one to wait
    // for.
    let first = match config.store.as_ref().is_some_and(StoreSetting::is_locked) {
        true => Phase::Waiting,
        false => Phase::Starting,
    };
    let gate = Gate::new(first);
    let ready = match config.ready_listen {
        Some(at) => {
            let bound = config.ready_bound.take();
            Some(ready_port(at, bound, config.limits(), gate.clone()).await?)
        }
        None => None,
    };
    // The store before anything else (design 6.7, step 1): a SQLite file's lock is taken
    // before the store is read and before any listener but the readiness one.
    let store_hosts = config.store.as_ref().map(StoreSetting::tcp_hosts);
    let store = match (config.store.take(), &ready) {
        (Some(setting), Some(_)) => Some(ServerStore::open_waiting(setting, retry).await?),
        (Some(setting), None) => Some(ServerStore::open(setting)?),
        (None, _) => None,
    };
    gate.set(Phase::Starting);
    // Over a store other servers share, a periodic duty is claimed for half a tick, so that of
    // the servers whose ticks fall in one period one runs it, and a server whose tick is a
    // little early does not skip a period. A store one process writes has no one to leave it to.
    let duty_period = match store.as_ref().is_some_and(|store| !store.is_locked()) {
        true => config.tick_period() / 2,
        false => Duration::ZERO,
    };
    let (prepared, listeners) = prepare(config, store, &clock)?;
    let tls = listeners
        .tls
        .as_ref()
        .map(|files| tls::Tls::open(files, listeners.agent_client_ca.as_deref(), clock.clone()))
        .transpose()?;
    let acceptor = tls.as_ref().map(tls::Tls::acceptor);
    let (listener, addr) = bind(listeners.listen, listeners.listen_flag, listeners.bound).await?;
    let agent_listener = match listeners.agent_listen {
        Some(agent) => Some(bind(agent, "server.agent_listen", None).await?),
        None => None,
    };
    let job_listener = match listeners.job_listen {
        Some(job) => Some(bind(job.addr, job.flag, None).await?),
        None => None,
    };
    let metrics_listener = match &listeners.metrics {
        Some((metrics, _)) => Some(bind(metrics.addr, metrics.flag, None).await?),
        None => None,
    };
    let job_acceptor = tls.as_ref().map(tls::Tls::job_acceptor);
    let clients = listeners.clients;
    let served_tls = acceptor.is_some();
    let Opened {
        shared,
        scaler,
        checker,
        cached,
    } = open(prepared, &clock)?;
    // The ended jobs are read and taught while the server serves; nothing waits for them.
    let sizes = job_sizes::spawn(&shared, &gate, hold);
    shared.agents().set_duty_period(duty_period);
    let heard = listening::start(shared.server_store(), shared.agents());
    // The name this server claims dispatch messages by: where it serves agents, and its pid.
    let agents_at = agent_listener.as_ref().map_or(addr, |(_, agents)| *agents);
    let claimer = format!("{agents_at}/{}", std::process::id());
    shared.agents().dispatching.set_claimer(claimer.clone());
    let ports = [&agent_listener, &job_listener, &metrics_listener];
    let bound = ports.into_iter().flatten().map(|(_, at)| *at);
    let listening: Vec<SocketAddr> = std::iter::once(addr).chain(bound).collect();
    let delivering = callbacks::start(shared.agents(), &listening, store_hosts, claimer).await;
    info!(
        %addr,
        hosts = listing(&shared).hosts.len(),
        tls = served_tls,
        logs = %logs,
        "listening"
    );
    let limits = listeners.limits;
    let split = agent_listener.is_some();
    // Every listener stops at its own stop.
    let (stop, stopped) = oneshot::channel::<()>();
    let slots = Arc::new(Slots::new(limits.max_connections));
    let serving = serve(
        listener,
        acceptor.clone(),
        router(shared.clone(), !split),
        limits,
        slots.clone(),
        stopped,
    );
    let agent = agent_listener.map(|(listener, addr)| {
        let (stop, stopped) = oneshot::channel::<()>();
        let slots = Arc::new(Slots::new(limits.max_connections));
        let app = agent_router(&shared);
        let serving = serve(listener, acceptor, app, limits, slots, stopped);
        info!(%addr, "listening for agents");
        Port {
            addr,
            stop: Stop::new(stop),
            serving: tokio::spawn(serving),
        }
    });
    // The job service, when it is on: the server holds its one job store exactly when
    // `--job-listen` is given, so the listener and the service are there together.
    let job = job_listener
        .zip(job_api::from_server(
            &shared,
            listeners.max_job_wall,
            log_reads,
        ))
        .map(|((listener, addr), api)| {
            let (stop, stopped) = oneshot::channel::<()>();
            let slots = Arc::new(Slots::new(limits.max_connections));
            let app = job_api::port(api, clients);
            let serving = serve_both(listener, job_acceptor, app, limits, slots, stopped);
            info!(%addr, tls = served_tls, "listening for the job service");
            Port {
                addr,
                stop: Stop::new(stop),
                serving: tokio::spawn(serving),
            }
        });
    // The metrics, when they are on: plain HTTP whatever the other ports speak, since they
    // carry no token, on a private address only (`not_private`).
    let metrics =
        metrics_listener
            .zip(listeners.metrics)
            .map(|((listener, addr), (_, recorder))| {
                let (stop, stopped) = oneshot::channel::<()>();
                let slots = Arc::new(Slots::new(limits.max_connections));
                let app = metrics::router(shared.clone(), recorder);
                let serving = serve(listener, None, app, limits, slots, stopped);
                info!(%addr, "serving the metrics");
                Port {
                    addr,
                    stop: Stop::new(stop),
                    serving: tokio::spawn(serving),
                }
            });
    let ticking = Box::pin(tick_loop(
        shared.clone(),
        ticker,
        scaler.clone().map(|scaler| (scaler, cached)),
    ));
    let ticking = Ticking(tokio::spawn(ticking));
    let (stop_polling, polling_stopped) = oneshot::channel();
    let polling = Polling {
        stop: stop_polling,
        task: Ticking(tokio::spawn(poll_loop(shared.clone(), polling_stopped))),
    };
    let checking = Checking(checker.map(|checker| tokio::spawn(checker.run(checks))));
    gate.set(Phase::Ready);
    Ok(Running {
        addr,
        agent,
        job,
        metrics,
        ready,
        gate,
        shared,
        stop: Stop::new(stop),
        slots,
        serving: tokio::spawn(serving),
        ticking,
        polling,
        _checking: checking,
        _listening: heard,
        scaler,
        tls,
        drain_hold: limits.drain_hold,
        sizes: Some(sizes),
        delivering,
    })
}

/// What only the listeners of a start read of its configuration.
pub(crate) struct Listeners {
    listen: SocketAddr,
    listen_flag: &'static str,
    bound: Option<std::net::TcpListener>,
    agent_listen: Option<SocketAddr>,
    job_listen: Option<JobListen>,
    /// Where the metrics are served, with the recorder that counts their events.
    metrics: Option<(MetricsListen, Arc<Recorder>)>,
    /// The ceiling on a submitted job's wall limit (`--max-job-wall`, `server.max_job_wall`).
    pub(crate) max_job_wall: Duration,
    /// The clients the job service's bearer layer admits, as the tick reloads them.
    pub(crate) clients: LiveClients,
    tls: Option<TlsFiles>,
    agent_client_ca: Option<PathBuf>,
    limits: Limits,
}

/// What a start has built when its listeners are about to be bound: the agent side, the
/// elastic pool and its scaler, with the parts of the configuration that [`open`] reads.
pub(crate) struct Prepared {
    agents: Arc<Agents>,
    pool: Option<Pool>,
    scaler: Option<Scaler>,
    tick: Duration,
    token: Token,
    warnings: Vec<String>,
    status: bool,
    admit: Admit,
    cached: Cached,
    /// The store, when the configuration names one.
    store: Option<ServerStore>,
    /// The health of the store paths, which the tick records and `/healthz` reads.
    store_health: StoreHealth,
}

/// What a start has built once the shared state is made: the state the routes and the tick
/// share, the elastic pool's scaler and its check loop, the cache of the launching
/// providers' catalogs, and the state file's path.
pub(crate) struct Opened {
    pub(crate) shared: Arc<Shared>,
    pub(crate) scaler: Option<Scaler>,
    pub(crate) checker: Option<Checker>,
    pub(crate) cached: Cached,
}

/// The first half of a start, everything before a listener is bound: with the store the
/// caller opened (`store`, `None` when the configuration names none),
/// puts the agent side together and builds the elastic pool and its scaler
/// (which marks the machines its records show as being released). Binds nothing and spawns
/// nothing. No lease file is read: a lease file of an earlier release at the state file's
/// path is left as it is.
pub(crate) fn prepare(
    config: Config,
    store: Option<ServerStore>,
    clock: &SharedClock,
) -> Result<(Prepared, Listeners), StartupError> {
    let Config {
        listen,
        listen_flag,
        bound,
        agent_listen,
        job_listen,
        metrics_listen,
        advertise_url,
        warnings,
        status,
        mut launch_inputs,
        elastic,
        state_file,
        boot,
        tick,
        token,
        clients,
        tls,
        agent_client_ca,
        limits,
        agents,
        usage,
        monotonic,
        entropy,
        alerts,
        alert_repeat,
        logs,
        max_job_wall,
        store_unreachable_after,
        ..
    } = config;
    // Once, the files a server without the setting kept beside the state file: before
    // the store is read for them. A live pool not at its first start needs its records.
    let live = elastic
        .as_ref()
        .is_some_and(|plan| plan.launch == Launch::Live);
    let records_needed = [live, boot != BootMode::Initialise]
        .into_iter()
        .all(|holds| holds);
    if let Some(store) = &store {
        store.import((&state_file, clock), records_needed)?;
    }
    let logs = crate::logs::from_config(&logs, clock);
    let kept = (state_file.as_path(), store.as_ref());
    let agents = assemble_in(agents, usage, clock, kept, logs)?;
    // Every launching provider of the elastic pool: admission asks each whether it could
    // ever serve a job, and the scaler's tick fetches each one's catalog. An elastic plan
    // comes with a pool whose hosts register (agents), so a server with a catalog to
    // fetch always has a scaler.
    let providers = || elastic.iter().flat_map(|plan| plan.providers.iter());
    let launching = providers().map(|each| Launching {
        provider: Arc::clone(&each.provider),
        share: each.share,
        limits: each.limits.clone(),
    });
    let catalogs = providers().fold(Catalogs::new(), |catalogs, each| {
        let provider = Arc::clone(&each.provider);
        catalogs.with(provider, each.catalog_refresh, each.offer_cooldown)
    });
    let cached = Cached::new(Arc::new(catalogs), clock.clone());
    let admit = Admit::new(launching.collect(), cached.clone());
    let elastic = elastic.map(|mut plan| {
        let setup = Setup {
            advertise: advertise_url.clone(),
            tls: tls.clone(),
            inputs: std::mem::take(&mut launch_inputs),
            clock: clock.clone(),
        };
        (Pool::new(&mut plan, setup), plan)
    });
    // Before anything is served: the scaler marks the machines its records show as being
    // released, so a host of theirs that says hello again is admitted already marked.
    let scaler = match &elastic {
        Some((pool, plan)) => {
            let parts = Parts {
                agents: Arc::clone(&agents),
                clock: clock.clone(),
                state_file: &state_file,
                store: store.as_ref(),
                advertise_url: advertise_url.as_deref(),
                first_start: boot == BootMode::Initialise,
                health: pool.health.clone(),
                mode: pool.mode(),
                catalogs: Arc::clone(cached.catalogs()),
                entropy,
            };
            Some(build(plan, parts)?)
        }
        None => None,
    };
    // The store's alert goes to the same sink as the scaler's, through a dedupe of its own.
    let store_health = StoreHealth::new(clock.clone(), store_unreachable_after);
    let store_health = match &alerts {
        Some(sink) => store_health.with_sink(Arc::clone(sink), alert_repeat),
        None => store_health,
    };
    store_health.set_lock(match store.as_ref().is_some_and(ServerStore::is_locked) {
        true => LockState::Held,
        false => LockState::None,
    });
    if let (Some(scaler), Some(sink)) = (&scaler, alerts) {
        scaler.set_sink(sink, alert_repeat);
    }
    // With the metrics on, the agent side and the scaler count their events from here on,
    // before anything is served or ticked.
    let metrics = metrics_listen.map(|at| (at, Arc::new(Recorder::default())));
    if let Some((_, recorder)) = &metrics {
        let meter = Meter::new(recorder.clone(), Arc::clone(&monotonic));
        agents.set_meter(meter.clone());
        scaler
            .iter()
            .for_each(|scaler| scaler.set_meter(meter.clone()));
    }
    let prepared = Prepared {
        agents,
        pool: elastic.map(|(pool, _)| pool),
        scaler,
        tick,
        token,
        warnings,
        status,
        admit,
        cached,
        store,
        store_health,
    };
    let listeners = Listeners {
        listen,
        listen_flag,
        bound,
        agent_listen,
        job_listen,
        metrics,
        max_job_wall,
        clients,
        tls,
        agent_client_ca,
        limits,
    };
    Ok((prepared, listeners))
}

/// The second half of a start, once the listeners are bound (or, in process, once there is
/// nothing to bind): the scaler starts counting the registry's view, and the state the
/// routes and the tick share is built. Spawns nothing.
pub(crate) fn open(prepared: Prepared, clock: &SharedClock) -> Result<Opened, StartupError> {
    let Prepared {
        agents,
        pool,
        scaler,
        tick,
        token,
        warnings,
        status: serves_status,
        admit,
        cached,
        store,
        store_health,
    } = prepared;
    let logs = agents.logs.clone();
    if let Some(scaler) = &scaler {
        scaler.listening(Some(2 * tick));
    }
    for warning in &warnings {
        warn!("{warning}");
    }
    let checker = pool.as_ref().map(|pool| pool.checker(clock.clone()));
    let store_health = Arc::new(store_health);
    let pools = Status::new(pool, scaler.clone(), warnings, clock.clone());
    let pools = Arc::new(
        pools
            .with_logs(logs)
            .with_store_health(Arc::clone(&store_health))
            .with_catalogs(Arc::clone(cached.catalogs())),
    );
    let status = serves_status.then(|| Arc::clone(&pools));
    let shared = Arc::new(
        Shared::new(token, agents)
            .with_admit(admit)
            .with_status(status)
            .with_pools(pools)
            .with_scaler(scaler.clone())
            .with_store(store)
            .with_store_health(store_health),
    );
    Ok(Opened {
        shared,
        scaler,
        checker,
        cached,
    })
}

/// Marks the tick loop as stopped when it is dropped, however it ends: the caller
/// aborting it, or the ticker or the table panicking. `/healthz` reads the mark.
struct Alive(Arc<Shared>);

impl Drop for Alive {
    fn drop(&mut self) {
        self.0.stop_ticking();
    }
}

/// Ticks the server at every tick of `ticker`, and hands each tick to the scaler of the
/// elastic pool when the server has one, with the cache of its provider's catalog, forever;
/// the caller aborts it. The scaler ticks on a task of its own (a provider's slow call must
/// not hold back the job pass): at each tick it first fetches the catalogs that are due
/// ([`Cached::refresh`]) and then runs its own tick, so a tick is finished when the server's
/// tick, the fetch and the scaler are done, or at once when the scaler is still busy with an
/// earlier tick, which skips this one for it.
///
/// A panic in the scaler ends its task, as one in the server's tick ends this loop: `/healthz` then
/// answers that the server is not ticking, and the supervisor of the process must restart
/// it. The records are on disk before any call they describe, so a restart launches nothing
/// twice.
async fn tick_loop<T: Ticker>(
    shared: Arc<Shared>,
    mut ticker: T,
    scaler: Option<(Scaler, Cached)>,
) {
    let _alive = Alive(shared.clone());
    let side = scaler.map(|(scaler, cached)| ScalerLoop::spawn(shared.clone(), scaler, cached));
    loop {
        let tick = ticker.next().await;
        tick_once(&shared, side.as_ref(), tick).await;
    }
}

/// The placement poller of `shared`'s agent side: one pass over the queued jobs after each
/// poke ([`crate::poller::poked`], [`crate::poller::run`]), until `stopped` fires (or its
/// sender is dropped), which it heeds only between passes. A panic in a pass ends it as one in
/// the tick ends the tick loop: `/healthz` then answers that the server is not ticking, and the
/// supervisor of the process must restart it.
async fn poll_loop(shared: Arc<Shared>, mut stopped: oneshot::Receiver<()>) {
    let _alive = Alive(shared.clone());
    let agents = Arc::clone(shared.agents());
    loop {
        tokio::select! {
            biased;
            _ = &mut stopped => return,
            () = crate::poller::poked(&agents) => {}
        }
        crate::poller::run(&agents).await;
    }
}

/// One tick: the server's own, and then the scaler's turn when there is one. Not generic, so that
/// every kind of ticker runs the same code.
async fn tick_once(shared: &Shared, side: Option<&ScalerLoop>, tick: Tick) {
    shared.tick().await;
    match side {
        Some(side) => side.offer(tick),
        None => tick.finish(),
    }
}

/// The task that ticks the scaler, ended when it is dropped.
struct ScalerLoop {
    ticks: mpsc::Sender<Tick>,
    task: JoinHandle<()>,
}

impl ScalerLoop {
    fn spawn(shared: Arc<Shared>, scaler: Scaler, cached: Cached) -> Self {
        let (ticks, mut due) = mpsc::channel::<Tick>(1);
        let task = tokio::spawn(async move {
            let _alive = Alive(shared);
            loop {
                let tick = or_pending(due.recv().await).await;
                cached.refresh().await;
                scaler.tick().await;
                tick.finish();
            }
        });
        Self { ticks, task }
    }

    /// Hands `tick` to the scaler, which finishes it after its own tick. When the scaler is
    /// still at an earlier tick (or has ended), `tick` is finished here and the scaler goes
    /// without it.
    fn offer(&self, tick: Tick) {
        if let Err(refused) = self.ticks.try_send(tick) {
            refused.into_inner().finish();
        }
    }
}

impl Drop for ScalerLoop {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The tick that ends a shutdown, and the usage side's last flush.
async fn final_tick(shared: &Shared) {
    shared.tick().await;
    if let Some(usage) = shared.usage() {
        usage.finish().await;
    }
}

impl Running {
    /// Re-reads `--tls-cert` and `--tls-key` and serves the pair from the next handshake on;
    /// with `--agent-client-ca`, reads its file again too and trusts its certificates from the
    /// next handshake on.
    ///
    /// The pair is checked whole first: the key file owner-only, the key the certificate's,
    /// the certificate valid now. A pair or a CA file that fails is refused with one logged line
    /// and the pair and the CA being served stay. Connections already open keep their session.
    pub fn reload_tls(&self) -> Result<(), StartupError> {
        let Some(tls) = &self.tls else {
            return Err(StartupError::new(
                "--tls-cert",
                "the server was started without TLS, so there is nothing to reload",
            ));
        };
        match tls.reload() {
            Ok(()) => {
                info!("tls reloaded: the renewed certificate is served from the next handshake");
                Ok(())
            }
            Err(err) => {
                error!(%err, "tls reload refused: the previous certificate and key stay in use");
                Err(err)
            }
        }
    }

    /// The address the server listens on, with the port the system chose for port 0.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The address the agent routes alone are served on, with the port the system chose
    /// for port 0; `None` when the agent routes share the port of [`Running::addr`].
    pub fn agent_addr(&self) -> Option<SocketAddr> {
        self.agent.as_ref().map(|agent| agent.addr)
    }

    /// The address the job service is served on (`--job-listen`), with the port the system
    /// chose for port 0; `None` when the job service is off.
    pub fn job_addr(&self) -> Option<SocketAddr> {
        self.job.as_ref().map(|job| job.addr)
    }

    /// A handle on the agent registry (the same registry the routes and the tick use),
    /// for reading what it records, such as [`js_registry::Registry::request_record`].
    /// Always `Some`: every server registers hosts. The `Option` is kept so that callers
    /// built against the release that had agentless servers keep building.
    pub fn registry(&self) -> Option<js_registry::Registry> {
        Some(self.shared.registry())
    }

    /// The address the metrics are served on (`--metrics-listen`), with the port the system
    /// chose for port 0; `None` when the metrics are off, and no listener serves them.
    pub fn metrics_addr(&self) -> Option<SocketAddr> {
        self.metrics.as_ref().map(|metrics| metrics.addr)
    }

    /// The address the readiness route is served on (`--ready-listen`), with the port the
    /// system chose for port 0; `None` when the route is off, and no listener serves it.
    pub fn ready_addr(&self) -> Option<SocketAddr> {
        self.ready.as_ref().map(|ready| ready.addr)
    }

    /// The scaler of the elastic pool of the configuration file; `None` without one. A test
    /// reads what it decided through [`Scaler::state`].
    pub fn scaler(&self) -> Option<&Scaler> {
        self.scaler.as_ref()
    }

    /// The store the server keeps its state in (`--store`, `server.store`), shared;
    /// `None` without a store setting.
    pub fn store(&self) -> Option<Arc<dyn js_state::Store>> {
        self.shared.server_store().map(ServerStore::store)
    }

    /// How many times the agent side could not read or write one of its files since the
    /// server started: the join token file that could not be read again at a tick, the
    /// file of bindings (or, with a store setting, the store) that could not be written, and
    /// the credential times a tick could not commit to the store.
    pub fn agent_errors(&self) -> u64 {
        self.shared.agent_errors()
    }

    /// The curves learned from runs that ended cleanly, for admission to read as its
    /// `CurveSource`. A curve is kept under the profile name the caller supplied for the runs,
    /// not under the work key, and the server maps no work key to a profile name, so every
    /// work key is charged flat. `None` with `--usage-off` or with a sink that learns
    /// none.
    pub fn usage_curves(&self) -> Option<Arc<ProfileCurves>> {
        self.shared.usage_curves()
    }

    /// What the usage side has counted since the server started: loads rejected, records
    /// dropped, runs learned or not and why, and the failures of the usage store, with the
    /// sink's own counts. All zero with `--usage-off`.
    pub fn usage_counters(&self) -> UsageCounters {
        self.shared.usage_counters()
    }

    /// How many times the accept loop found every one of the `--max-connections` slots
    /// taken and waited for one to be given back, since the server started. A loop
    /// that waits is not accepting: this is how a test sees the cap hold, with a
    /// connection open at the cap, without waiting for something not to happen.
    pub fn accept_waits(&self) -> u64 {
        self.slots.waits()
    }

    /// The first half of a stop on a termination signal: the readiness route
    /// (`--ready-listen`) answers 503 `draining` at once, and the server keeps serving the
    /// job service and ticking for `--drain-hold`, so a proxy that polls readiness takes it
    /// out of rotation while requests still succeed. [`Running::shutdown`] is the second half.
    ///
    /// The agents leave at once ([`crate::drain`]): the listener that serves the agent routes
    /// (`server.agent_listen`, or `--listen` when they share it) takes no new connection, each
    /// request it is serving is answered and its connection closed (within `--drain-timeout`),
    /// a held heartbeat is answered now, and the server places no more jobs. An agent opens a
    /// connection for each request, so its next heartbeat reaches another server over the same
    /// store, which serves its session from the host's row (no hello again); the agent keeps
    /// its runs. Without another server the agents retry with their backoff until one starts.
    pub async fn hold(&self) {
        self.gate.drain();
        self.shared.agents().draining.begin();
        match &self.agent {
            Some(agent) => agent.stop.send(),
            None => self.stop.send(),
        }
        tokio::time::sleep(self.drain_hold).await;
    }

    /// Stops the server: no new connections, the requests in flight finish (those still
    /// open after `--drain-timeout` are cut), and the server is ticked once more (so a job
    /// that ran out of time is ended before the process ends).
    ///
    /// It is an error when the serving task failed.
    ///
    /// The readiness route (`--ready-listen`) answers 503 `draining` from the start of the stop,
    /// and its listener is closed last, after the last tick.
    pub async fn shutdown(mut self) -> io::Result<()> {
        self.gate.drain();
        if let Some(ready) = &self.ready {
            info!(addr = %ready.addr, "draining: the readiness route answers 503 until the process exits");
        }
        let ready = self.ready.take();
        let mut ended = self.stop_serving().await;
        for closing in ready.into_iter().map(Port::stop) {
            ended = ended.and(closing.await.map_err(io::Error::other));
        }
        ended
    }

    /// The stop of [`Running::shutdown`] but for the readiness listener.
    async fn stop_serving(mut self) -> io::Result<()> {
        self.stop.send();
        let ports = [self.agent.take(), self.job.take(), self.metrics.take()];
        let others: Vec<JoinHandle<()>> = ports.into_iter().flatten().map(Port::stop).collect();
        let mut served = self.serving.await.map_err(io::Error::other);
        for serving in others {
            served = served.and(serving.await.map_err(io::Error::other));
        }
        self.ticking.0.abort();
        let _ = (&mut self.ticking.0).await;
        // The poller ends after the pass it may be running, so that pass lets go of the state
        // before the last tick and before the stop returns.
        let Polling { stop, mut task } = self.polling;
        let _ = stop.send(());
        let _ = (&mut task.0).await;
        // The delivery threads end after their tries in flight, each within its send limit.
        let delivering = self.delivering.take();
        blocking(move || delivering.into_iter().for_each(Deliverers::stop)).await;
        self.shared.agents().sizes.stop();
        if let Some(sizes) = self.sizes.take() {
            let _ = sizes.await;
        }
        final_tick(&self.shared).await;
        served
    }
}

#[cfg(test)]
#[path = "server_shutdown_tests.rs"]
mod shutdown_tests;

#[cfg(test)]
#[path = "server_sizes_tests.rs"]
mod sizes_tests;

#[cfg(test)]
#[path = "server_listen_tests.rs"]
mod listen_tests;

#[cfg(test)]
#[path = "server_storm_tests.rs"]
mod storm_tests;
