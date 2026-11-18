//! A node-local mount provider (`jobscheduler.provider.v1.MountProvider`, feature
//! `mount-provider`) served on a Unix socket in this process, for the tests of a crate that
//! asks one.
//!
//! [`FakeProvider::start`] binds `mp.sock` in a directory the test gives and serves on a
//! runtime thread of its own, so a test that is a plain function (the agent's lifecycle
//! polls and never waits on an executor) can use it. The fake keeps the contract of
//! `docs/design/mount-providers.md`:
//! - `Prepare` is idempotent: a source (agent, run, index) prepared again with the same ref
//!   succeeds, with another ref it is refused `FAILED_PRECONDITION`.
//! - `Mount` answers [`mount_path`] of a prepared source, and `FAILED_PRECONDITION` for one
//!   that was not prepared.
//! - `Release` of a source the fake does not know succeeds.
//! - `Check` answers the cause a test scripted with [`FakeProvider::lose`] for a source, and
//!   an empty one for every other source (also one the fake does not hold).
//! - `Sweep` releases the sources of the agent it names whose run is not in `keep`, and
//!   answers how many; another agent's sources stay.
//!
//! A test scripts it: [`FakeProvider::refuse`] a ref (`INVALID_ARGUMENT`),
//! [`FakeProvider::fail_mount`] (`UNAVAILABLE` for the next calls),
//! [`FakeProvider::fail_release`] (`INTERNAL` for the next calls),
//! [`FakeProvider::lose`] a source (what `Check` answers for it),
//! [`FakeProvider::leave_out_check`] (`Check` answers `UNIMPLEMENTED`), and
//! [`FakeProvider::hold`] a kind of call, which then never answers until
//! [`FakeProvider::unhold`] ([`FakeProvider::wait_parked`] waits until a held call has
//! arrived). [`FakeProvider::requests`] is every request the fake received, in
//! order, and [`FakeProvider::prepared`] the sources it holds.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_proto::provider::v1::mount_provider_server::{MountProvider, MountProviderServer};
use js_proto::provider::v1::{
    CheckRequest, CheckResponse, MountRequest, MountResponse, PrepareRequest, PrepareResponse,
    ReleaseRequest, ReleaseResponse, Source, SweepRequest, SweepResponse,
};
use tokio::net::UnixListener;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Notify;
use tonic::codegen::tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

/// The name of the socket file in the directory the test gives. Short, because a socket's
/// path is at most about 108 bytes.
const SOCKET: &str = "mp.sock";

/// The kinds of call of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Prepare,
    Mount,
    Release,
    Check,
    Sweep,
}

/// One request the fake received.
#[derive(Debug, Clone, PartialEq)]
pub enum Seen {
    Prepare(PrepareRequest),
    Mount(MountRequest),
    Release(ReleaseRequest),
    Check(CheckRequest),
    Sweep(SweepRequest),
}

/// The directory the fake answers to `Mount` for a source: `/trees/<agent>/<run>/<index>`.
#[must_use]
pub fn mount_path(agent: &str, run: &str, index: u32) -> String {
    format!("/trees/{agent}/{run}/{index}")
}

/// What the fake knows and what the test scripted.
#[derive(Default)]
struct Book {
    seen: Vec<Seen>,
    /// The sources prepared and not released, by (agent, run, index), with their refs.
    sources: BTreeMap<(String, String, u32), String>,
    refused: BTreeSet<String>,
    mount_failures: u32,
    release_failures: u32,
    /// The sources `Check` answers a cause for, by (run, index), with the cause.
    lost: BTreeMap<(String, u32), String>,
    /// Whether `Check` answers `UNIMPLEMENTED`, as a provider from before it does.
    check_left_out: bool,
    held: BTreeSet<Kind>,
    /// The held calls waiting to be let go. A call whose client gave up stays counted.
    parked: u32,
}

#[derive(Default)]
struct Shared {
    book: Mutex<Book>,
    /// Woken when a kind of call is no longer held.
    changed: Notify,
    /// Woken when a call is parked.
    parking: Condvar,
}

