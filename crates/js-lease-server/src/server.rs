//! Starting the server, running it, and stopping it.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use js_lease::{Booking, Boot, LeaseTable, Monotonic, Settings, read_bookings};
use js_provider::Pool as _;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::adapt::{BoxedPool, SharedClock};
use crate::admit::{Admit, Launching};
use crate::agents::{Agents, assemble};
use crate::api::{Shared, Table, agent_router, router};
use crate::capacity::Seated;
use crate::catalogs::{Cached, Catalogs};
use crate::checks::Checker;
use crate::config::{
    Config, Hosts, JobListen, Limits, LiveClients, MetricsListen, TlsFiles, Token,
};
use crate::error::StartupError;
use crate::job_api;
use crate::listener::{Slots, serve, serve_both};
use crate::metrics::{self, Meter, Recorder};
use crate::pool::{Pool, Setup};
use crate::scaler::{Parts, Scaler, build};
use crate::status::Status;
use crate::store::{LockLost, LockWatch, ServerStore};
use crate::store_health::{LockState, StoreHealth};
use crate::ticker::{IntervalTicker, Tick, Ticker};
use crate::tls;
use crate::usage::ProfileCurves;
use crate::usage_hub::UsageCounters;
use crate::util::or_pending;

/// A running server. Dropping it stops serving and ticking; [`Running::shutdown`] stops
/// it the orderly way (requests in flight finish, one last tick).
pub struct Running {
    addr: SocketAddr,
    agent: Option<Port>,
    job: Option<Port>,
    metrics: Option<Port>,
    state_file: PathBuf,
    shared: Arc<Shared>,
    stop: oneshot::Sender<()>,
    slots: Arc<Slots>,
    serving: JoinHandle<()>,
    ticking: Ticking,
    _checking: Checking,
    scaler: Option<Scaler>,
    tls: Option<tls::Tls>,
    /// Where the tick announces the loss of the store's writer lock.
    lock_watch: LockWatch,
}

/// A listener beside the one of `--listen`: the agent routes alone (`server.agent_listen`),
/// the job service (`--job-listen`), or the metrics (`--metrics-listen`).
struct Port {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    serving: JoinHandle<()>,
}

