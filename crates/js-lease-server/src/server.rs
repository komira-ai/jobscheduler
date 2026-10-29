//! Starting the server, running it, and stopping it.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use js_lease::{Boot, LeaseTable, read_bookings};
use js_provider::Provider;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::adapt::{BoxedProvider, SharedClock};
use crate::agents::assemble;
use crate::api::{Shared, agent_router, router};
use crate::capacity::Seated;
use crate::config::Config;
use crate::error::StartupError;
use crate::listener::{Slots, serve};
use crate::pool::{Pool, Setup};
use crate::scaler::{Parts, Scaler, build};
use crate::status::Status;
use crate::ticker::{IntervalTicker, Ticker};
use crate::tls;

/// A running server. Dropping it stops serving and ticking; [`Running::shutdown`] stops
/// it the orderly way (requests in flight finish, one last tick).
pub struct Running {
    addr: SocketAddr,
    agent: Option<AgentPort>,
    state_file: PathBuf,
    shared: Arc<Shared>,
    stop: oneshot::Sender<()>,
    slots: Arc<Slots>,
    serving: JoinHandle<()>,
    ticking: Ticking,
    _checking: Checking,
    scaler: Option<Scaler>,
}

/// The listener that serves the agent routes alone (`server.agent_listen`).
struct AgentPort {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    serving: JoinHandle<()>,
}

