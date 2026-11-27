//! The fake on its socket: the provider service (`jobscheduler.provider.v1.MountProvider`) on
//! a Unix socket, answered by the [`Store`], and the trees it places under its root.
//!
//! A placed tree is a directory `<root>/<agent>/<run>/<index>` holding one empty file,
//! [`READY_FILE`], so a job that mounts it can check that it sees the tree. A `Mount` that
//! the store makes wait is held until the test changes the mode or the wait ends, and then
//! asked again. A restart answers the `Mount` calls held `UNAVAILABLE` at once, closes the
//! socket, waits the restart pause, forgets every source and binds the socket again, as a
//! provider that restarts; a quit answers them so too, and ends.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use js_proto::provider::v1::mount_provider_server::{MountProvider, MountProviderServer};
use js_proto::provider::v1::{
    MountRequest, MountResponse, PrepareRequest, PrepareResponse, ReleaseRequest, ReleaseResponse,
    Source, SweepRequest, SweepResponse,
};
use tokio::net::UnixListener;
use tokio::sync::{Notify, oneshot, watch};
use tonic::codegen::tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use crate::model::{Key, Mounted, Refusal, Store};

/// The file every placed tree holds, relative to the tree.
pub(crate) const READY_FILE: &str = "input/ready";

/// What the control socket tells the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Signal {
    /// Serve; the number counts the restarts asked for.
    Run(u64),
    /// Close the socket and end.
    Quit,
}

/// What the service and the control socket share.
pub(crate) struct Shared {
    store: Mutex<Store>,
    changed: Notify,
    waiting: AtomicUsize,
    cuts: AtomicU64,
    started: Instant,
    root: PathBuf,
    wait: Duration,
}

impl Shared {
    /// The fake over `store`, placing trees under `root`, holding a stalled `Mount` for `wait`.
    pub(crate) fn new(store: Store, root: PathBuf, wait: Duration) -> Arc<Shared> {
        Arc::new(Shared {
            store: Mutex::new(store),
            changed: Notify::new(),
            waiting: AtomicUsize::new(0),
            cuts: AtomicU64::new(0),
            started: Instant::now(),
            root,
            wait,
        })
    }

    /// The store, locked.
    pub(crate) fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wakes the calls the store made wait, to be asked again.
    pub(crate) fn changed(&self) {
        self.changed.notify_waiters();
    }

    /// Answers every `Mount` held now `UNAVAILABLE`, as a provider that stops does.
    pub(crate) fn cut(&self) {
        self.cuts.fetch_add(1, Ordering::SeqCst);
        self.changed();
    }

    /// The `Mount` calls the store makes wait now.
    pub(crate) fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    /// Milliseconds since the fake started.
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Counts a held `Mount` among the waiting while it lives.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn new(count: &'a AtomicUsize) -> Waiting<'a> {
        count.fetch_add(1, Ordering::SeqCst);
        Waiting(count)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The key of a request's source; a request without one is the empty source.
fn key(source: Option<Source>) -> Key {
    let source = source.unwrap_or_default();
    (source.agent, source.run, source.index)
}

/// The gRPC status of a refusal.
fn status(refusal: Refusal) -> Status {
    match refusal {
        Refusal::Unavailable(why) => Status::unavailable(why),
        Refusal::Invalid(why) => Status::invalid_argument(why),
        Refusal::Precondition(why) => Status::failed_precondition(why),
    }
}

/// Makes the tree at `relative` under `root` and answers its path.
fn place(root: &Path, relative: &str) -> Result<Response<MountResponse>, Status> {
    let tree = root.join(relative);
    let ready = tree.join(READY_FILE);
    let made = ready
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(&ready, b""));
    made.map_err(|e| Status::internal(format!("place {}: {e}", tree.display())))?;
    let path = tree.display().to_string();
    Ok(Response::new(MountResponse { path }))
}

/// The service: every call goes through the store.
struct Service(Arc<Shared>);

#[tonic::async_trait]
impl MountProvider for Service {
    async fn prepare(
        &self,
        request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        let request = request.into_inner();
        let at = self.0.now_ms();
        let answer = self
            .0
            .store()
            .prepare(at, key(request.source), &request.r#ref);
        answer
            .map(|()| Response::new(PrepareResponse {}))
            .map_err(status)
    }

    async fn mount(
        &self,
        request: Request<MountRequest>,
    ) -> Result<Response<MountResponse>, Status> {
        let key = key(request.into_inner().source);
        let at = self.0.now_ms();
        let deadline = tokio::time::Instant::now() + self.0.wait;
        let cuts = self.0.cuts.load(Ordering::SeqCst);
        loop {
            // Made before the store is asked, so a change between the two wakes the wait.
            let changed = self.0.changed.notified();
            let waited = tokio::time::Instant::now() >= deadline;
            if self.0.cuts.load(Ordering::SeqCst) != cuts {
                return Err(status(self.0.store().cut_mount(at)));
            }
            let answer = self.0.store().mount(at, &key, waited);
            match answer.map_err(status)? {
                Mounted::Placed(relative) => return place(&self.0.root, &relative),
                Mounted::Wait => {
                    let _waiting = Waiting::new(&self.0.waiting);
                    let _ = tokio::time::timeout_at(deadline, changed).await;
                }
            }
        }
    }

    async fn release(
        &self,
        request: Request<ReleaseRequest>,
    ) -> Result<Response<ReleaseResponse>, Status> {
        let key = key(request.into_inner().source);
        let at = self.0.now_ms();
        let answer = self.0.store().release(at, &key);
        answer
            .map(|()| Response::new(ReleaseResponse {}))
            .map_err(status)
    }

    async fn sweep(
        &self,
        request: Request<SweepRequest>,
    ) -> Result<Response<SweepResponse>, Status> {
        let request = request.into_inner();
        let at = self.0.now_ms();
        let answer = self.0.store().sweep(at, &request.agent, &request.keep);
        answer
            .map(|released| Response::new(SweepResponse { released }))
            .map_err(status)
    }
}

/// A listener on `socket`, a stale socket file removed first, mode 0600.
fn bind(socket: &Path) -> io::Result<UnixListener> {
    let _ = fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serves `shared` on `socket` until `signal` says quit, and says on `bound` once the socket
/// is first bound; each restart closes the socket, waits `pause`, forgets every source and
/// binds again.
pub(crate) async fn serve(
    shared: Arc<Shared>,
    socket: PathBuf,
    pause: Duration,
    mut signal: watch::Receiver<Signal>,
    bound: oneshot::Sender<()>,
) -> io::Result<()> {
    let mut listener = bind(&socket)?;
    let _ = bound.send(());
    loop {
        let mut stop = signal.clone();
        let service = MountProviderServer::new(Service(Arc::clone(&shared)));
        let stopped = async move {
            let _ = stop.changed().await;
        };
        Server::builder()
            .serve_with_incoming_shutdown(service, UnixListenerStream::new(listener), stopped)
            .await
            .map_err(io::Error::other)?;
        let _ = fs::remove_file(&socket);
        if *signal.borrow_and_update() == Signal::Quit {
            return Ok(());
        }
        tokio::time::sleep(pause).await;
        let at = shared.now_ms();
        shared.store().restart(at);
        listener = bind(&socket)?;
    }
}

#[cfg(test)]
#[path = "serve_tests.rs"]
mod tests;