impl Port {
    /// Tells the listener to stop; the task to await for its drain.
    fn stop(self) -> JoinHandle<()> {
        let _ = self.stop.send(());
        self.serving
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

/// Starts a server: builds the provider from the capacity source, opens the lease table
/// over the state file, binds the listener and begins serving and ticking. The checks of
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

/// [`start`] with the ticker of the pool checks given too. At each tick of `checks` the
/// check loop of the elastic pool (if the configuration has one) asks the injected clock
/// whether a pass is due, runs it if so, and finishes the tick when the pass is done, so a
/// test steps the checks as it steps the table. Without an elastic pool `checks` is
/// dropped.
pub async fn start_with_checks<T: Ticker, C: Ticker>(
    config: Config,
    clock: SharedClock,
    ticker: T,
    checks: C,
) -> Result<Running, StartupError> {
    let logs = config.logs().sink();
    let log_reads = job_api::logs::Ctx::from_config(config.logs(), &clock);
    let (prepared, listeners) = prepare(config, &clock)?;
    let tls = listeners
        .tls
        .as_ref()
        .map(|files| tls::Tls::open(files, clock.clone()))
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
        state_file,
    } = open(prepared, &clock, |table| {
        info!(
            %addr,
            provider = %table.provider().id(),
            hosts = table.provider().capacity().len(),
            tls = served_tls,
            logs = %logs,
            "listening"
        );
    })?;
    let limits = listeners.limits;
    let split = agent_listener.is_some();
    // Every listener stops at its own stop or at the loss of the store's writer lock.
    let lock_watch = LockWatch::new();
    let (stop, stopped) = oneshot::channel::<()>();
    let slots = Arc::new(Slots::new(limits.max_connections));
    let serving = serve(
        listener,
        acceptor.clone(),
        router(shared.clone(), !split),
        limits,
        slots.clone(),
        lock_watch.or_stopped(stopped),
    );
    let agent = agent_listener.map(|(listener, addr)| {
        let (stop, stopped) = oneshot::channel::<()>();
        let slots = Arc::new(Slots::new(limits.max_connections));
        let app = agent_router(&shared);
        let serving = serve(
            listener,
            acceptor,
            app,
            limits,
            slots,
            lock_watch.or_stopped(stopped),
        );
        info!(%addr, "listening for agents");
        Port {
            addr,
            stop,
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
            let stopped = lock_watch.or_stopped(stopped);
            let serving = serve_both(listener, job_acceptor, app, limits, slots, stopped);
            info!(%addr, tls = served_tls, "listening for the job service");
            Port {
                addr,
                stop,
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
                let stopped = lock_watch.or_stopped(stopped);
                let serving = serve(listener, None, app, limits, slots, stopped);
                info!(%addr, "serving the metrics");
                Port {
                    addr,
                    stop,
                    serving: tokio::spawn(serving),
                }
            });
    let ticking = Box::pin(tick_loop(
        shared.clone(),
        ticker,
        scaler.clone().map(|scaler| (scaler, cached)),
        lock_watch.clone(),
    ));
    let ticking = Ticking(tokio::spawn(until_fenced(shared.clone(), ticking)));
    let checking = Checking(checker.map(|checker| tokio::spawn(checker.run(checks))));
    Ok(Running {
        addr,
        agent,
        job,
        metrics,
        state_file,
        shared,
        stop,
        slots,
        serving: tokio::spawn(serving),
        ticking,
        _checking: checking,
        scaler,
        tls,
        lock_watch,
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
    limits: Limits,
}

/// What a start has built when its listeners are about to be bound: the agent side, the
/// elastic pool and its scaler, and the provider seated over the lease file's bookings,
/// with the parts of the configuration that [`open`] reads.
pub(crate) struct Prepared {
    bookings: Vec<Booking>,
    agents: Option<Arc<Agents>>,
    pool: Option<Pool>,
    scaler: Option<Scaler>,
    seated: Seated,
    state_file: PathBuf,
    boot: Boot,
    settings: Settings,
    tick: Duration,
    token: Token,
    monotonic: Arc<dyn Monotonic>,
    warnings: Vec<String>,
    status: bool,
    admit: Admit,
    cached: Cached,
    /// The store, opened under its writer lock, when the configuration names one.
    store: Option<ServerStore>,
    /// The health of the store paths, which the tick records and `/healthz` reads.
    store_health: StoreHealth,
}

/// What a start has built once the lease table is open: the state the routes and the tick
/// share, the elastic pool's scaler and its check loop, the cache of the launching
/// providers' catalogs, and the lease file.
pub(crate) struct Opened {
    pub(crate) shared: Arc<Shared>,
    pub(crate) scaler: Option<Scaler>,
    pub(crate) checker: Option<Checker>,
    pub(crate) cached: Cached,
    pub(crate) state_file: PathBuf,
}

/// The first half of a start, everything before a listener is bound: takes the store's
/// writer lock and opens the store when the configuration names one, then reads the lease
/// file's bookings, puts the agent side together, builds the elastic pool and its scaler
/// (which marks the machines its records show as being released) and seats the provider
/// over the bookings. Binds nothing and spawns nothing.
pub(crate) fn prepare(
    config: Config,
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
        settings,
        tick,
        token,
        clients,
        tls,
        limits,
        hosts,
        usage,
        monotonic,
        entropy,
        alerts,
        alert_repeat,
        logs,
        max_job_wall,
        store,
        store_unreachable_after,
        ..
    } = config;
    // The store's writer lock before anything else (design 6.7, step 1): before the store or
    // the lease file is read, before the lease file is locked and before any listener.
    let store = store.map(ServerStore::open).transpose()?;
    if let Some(store) = &store {
        store.refuse_agents(matches!(hosts, Hosts::Layered { .. }))?;
    }
    let bookings =
        read_bookings(&state_file).map_err(|err| StartupError::new("--state-file", err))?;
    let logs = crate::logs::from_config(&logs, clock);
    let (source, agents) = assemble(hosts, usage, clock, &state_file, logs)?;
    let launching = elastic.iter().map(|plan| Launching {
        provider: Arc::clone(&plan.provider),
        share: plan.share,
        limits: plan.limits.clone(),
    });
    // The scaler's tick fetches the catalogs. An elastic plan comes only with a pool whose
    // hosts register (agents), so a server with a catalog to fetch always has a scaler.
    let catalogs = elastic.iter().fold(Catalogs::new(), |catalogs, plan| {
        let provider = Arc::clone(&plan.provider);
        catalogs.with(provider, plan.catalog_refresh, plan.offer_cooldown)
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
    let scaler = match (&elastic, agents.clone()) {
        (Some((pool, plan)), Some(agents)) => {
            let parts = Parts {
                agents,
                clock: clock.clone(),
                state_file: &state_file,
                advertise_url: advertise_url.as_deref(),
                first_start: boot == Boot::Initialise,
                health: pool.health.clone(),
                mode: pool.mode(),
                catalogs: Arc::clone(cached.catalogs()),
                entropy,
            };
            Some(build(plan, parts)?)
        }
        _ => None,
    };
    // The store's alert goes to the same sink as the scaler's, through a dedupe of its own.
    let store_health = StoreHealth::new(clock.clone(), store_unreachable_after);
    let store_health = match &alerts {
        Some(sink) => store_health.with_sink(Arc::clone(sink), alert_repeat),
        None => store_health,
    };
    store_health.set_lock(match &store {
        Some(_) => LockState::Held,
        None => LockState::None,
    });
    if let (Some(scaler), Some(sink)) = (&scaler, alerts) {
        scaler.set_sink(sink, alert_repeat);
    }
    // With the metrics on, the agent side and the scaler count their events from here on,
    // before anything is served or ticked.
    let metrics = metrics_listen.map(|at| (at, Arc::new(Recorder::default())));
    if let Some((_, recorder)) = &metrics {
        let meter = Meter::new(recorder.clone(), Arc::clone(&monotonic));
        agents
            .iter()
            .for_each(|agents| agents.set_meter(meter.clone()));
        scaler
            .iter()
            .for_each(|scaler| scaler.set_meter(meter.clone()));
    }
    let seated = source.provider(&bookings)?;
    let prepared = Prepared {
        bookings,
        agents,
        pool: elastic.map(|(pool, _)| pool),
        scaler,
        seated,
        state_file,
        boot,
        settings,
        tick,
        token,
        monotonic,
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
        limits,
    };
    Ok((prepared, listeners))
}

/// The second half of a start, once the listeners are bound (or, in process, once there is
/// nothing to bind): the scaler starts counting the registry's view, the lease table is
/// opened over the lease file (and locks it), the bookings it could not seat are logged,
/// `announce` is given the table to say where it is served, and the state the routes and the
/// tick share is built over it. Spawns nothing.
pub(crate) fn open(
    prepared: Prepared,
    clock: &SharedClock,
    announce: impl FnOnce(&Table),
) -> Result<Opened, StartupError> {
    let Prepared {
        bookings,
        agents,
        pool,
        scaler,
        seated: Seated { provider, unseated },
        state_file,
        boot,
        settings,
        tick,
        token,
        monotonic,
        warnings,
        status: serves_status,
        admit,
        cached,
        store,
        store_health,
    } = prepared;
    let registry = agents.as_ref().map(|agents| agents.registry.clone());
    let logs = agents.as_ref().and_then(|agents| agents.logs.clone());
    if let Some(scaler) = &scaler {
        scaler.listening(Some(2 * tick), &bookings);
    }
    let mut table = LeaseTable::open(
        BoxedPool(provider),
        clock.clone(),
        settings,
        &state_file,
        boot,
    )
    .map_err(|err| match err {
        js_lease::OpenError::Missing { .. } => StartupError::new("--boot", err),
        _ => StartupError::new("--state-file", err),
    })?;
    table.watch_saves(monotonic);
    for lost in &unseated {
        warn!(
            lease = %lost.booking.lease,
            host = %lost.booking.host,
            state = %lost.booking.state,
            reason = %lost.reason,
            "the lease file's booking cannot be restored; the provider does not know the lease, so the table drops it"
        );
    }
    announce(&table);
    for warning in &warnings {
        warn!("{warning}");
    }
    let checker = pool.as_ref().map(|pool| pool.checker(clock.clone()));
    let store_health = Arc::new(store_health);
    let pools = Status::new(pool, registry, scaler.clone(), warnings, clock.clone());
    let pools = Arc::new(
        pools
            .with_logs(logs)
            .with_store_health(Arc::clone(&store_health)),
    );
    let status = serves_status.then(|| Arc::clone(&pools));
    let shared = Arc::new(
        Shared::new(table, token, agents)
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
        state_file,
    })
}

/// The tick task, ended when it is dropped: a server that is dropped must not go on
/// ticking a table (and holding its lock on the lease file).
struct Ticking(JoinHandle<()>);

impl Drop for Ticking {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The check loop of the elastic pool, ended when it is dropped.
struct Checking(Option<JoinHandle<()>>);

impl Drop for Checking {
    fn drop(&mut self) {
        self.0.iter().for_each(JoinHandle::abort);
    }
}

/// Marks the tick loop as stopped when it is dropped, however it ends: the caller
/// aborting it, or the ticker or the table panicking. `/healthz` reads the mark.
struct Alive(Arc<Shared>);

impl Drop for Alive {
    fn drop(&mut self) {
        self.0.stop_ticking();
    }
}

/// Ticks the table at every tick of `ticker`, and hands each tick to the scaler of the
/// elastic pool when the server has one, with the cache of its provider's catalog, forever;
/// the caller aborts it, or drops it once the state store says this process is no longer its
/// writer ([`until_fenced`]). The scaler ticks on a task of its own (a provider's slow call must
/// not hold back the table's lease expiry): at each tick it first fetches the catalogs that
/// are due ([`Cached::refresh`]) and then runs its own tick, so a tick is finished when the
/// table, the fetch and the scaler are done, or at once when the scaler is still busy with
/// an earlier tick, which skips this one for it.
///
/// A panic in the scaler ends its task, as one in the table ends this loop: `/healthz` then
/// answers that the server is not ticking, and the supervisor of the process must restart
/// it. The records are on disk before any call they describe, so a restart launches nothing
/// twice.
///
/// With a store, each tick first asks whether the store's writer lock is still held. A tick
/// that finds it lost announces the loss on `lock_watch` (which stops the listeners), does
/// nothing else, and ends the loop: no tick of the table or the scaler follows.
async fn tick_loop<T: Ticker>(
    shared: Arc<Shared>,
    mut ticker: T,
    scaler: Option<(Scaler, Cached)>,
    lock_watch: LockWatch,
) {
    let _alive = Alive(shared.clone());
    let side = scaler.map(|(scaler, cached)| ScalerLoop::spawn(shared.clone(), scaler, cached));
    loop {
        let tick = ticker.next().await;
        if let Err(lost) = shared.check_store().await {
            let health = shared.store_health();
            health
                .iter()
                .for_each(|health| health.set_lock(LockState::Lost));
            lock_watch.lose(lost);
            tick.finish();
            return;
        }
        tick_once(&shared, side.as_ref(), tick).await;
    }
}

/// One tick: the table, and then the scaler's turn when there is one. Not generic, so that
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

/// Why a server whose state store has a newer writer stops with an error.
pub(crate) const FENCED: &str =
    "the state store has a newer writer, so this server may no longer write to it";

/// Completes when `signalled` does (a termination signal), or, logging one error line, when
/// the state store says this process is no longer its writer: the server cannot write again,
/// so the caller stops it ([`Running::shutdown`], which then answers an error) and the process
/// exits with a failure for a restart to open a new handle.
pub(crate) async fn stop_or_fence(
    running: &Running,
    signalled: impl Future<Output = io::Result<()>>,
) -> io::Result<()> {
    tokio::select! {
        biased;
        () = running.fenced() => {
            error!("the state store has a newer writer: this server can write no more and is stopping");
            Ok(())
        }
        stopped = signalled => stopped,
    }
}

/// Runs `ticking` (the tick loop) until the state store says this process is no longer its
/// writer, when it is dropped: a tick under way is cut at its next wait, nothing is begun
/// after it, and the loop's guards run ([`Alive`] marks `/healthz`, the scaler's task ends).
async fn until_fenced(shared: Arc<Shared>, ticking: Pin<Box<dyn Future<Output = ()> + Send>>) {
    tokio::select! { biased; () = shared.fenced() => {}, () = ticking => {} }
}

/// The tick that ends a shutdown, and the usage side's last flush; an error when the tick
/// could not write the lease file.
async fn final_tick(shared: &Shared, state_file: &Path) -> io::Result<()> {
    let report = shared.tick().await;
    if let Some(usage) = shared.usage() {
        usage.finish().await;
    }
    report.save_error.map_or(Ok(()), |err| {
        let file = state_file.display();
        let reason = format!("the final tick could not write the lease file {file}: {err}");
        Err(io::Error::other(reason))
    })
}

impl Running {
    /// Re-reads `--tls-cert` and `--tls-key` and serves the pair from the next handshake on.
    ///
    /// The pair is checked whole first: the key file owner-only, the key the certificate's,
    /// the certificate valid now. A pair that fails is refused with one logged line and
    /// the pair being served stays. Connections already open keep their session.
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
    /// for reading what it records, such as [`js_registry::Registry::request_record`];
    /// `None` without agent flags.
    pub fn registry(&self) -> Option<js_registry::Registry> {
        self.shared.registry()
    }

    /// The address the metrics are served on (`--metrics-listen`), with the port the system
    /// chose for port 0; `None` when the metrics are off, and no listener serves them.
    pub fn metrics_addr(&self) -> Option<SocketAddr> {
        self.metrics.as_ref().map(|metrics| metrics.addr)
    }

    /// How long the lease table's steps took since the server started.
    pub fn timings(&self) -> js_lease::Timings {
        self.shared.timings()
    }

    /// The scaler of the elastic pool of the configuration file; `None` without one. A test
    /// reads what it decided through [`Scaler::state`].
    pub fn scaler(&self) -> Option<&Scaler> {
        self.scaler.as_ref()
    }

    /// Completes when the server stopped on its own because a tick found the store's writer
    /// lock lost (`--store`): it then serves, ticks and writes nothing more, and
    /// [`Running::shutdown`] answers the loss. It never completes without a store, or while
    /// the lock is held.
    pub async fn lock_lost(&self) -> LockLost {
        self.lock_watch.wait().await
    }

    /// The store the server holds under its writer lock (`--store`, `server.store`), shared;
    /// `None` without a store setting.
    pub fn store(&self) -> Option<Arc<dyn js_state::Store>> {
        self.shared.server_store().map(ServerStore::store)
    }

    /// How many ticks failed to write the lease file since the server started.
    pub fn save_errors(&self) -> u64 {
        self.shared.save_errors()
    }

    /// How many times the agent side could not read or write one of its files since the
    /// server started: the join token file that could not be read again at a tick, and
    /// the file of bindings that could not be written. Always 0 without agent flags.
    pub fn agent_errors(&self) -> u64 {
        self.shared.agent_errors()
    }

    /// The curves learned from runs that ended cleanly, for admission to read as its
    /// `CurveSource`. A curve is kept under the profile name the caller supplied for the runs,
    /// not under the work key, so admission finds it through the names the caller gave
    /// (`Config::with_curve_names`); without them every work key is charged flat.
    /// `None` without agent registration, with `--usage-off`, or with a sink that learns
    /// none.
    pub fn usage_curves(&self) -> Option<Arc<ProfileCurves>> {
        self.shared.usage_curves()
    }

    /// What the usage side has counted since the server started: loads rejected, records
    /// dropped, runs learned or not and why, and the failures of the usage store, with the
    /// sink's own counts. All zero without agent registration or with `--usage-off`.
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

    /// Completes when the state store has said this process is no longer its writer, which
    /// the server cannot go on from: the caller stops it ([`Running::shutdown`], which then
    /// answers an error) and exits with a failure. Never completes for a server without
    /// agents or without the job service, which have no state store.
    pub async fn fenced(&self) {
        self.shared.fenced().await;
    }

    /// Stops the server: no new connections, the requests in flight finish (those still
    /// open after `--drain-timeout` are cut), the table is ticked once more (so an expiry
    /// that fell due is stopped before the process ends) and the lease table is released.
    ///
    /// It is an error when the serving task failed, or when that last tick could not
    /// write the lease file: the file is then behind the table, and an orderly stop
    /// that says nothing would hide it. The cause is in the error and in the log.
    ///
    /// After the store's writer lock was lost ([`Running::lock_lost`]) there is no last tick
    /// (nothing more is written) and the error is the loss.
    ///
    /// When the state store has said this process is no longer its writer, the last tick is
    /// not run (it would write the job tables and the lease file of a server that no longer
    /// owns them) and the stop is an error that says so.
    pub async fn shutdown(mut self) -> io::Result<()> {
        let _ = self.stop.send(());
        let ports = [self.agent.take(), self.job.take(), self.metrics.take()];
        let others: Vec<JoinHandle<()>> = ports.into_iter().flatten().map(Port::stop).collect();
        let mut served = self.serving.await.map_err(io::Error::other);
        for serving in others {
            served = served.and(serving.await.map_err(io::Error::other));
        }
        self.ticking.0.abort();
        let _ = (&mut self.ticking.0).await;
        if let Some(lost) = self.lock_watch.lost() {
            return served.and(Err(io::Error::other(lost)));
        }
        let saved = match self.shared.is_fenced() {
            true => Err(io::Error::other(FENCED)),
            false => final_tick(&self.shared, &self.state_file).await,
        };
        served.and(saved)
    }
}

#[cfg(test)]
#[path = "server_fence_tests.rs"]
mod fence_tests;