/// Binds `addr`, refusing the start naming `flag` when it cannot.
async fn bind(
    addr: SocketAddr,
    flag: &'static str,
) -> Result<(TcpListener, SocketAddr), StartupError> {
    let bound = TcpListener::bind(addr)
        .await
        .and_then(|listener| listener.local_addr().map(|addr| (listener, addr)));
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
    mut config: Config,
    clock: SharedClock,
    ticker: T,
    checks: C,
) -> Result<Running, StartupError> {
    let bookings =
        read_bookings(&config.state_file).map_err(|err| StartupError::new("--state-file", err))?;
    let (source, agents) = assemble(config.hosts, &clock, &config.state_file)?;
    let registry = agents.as_ref().map(|agents| agents.registry.clone());
    let elastic = config.elastic.take().map(|mut plan| {
        let setup = Setup {
            advertise: config.advertise_url.clone(),
            tls: config.tls.clone(),
            inputs: std::mem::take(&mut config.launch_inputs),
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
                state_file: &config.state_file,
                advertise_url: config.advertise_url.as_deref(),
                first_start: config.boot == Boot::Initialise,
                health: pool.health.clone(),
                mode: pool.mode(),
            };
            Some(build(plan, parts)?)
        }
        _ => None,
    };
    let Seated { provider, unseated } = source.provider(&bookings)?;
    let acceptor = config.tls.as_ref().map(tls::acceptor).transpose()?;
    let (listener, addr) = bind(config.listen, config.listen_flag).await?;
    let agent_listener = match config.agent_listen {
        Some(agent) => Some(bind(agent, "server.agent_listen").await?),
        None => None,
    };
    let table = LeaseTable::open(
        BoxedProvider(provider),
        clock.clone(),
        config.settings,
        &config.state_file,
        config.boot,
    )
    .map_err(|err| match err {
        js_lease::OpenError::Missing { .. } => StartupError::new("--boot", err),
        _ => StartupError::new("--state-file", err),
    })?;
    for lost in &unseated {
        warn!(
            lease = %lost.booking.lease,
            host = %lost.booking.host,
            state = %lost.booking.state,
            reason = %lost.reason,
            "the lease file's booking cannot be restored; the provider does not know the lease, so the table drops it"
        );
    }
    info!(
        %addr,
        provider = %table.provider().id(),
        hosts = table.provider().capacity().len(),
        tls = acceptor.is_some(),
        "listening"
    );
    for warning in &config.warnings {
        warn!("{warning}");
    }
    let pool = elastic.map(|(pool, _)| pool);
    let checker = pool.as_ref().map(|pool| pool.checker(clock.clone()));
    let status = Status::new(
        pool,
        registry,
        scaler.clone(),
        config.warnings.clone(),
        clock.clone(),
    );
    let status = config.status.then(|| Arc::new(status));
    let shared = Arc::new(Shared::new(table, config.token, agents).with_status(status));
    let split = agent_listener.is_some();
    let (stop, stopped) = oneshot::channel::<()>();
    let slots = Arc::new(Slots::new(config.limits.max_connections));
    let serving = serve(
        listener,
        acceptor.clone(),
        router(shared.clone(), !split),
        config.limits,
        slots.clone(),
        stopped,
    );
    let agent = agent_listener.map(|(listener, addr)| {
        let (stop, stopped) = oneshot::channel::<()>();
        let slots = Arc::new(Slots::new(config.limits.max_connections));
        let app = agent_router(&shared);
        let serving = serve(listener, acceptor, app, config.limits, slots, stopped);
        info!(%addr, "listening for agents");
        AgentPort {
            addr,
            stop,
            serving: tokio::spawn(serving),
        }
    });
    let ticking = Ticking(tokio::spawn(tick_loop(
        shared.clone(),
        ticker,
        scaler.clone(),
    )));
    let checking = Checking(checker.map(|checker| tokio::spawn(checker.run(checks))));
    Ok(Running {
        addr,
        agent,
        state_file: config.state_file,
        shared,
        stop,
        slots,
        serving: tokio::spawn(serving),
        ticking,
        _checking: checking,
        scaler,
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

/// Ticks the table at every tick of `ticker`, and then the scaler when the server has an
/// elastic pool, forever; the caller aborts it. A tick is finished when both are done.
async fn tick_loop<T: Ticker>(shared: Arc<Shared>, mut ticker: T, scaler: Option<Scaler>) {
    let _alive = Alive(shared.clone());
    loop {
        let tick = ticker.next().await;
        tick_once(&shared, scaler.as_ref()).await;
        tick.finish();
    }
}

/// One tick: the table, and then the scaler of the elastic pool when there is one. Not
/// generic, so that every kind of ticker runs the same code.
async fn tick_once(shared: &Shared, scaler: Option<&Scaler>) {
    shared.tick().await;
    if let Some(scaler) = scaler {
        scaler.tick().await;
    }
}

impl Running {
    /// The address the server listens on, with the port the system chose for port 0.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The address the agent routes alone are served on, with the port the system chose
    /// for port 0; `None` when the agent routes share the port of [`Running::addr`].
    pub fn agent_addr(&self) -> Option<SocketAddr> {
        self.agent.as_ref().map(|agent| agent.addr)
    }

    /// A handle on the agent registry (the same registry the routes and the tick use),
    /// for reading what it records, such as [`js_registry::Registry::request_record`];
    /// `None` without agent flags.
    pub fn registry(&self) -> Option<js_registry::Registry> {
        self.shared.registry()
    }

    /// The scaler of the elastic pool of the configuration file; `None` without one. A test
    /// reads what it decided through [`Scaler::state`].
    pub fn scaler(&self) -> Option<&Scaler> {
        self.scaler.as_ref()
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

    /// How many times the accept loop found every one of the `--max-connections` slots
    /// taken and waited for one to be given back, since the server started. A loop
    /// that waits is not accepting: this is how a test sees the cap hold, with a
    /// connection open at the cap, without waiting for something not to happen.
    pub fn accept_waits(&self) -> u64 {
        self.slots.waits()
    }

    /// Stops the server: no new connections, the requests in flight finish (those still
    /// open after `--drain-timeout` are cut), the table is ticked once more (so an expiry
    /// that fell due is stopped before the process ends) and the lease table is released.
    ///
    /// It is an error when the serving task failed, or when that last tick could not
    /// write the lease file: the file is then behind the table, and an orderly stop
    /// that says nothing would hide it. The cause is in the error and in the log.
    pub async fn shutdown(mut self) -> io::Result<()> {
        let _ = self.stop.send(());
        let agent = self.agent.take().map(|agent| {
            let _ = agent.stop.send(());
            agent.serving
        });
        let served = self.serving.await.map_err(io::Error::other);
        let agent_served = match agent {
            Some(serving) => serving.await.map_err(io::Error::other),
            None => Ok(()),
        };
        let served = served.and(agent_served);
        self.ticking.0.abort();
        let _ = (&mut self.ticking.0).await;
        let report = self.shared.tick().await;
        let saved = report.save_error.map_or(Ok(()), |err| {
            let file = self.state_file.display();
            let reason = format!("the final tick could not write the lease file {file}: {err}");
            Err(io::Error::other(reason))
        });
        served.and(saved)
    }
}