impl Shared {
    fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a request, then waits for as long as its kind is held.
    async fn arrive(&self, kind: Kind, seen: Seen) {
        self.book().seen.push(seen);
        loop {
            // Made before the check, so an `unhold` between the check and the wait wakes it.
            let changed = self.changed.notified();
            {
                let mut book = self.book();
                if !book.held.contains(&kind) {
                    return;
                }
                book.parked += 1;
            }
            self.parking.notify_all();
            changed.await;
            self.book().parked -= 1;
        }
    }
}

/// The key of a request's source; a request without one is the empty source.
fn key(source: Option<Source>) -> (String, String, u32) {
    let source = source.unwrap_or_default();
    (source.agent, source.run, source.index)
}

/// The service: every call goes through `Shared`.
struct Fake {
    shared: Arc<Shared>,
}

#[tonic::async_trait]
impl MountProvider for Fake {
    async fn prepare(
        &self,
        request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        let request = request.into_inner();
        self.shared
            .arrive(Kind::Prepare, Seen::Prepare(request.clone()))
            .await;
        let mut book = self.shared.book();
        if book.refused.contains(&request.r#ref) {
            return Err(Status::invalid_argument(format!(
                "the fake provider refuses the ref {}",
                request.r#ref
            )));
        }
        match book.sources.entry(key(request.source)) {
            Entry::Vacant(slot) => {
                slot.insert(request.r#ref);
            }
            Entry::Occupied(slot) if *slot.get() != request.r#ref => {
                return Err(Status::failed_precondition(format!(
                    "the source is prepared with the ref {}",
                    slot.get()
                )));
            }
            Entry::Occupied(_) => {}
        }
        Ok(Response::new(PrepareResponse {}))
    }

    async fn mount(
        &self,
        request: Request<MountRequest>,
    ) -> Result<Response<MountResponse>, Status> {
        let request = request.into_inner();
        self.shared
            .arrive(Kind::Mount, Seen::Mount(request.clone()))
            .await;
        let mut book = self.shared.book();
        if book.mount_failures > 0 {
            book.mount_failures -= 1;
            return Err(Status::unavailable("the fake provider is not ready"));
        }
        let source = key(request.source);
        if !book.sources.contains_key(&source) {
            return Err(Status::failed_precondition("the source was not prepared"));
        }
        Ok(Response::new(MountResponse {
            path: mount_path(&source.0, &source.1, source.2),
        }))
    }

    async fn release(
        &self,
        request: Request<ReleaseRequest>,
    ) -> Result<Response<ReleaseResponse>, Status> {
        let request = request.into_inner();
        self.shared
            .arrive(Kind::Release, Seen::Release(request.clone()))
            .await;
        let mut book = self.shared.book();
        if book.release_failures > 0 {
            book.release_failures -= 1;
            return Err(Status::internal("the fake provider failed to release"));
        }
        book.sources.remove(&key(request.source));
        Ok(Response::new(ReleaseResponse {}))
    }

    async fn check(
        &self,
        request: Request<CheckRequest>,
    ) -> Result<Response<CheckResponse>, Status> {
        let request = request.into_inner();
        self.shared
            .arrive(Kind::Check, Seen::Check(request.clone()))
            .await;
        let (_, run, index) = key(request.source);
        let book = self.shared.book();
        if book.check_left_out {
            return Err(Status::unimplemented("the fake provider has no Check"));
        }
        let lost = book.lost.get(&(run, index)).cloned().unwrap_or_default();
        Ok(Response::new(CheckResponse { lost }))
    }

    async fn sweep(
        &self,
        request: Request<SweepRequest>,
    ) -> Result<Response<SweepResponse>, Status> {
        let request = request.into_inner();
        self.shared
            .arrive(Kind::Sweep, Seen::Sweep(request.clone()))
            .await;
        let mut book = self.shared.book();
        let before = book.sources.len();
        book.sources
            .retain(|(agent, run, _), _| *agent != request.agent || request.keep.contains(run));
        Ok(Response::new(SweepResponse {
            released: u32::try_from(before - book.sources.len()).unwrap_or(u32::MAX),
        }))
    }
}

