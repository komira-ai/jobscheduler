//! The forwarder of a job's host ports (`docs/design/job-rootfs.md`, 4.5): a listener the
//! agent holds inside the job's network namespace at each job port, and for each connection
//! a job makes to one, a new connection to the host port on the host's loopback, with the
//! bytes copied both ways.
//!
//! The listeners are made by [`Listen`] ([`Kernel`](super::Kernel) on a node: a thread of its
//! own enters the job's namespace, binds, and ends), and a socket keeps the namespace it was
//! made in, so the listener answers in the job's network while every connection out is made
//! in the agent's. The copying runs on a runtime of its own, [`THREADS`] threads named
//! `js-ports`, made at the first offer, so a busy upload never slows the lifecycle; it needs
//! no interface and no firewall rule, so it works at every network level, `none` included.
//!
//! A job's connection ends as TCP does through any proxy: when one side closes, the other is
//! shut for writing once what it sent is through, and the connection is gone when both are;
//! a reset or an error on one side closes both. A connection to the host port that is
//! refused closes the job's connection at once. An [`Offered`] that is dropped (the run is
//! released) closes its listeners and every connection through them.

use std::fmt::Debug;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener as StdListener};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_agent_wire::HostPort;
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::AbortHandle;

/// The threads of the runtime that copies the bytes of every run's host ports.
pub const THREADS: usize = 2;

/// How long a listener waits after an accept failed (the agent ran out of descriptors or
/// memory, say) before it accepts again: a failure is the agent's, and a pause lets it pass
/// without a spin.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// What makes the listeners of a job's ports. [`Kernel`](super::Kernel) is the one an agent
/// runs; a test's binds in its own namespace.
pub trait Listen: Debug + Send + Sync {
    /// A listener on `127.0.0.1` at each of `ports`, in order and one for each, in the network
    /// namespace of the process `pid` (a container's first process).
    ///
    /// # Errors
    ///
    /// The namespace cannot be entered, or a port cannot be bound there; no listener is
    /// left.
    fn listen(&self, pid: u32, ports: &[u16]) -> io::Result<Vec<StdListener>>;
}

/// Why a run's host ports could not be offered.
#[derive(Debug, thiserror::Error)]
pub enum OfferError {
    /// The listeners could not be made in the job's network namespace.
    #[error("listen at the job's ports {ports} in its network namespace: {source}")]
    Listen {
        /// The job ports, comma separated.
        ports: String,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// The runtime of the copying could not be made, or a listener could not be handed to it.
    #[error("start copying the host ports: {0}")]
    Copying(#[source] io::Error),
}

/// Offers runs their host ports; see the [module](self).
#[derive(Debug)]
pub struct Forwarder {
    listen: Arc<dyn Listen>,
    /// The runtime of the copying, made at the first offer.
    runtime: Mutex<Option<Runtime>>,
}

impl Forwarder {
    /// A forwarder whose listeners `listen` makes.
    #[must_use]
    pub fn new(listen: Arc<dyn Listen>) -> Self {
        Self {
            listen,
            runtime: Mutex::new(None),
        }
    }

    /// Offers the job whose container's first process is `pid` its host `ports`: a listener at
    /// each job port ([`HostPort::job_port`]) in its network namespace, each connection to
    /// which is joined to a new connection to the host port on `127.0.0.1`. The offer lasts
    /// until the [`Offered`] is dropped.
    ///
    /// # Errors
    ///
    /// No listener could be made, or the copying could not start; nothing is left listening.
    pub fn offer(&self, pid: u32, ports: &[HostPort]) -> Result<Offered, OfferError> {
        let handle = self.handle().map_err(OfferError::Copying)?;
        let job: Vec<u16> = ports.iter().map(HostPort::job_port).collect();
        let listeners = self.listen.listen(pid, &job).map_err(|source| {
            let ports: Vec<String> = job.iter().map(u16::to_string).collect();
            let ports = ports.join(",");
            OfferError::Listen { ports, source }
        })?;
        let _entered = handle.enter();
        let mut offered = Offered::default();
        for (listener, port) in listeners.into_iter().zip(ports) {
            // A failure drops `offered`, which closes the listeners already serving.
            let listener = (listener.set_nonblocking(true))
                .and_then(|()| TcpListener::from_std(listener))
                .map_err(OfferError::Copying)?;
            let errors = Arc::clone(&offered.accept_errors);
            let task = handle.spawn(serve(listener, port.host_port, errors));
            offered.tasks.0.push(task.abort_handle());
        }
        Ok(offered)
    }

    /// The handle of the runtime of the copying, made now if it is not yet.
    fn handle(&self) -> io::Result<Handle> {
        let mut runtime = self.runtime.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(runtime) = &*runtime {
            return Ok(runtime.handle().clone());
        }
        let mut builder = Builder::new_multi_thread();
        let made = builder.worker_threads(THREADS).thread_name("js-ports");
        let made = made.enable_all().build()?;
        Ok(runtime.insert(made).handle().clone())
    }
}

/// A run's host ports while they are offered: dropping it closes the listeners and every
/// connection through them.
#[derive(Debug, Default)]
pub struct Offered {
    tasks: Aborting,
    /// How many accepts failed, over all the listeners.
    accept_errors: Arc<AtomicU64>,
}

impl Offered {
    /// How many times a listener of the run could not accept a connection: the first was
    /// logged, and each was followed by a pause and a new accept.
    #[cfg(test)]
    pub(crate) fn accept_errors(&self) -> u64 {
        self.accept_errors.load(Ordering::Relaxed)
    }
}

/// Tasks that are ended when this is dropped.
#[derive(Debug, Default)]
struct Aborting(Vec<AbortHandle>);

impl Drop for Aborting {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Accepts the job's connections on `listener` and joins each to the host's `host_port`, for
/// as long as the task runs: ending it closes the listener and every connection it joined.
/// A failed accept is counted in `errors`, logged the first time, and tried again after
/// [`ACCEPT_PAUSE`].
async fn serve(listener: TcpListener, host_port: u16, errors: Arc<AtomicU64>) {
    let host = SocketAddrV4::new(Ipv4Addr::LOCALHOST, host_port);
    let mut joined = Aborting::default();
    // Declared after `joined`, so dropped before it: the listener is closed before the
    // connections it joined are ended, and no job connects again in between.
    let listener = listener;
    loop {
        match listener.accept().await {
            Ok((job, _)) => {
                // What ended is dropped, so a job that makes many connections holds no more
                // than it has open.
                joined.0.retain(|task| !task.is_finished());
                joined.0.push(tokio::spawn(join(job, host)).abort_handle());
            }
            Err(why) => {
                if errors.fetch_add(1, Ordering::Relaxed) == 0 {
                    tracing::warn!(host_port, "accept a job's connection to a host port: {why}");
                }
                tokio::time::sleep(ACCEPT_PAUSE).await;
            }
        }
    }
}

/// Joins the job's connection `job` to a new connection to `host`, copying both ways until
/// both are closed. A connection to `host` that fails closes `job`.
async fn join(mut job: TcpStream, host: SocketAddrV4) {
    let Ok(mut host) = TcpStream::connect(host).await else {
        return;
    };
    // An error on either side ends both; the agent has nothing to tell the job beyond that.
    let _ = tokio::io::copy_bidirectional(&mut job, &mut host).await;
}
