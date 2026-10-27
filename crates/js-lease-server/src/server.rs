//! Starting the server, running it, and stopping it.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use js_lease::LeaseTable;
use js_provider::Provider;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::info;

use crate::adapt::{BoxedProvider, SharedClock};
use crate::api::{Shared, router};
use crate::config::Config;
use crate::error::StartupError;
use crate::listener::serve;
use crate::ticker::Ticker;
use crate::tls;

/// A running server. Dropping it stops serving and ticking; [`Running::shutdown`] stops
/// it the orderly way (requests in flight finish, one last tick).
pub struct Running {
    addr: SocketAddr,
    shared: Arc<Shared>,
    stop: oneshot::Sender<()>,
    serving: JoinHandle<()>,
    ticking: Ticking,
}

/// Starts a server: builds the provider from the capacity source, opens the lease table
/// over the state file, binds the listener and begins serving and ticking.
///
/// Time comes from `clock` and ticks from `ticker`, so a test decides both. Each
/// failure to start names the flag at fault.
pub async fn start<T: Ticker>(
    config: Config,
    clock: SharedClock,
    ticker: T,
) -> Result<Running, StartupError> {
    let provider = config.capacity.provider()?;
    let acceptor = config.tls.as_ref().map(tls::acceptor).transpose()?;
    let bound = TcpListener::bind(config.listen)
        .await
        .and_then(|listener| listener.local_addr().map(|addr| (listener, addr)));
    let (listener, addr) = bound.map_err(|err| StartupError::new("--listen", err))?;
    let table = LeaseTable::open(
        BoxedProvider(provider),
        clock,
        config.settings,
        &config.state_file,
        config.boot,
    )
    .map_err(|err| match err {
        js_lease::OpenError::Missing { .. } => StartupError::new("--boot", err),
        _ => StartupError::new("--state-file", err),
    })?;
    info!(
        %addr,
        provider = %table.provider().id(),
        hosts = table.provider().capacity().len(),
        tls = acceptor.is_some(),
        "listening"
    );
    let shared = Arc::new(Shared::new(table, config.token));
    let (stop, stopped) = oneshot::channel::<()>();
    let serving = serve(
        listener,
        acceptor,
        router(shared.clone()),
        config.limits,
        stopped,
    );
    let ticking = Ticking(tokio::spawn(tick_loop(shared.clone(), ticker)));
    Ok(Running {
        addr,
        shared,
        stop,
        serving: tokio::spawn(serving),
        ticking,
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

/// Ticks the table at every tick of `ticker`, forever; the caller aborts it.
async fn tick_loop<T: Ticker>(shared: Arc<Shared>, mut ticker: T) {
    loop {
        let tick = ticker.next().await;
        shared.tick().await;
        tick.finish();
    }
}

impl Running {
    /// The address the server listens on, with the port the system chose for port 0.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// How many ticks failed to write the lease file since the server started.
    pub fn save_errors(&self) -> u64 {
        self.shared.save_errors()
    }

    /// Stops the server: no new connections, the requests in flight finish (those still
    /// open after `--drain-timeout` are cut), the table is ticked once more (so an expiry
    /// that fell due is stopped before the process ends) and the lease table is released.
    pub async fn shutdown(mut self) -> io::Result<()> {
        let _ = self.stop.send(());
        let served = self.serving.await.map_err(io::Error::other);
        self.ticking.0.abort();
        let _ = (&mut self.ticking.0).await;
        self.shared.tick().await;
        served
    }
}