/// A mount provider on a Unix socket, served until it is dropped.
pub struct FakeProvider {
    socket: PathBuf,
    shared: Arc<Shared>,
    /// Kept to be shut down on drop.
    runtime: Option<Runtime>,
}

impl FakeProvider {
    /// Binds `mp.sock` in `dir` and serves on a runtime thread of its own. `dir` must exist,
    /// and `dir` and the socket's name must fit in a socket path (about 108 bytes).
    ///
    /// # Errors
    /// The runtime could not be made or the socket could not be bound.
    pub fn start(dir: &Path) -> io::Result<Self> {
        let socket = dir.join(SOCKET);
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_io()
            .build()?;
        let listener = {
            // A listener registers with the runtime it is made in.
            let _entered = runtime.enter();
            UnixListener::bind(&socket)?
        };
        let shared = Arc::new(Shared::default());
        let server = Server::builder().serve_with_incoming(
            MountProviderServer::new(Fake {
                shared: Arc::clone(&shared),
            }),
            UnixListenerStream::new(listener),
        );
        drop(runtime.spawn(server));
        Ok(Self {
            socket,
            shared,
            runtime: Some(runtime),
        })
    }

    /// The socket the fake listens on.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Every request received so far, in the order the fake began to handle them.
    #[must_use]
    pub fn requests(&self) -> Vec<Seen> {
        self.shared.book().seen.clone()
    }

    /// The sources the fake holds (prepared and not released), by (agent, run, index), with
    /// the ref each was prepared with.
    #[must_use]
    pub fn prepared(&self) -> BTreeMap<(String, String, u32), String> {
        self.shared.book().sources.clone()
    }

    /// From now on a `Prepare` of `reference` is refused `INVALID_ARGUMENT`.
    pub fn refuse(&self, reference: &str) {
        self.shared.book().refused.insert(reference.to_owned());
    }

    /// The next `times` calls of `Mount` answer `UNAVAILABLE`.
    pub fn fail_mount(&self, times: u32) {
        self.shared.book().mount_failures = times;
    }

    /// The next `times` calls of `Release` answer `INTERNAL`, and release nothing.
    pub fn fail_release(&self, times: u32) {
        self.shared.book().release_failures = times;
    }

    /// From now on `Check` answers `cause` for the source at `index` of the run `run` (as the
    /// provider sees the run, `js.` and the job's id), whichever agent asks.
    pub fn lose(&self, run: &str, index: u32, cause: &str) {
        self.shared
            .book()
            .lost
            .insert((run.to_owned(), index), cause.to_owned());
    }

    /// From now on `Check` is answered `UNIMPLEMENTED`, as a provider from before it does.
    pub fn leave_out_check(&self) {
        self.shared.book().check_left_out = true;
    }

    /// From now on a call of `kind` is recorded and then not answered, until
    /// [`FakeProvider::unhold`].
    pub fn hold(&self, kind: Kind) {
        self.shared.book().held.insert(kind);
    }

    /// Blocks until `calls` held calls are parked (received, and waiting to be let go), for at
    /// most `guard`; whether they are. A test holds a kind, asks, and waits here before it
    /// lets the call go, instead of sleeping. `guard` only ends a wait that would hang a
    /// failing test. A call whose client gave up (a bound that ended it) stays counted.
    #[must_use]
    pub fn wait_parked(&self, calls: u32, guard: Duration) -> bool {
        let (book, result) = self
            .shared
            .parking
            .wait_timeout_while(self.shared.book(), guard, |book| book.parked < calls)
            .unwrap_or_else(PoisonError::into_inner);
        drop(book);
        !result.timed_out()
    }

    /// Lets the calls of `kind` held, and those to come, be answered.
    pub fn unhold(&self, kind: Kind) {
        self.shared.book().held.remove(&kind);
        self.shared.changed.notify_waiters();
    }
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.runtime
            .take()
            .into_iter()
            .for_each(Runtime::shutdown_background);
    }
}
