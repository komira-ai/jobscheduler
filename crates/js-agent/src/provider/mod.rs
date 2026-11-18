//! The agent's client of its node-local mount providers (`--mount-provider NAME=SOCKET`,
//! `docs/design/mount-providers.md`): one gRPC channel for each provider, over the Unix
//! socket the operator named, and the calls of [`crate::lifecycle::ProviderCall`] carried to
//! it and back without the lifecycle's worker thread waiting.
//!
//! - [`ProviderClient`] implements [`Providers`]: `ask` sends a call the first time and
//!   answers `Pending` until the task that carries it has the answer, then hands the answer
//!   over once. It runs the calls on a tokio runtime of its own (two threads named
//!   `js-provide`), as the image pulls of `lifecycle::prefetch` do.
//! - Each channel connects lazily, on its first call, and again after a connection is lost.
//!   Before every connect it checks the socket file against the user the provider runs as
//!   (`--mount-provider-user`): a socket of that user or root that no other user can write
//!   (its group may), in a directory of root or that user that no one else can write
//!   (`socket`).
//! - Each call has a bound ([`CallBounds`]); a provider that does not answer within it is
//!   answered `Unavailable`, and the lifecycle asks again when it chooses. The client rings the
//!   lifecycle's wake at once for an answer it goes on from, and for a failed or refused one
//!   after the pause [`CallBounds::retry`], so a retry does not wait for the worker's next
//!   step and a provider that fails at once is asked at most once per pause.
//! - A reply of `INVALID_ARGUMENT` or `FAILED_PRECONDITION` is a refusal; every other failure
//!   is `Unavailable`.
//!
//! - The messages are the agent's own declarations of the protocol's ([`wire`]), so the
//!   agent does not link `js-proto`.
//!
//! The calls carry the agent's own id (the host id it registered under) in every `Source` and
//! in `SweepRequest.agent`, and `js.` before every run id.

mod call;
mod flight;
mod socket;
pub mod wire;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::{Builder, Handle, Runtime};
use tonic::transport::{Channel, Endpoint};

use crate::lifecycle::{ProviderAnswer, ProviderCall, Providers, Wake};
use flight::{Begun, Flights, Key};
use socket::UnixConnector;

/// How long each kind of call may take, from the moment it is sent to its answer, and the pause
/// after a failed one before the client rings the wake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallBounds {
    /// `Prepare`, which the provider answers at once and works on in the background.
    pub prepare: Duration,
    /// `Mount`, which waits until the tree can be opened.
    pub mount: Duration,
    /// `Release`.
    pub release: Duration,
    /// `Sweep`.
    pub sweep: Duration,
    /// The pause after a call that was refused or failed (or outlived its bound) before the
    /// client rings the lifecycle's wake, so the retry does not wait for the worker's next
    /// step. A call that succeeded rings at once.
    pub retry: Duration,
}

impl Default for CallBounds {
    /// The bounds of the provider contract: 10 s for `Prepare`, 30 s for `Mount`, 5 s for
    /// `Release` and 30 s for `Sweep`, and a retry pause of 200 ms.
    fn default() -> Self {
        Self {
            prepare: Duration::from_secs(10),
            mount: Duration::from_secs(30),
            release: Duration::from_secs(5),
            sweep: Duration::from_secs(30),
            retry: Duration::from_millis(200),
        }
    }
}

impl CallBounds {
    /// The bound of `call`'s kind.
    fn of(&self, call: &ProviderCall) -> Duration {
        match call {
            ProviderCall::Prepare { .. } => self.prepare,
            ProviderCall::Mount { .. } => self.mount,
            ProviderCall::Release { .. } => self.release,
            ProviderCall::Sweep { .. } => self.sweep,
        }
    }
}

/// The client of every mount provider of one agent, on a runtime of its own.
///
/// Dropping it stops the runtime without waiting for the calls under way (tokio's
/// `shutdown_background`), so it may be dropped anywhere, an async task included.
pub struct ProviderClient {
    /// The id the agent's host registered under.
    agent: String,
    bounds: CallBounds,
    /// One lazily connected channel for each provider, by name.
    clients: BTreeMap<String, Channel>,
    flights: Arc<Flights>,
    /// Rung when a call succeeded, and a pause after one failed ([`Providers::set_wake`]).
    wake: Wake,
    /// Kept to be shut down on drop; calls are spawned through `handle`.
    runtime: Option<Runtime>,
    handle: Handle,
}

impl ProviderClient {
    /// A client of the providers `sockets` names (name, socket path and the uid the provider
    /// runs as, whose socket it is), for the agent whose host id is `host_id`. The bounds are
    /// [`CallBounds::default`]. No socket is opened until a call is asked of its provider.
    ///
    /// # Errors
    /// The runtime could not be made (threads or descriptors are exhausted).
    pub fn new(host_id: &str, sockets: &[(String, PathBuf, u32)]) -> std::io::Result<Self> {
        Self::with_bounds(host_id, sockets, CallBounds::default())
    }

    /// [`ProviderClient::new`] with the bounds `bounds`.
    ///
    /// # Errors
    /// As [`ProviderClient::new`].
    pub fn with_bounds(
        host_id: &str,
        sockets: &[(String, PathBuf, u32)],
        bounds: CallBounds,
    ) -> std::io::Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("js-provide")
            .enable_all()
            .build()?;
        let clients = {
            // A channel starts its worker on the runtime it is made in.
            let _entered = runtime.enter();
            sockets
                .iter()
                .map(|(name, socket, user)| {
                    let channel = Endpoint::from_static("http://provider.invalid")
                        .connect_with_connector_lazy(UnixConnector::new(socket.clone(), *user));
                    (name.clone(), channel)
                })
                .collect()
        };
        Ok(Self {
            agent: host_id.to_owned(),
            bounds,
            clients,
            flights: Arc::new(Flights::default()),
            wake: Wake::default(),
            handle: runtime.handle().clone(),
            runtime: Some(runtime),
        })
    }

    /// Blocks until the answer of `call` to `provider` is in (it is then handed over by the
    /// next [`Providers::ask`]), or until `guard` has passed: whether it is in. For a test,
    /// which asks, waits and asks again instead of sleeping; `guard` only ends a wait that
    /// would otherwise hang a failing test, so it is far above any answer. A call that was
    /// never asked, or whose answer was already handed over, is not answered by waiting.
    #[must_use]
    pub fn wait_answered(&self, provider: &str, call: &ProviderCall, guard: Duration) -> bool {
        self.flights
            .wait_answered(&flight_key(provider, call), guard)
    }
}

/// What tells `call` to `provider` apart from every other.
fn flight_key(provider: &str, call: &ProviderCall) -> Key {
    let (kind, run, index) = call.key();
    (provider.to_owned(), kind, run.to_owned(), index)
}

impl Providers for ProviderClient {
    fn has(&self, name: &str) -> bool {
        self.clients.contains_key(name)
    }

    /// The names in alphabetical order.
    fn names(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }

    fn ask(&mut self, provider: &str, call: &ProviderCall) -> ProviderAnswer {
        let Some(client) = self.clients.get(provider) else {
            return ProviderAnswer::Unavailable(format!(
                "this agent was given no mount provider named {provider}"
            ));
        };
        let key = flight_key(provider, call);
        let (handed, sent) = match self.flights.begin(key.clone()) {
            Begun::Settled(answer) => return answer,
            Begun::Send(sent) => (ProviderAnswer::Pending, sent),
            Begun::Retry(failed, sent) => (failed, sent),
        };
        let task = call::run(
            client.clone(),
            self.agent.clone(),
            call.clone(),
            self.bounds.of(call),
        );
        let (flights, wake, retry) = (
            Arc::clone(&self.flights),
            self.wake.clone(),
            self.bounds.retry,
        );
        self.handle.spawn(async move {
            let answer = task.await;
            // An answer the lifecycle goes on from rings at once. A refused or failed call
            // rings after the retry pause, once its answer is stored: ringing for it at once
            // would ask a provider that fails at once again and again without a pause. The
            // step the ring brings is handed the failure and, for an unavailable one, sends
            // the call again in the same ask.
            let goes_on = matches!(answer, ProviderAnswer::Done(_));
            flights.finish(key.clone(), sent, answer);
            if !goes_on {
                tokio::time::sleep(retry).await;
                flights.rest(&key, sent);
            }
            wake.wake();
        });
        handed
    }

    fn set_wake(&mut self, wake: Wake) {
        self.wake = wake;
    }
}

impl Drop for ProviderClient {
    fn drop(&mut self) {
        self.runtime
            .take()
            .into_iter()
            .for_each(Runtime::shutdown_background);
    }
}
