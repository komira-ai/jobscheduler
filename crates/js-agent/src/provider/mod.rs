//! The agent's client of its node-local mount providers (`--mount-provider NAME=SOCKET`,
//! `docs/design/mount-providers.md`): one gRPC channel for each provider, over the Unix
//! socket the operator named, and the calls of [`crate::lifecycle::ProviderCall`] carried to
//! it and back without the lifecycle's worker thread waiting.
//!
//! - [`ProviderClient`] implements [`Providers`]: `ask` sends a call the first time and
//!   answers `Pending` until the task that carries it has the answer, then hands the answer
//!   over once. It runs the calls on a tokio runtime of its own (two threads named
//!   `js-provide`), as the image pulls of `lifecycle::prefetch` do.
//! - A `Prepare` that succeeds is followed by the `Mount` of its source, which the client sends
//!   itself in the task that carried the `Prepare`, once its answer is in: the `Mount` does not
//!   wait for the worker's next step, and the provider still sees the `Prepare` answered first.
//!   The lifecycle's ask of that `Mount` finds it out or answered. A `Release` of the source
//!   forgets its `Prepare` and `Mount`, so a `Prepare` still out is not followed after it.
//! - Each channel connects lazily, on its first call, and again after a connection is lost.
//!   Before every connect it checks the socket file against the user the provider runs as
//!   (`--mount-provider-user`): a socket of that user or root that no other user can write
//!   (its group may), in a directory of root or that user that no one else can write
//!   (`socket`).
//! - Each call has a bound ([`CallBounds`]); a provider that does not answer within it is
//!   answered `Unavailable`, and the lifecycle asks again when it chooses. The client rings the
//!   lifecycle's wake at once for an answer it goes on from, and for a failed or refused one
//!   after the pause [`CallBounds::retry`]. A failed answer is handed over like any other and
//!   sends nothing with it: the lifecycle chooses when to send the call again, after a wait
//!   that grows with each failure in a row. A `Prepare` that is followed by its `Mount` rings
//!   nothing of its own; the `Mount`'s answer rings.
//! - A reply of `INVALID_ARGUMENT` or `FAILED_PRECONDITION` is a refusal; every other failure
//!   is `Unavailable`.
//!
//! - The messages are the agent's own declarations of the protocol's ([`wire`]), so the
//!   agent does not link `js-proto`.
//!
//! The calls carry the agent's own id (the host id it registered under) in every `Source` and
//! in `SweepRequest.agent`, and `js.` before every run id.

mod call;
pub(crate) use call::run_name;
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
    /// client rings the lifecycle's wake, so the step that takes the failure does not wait for
    /// the worker's next step. The call is not sent again by the ring: the lifecycle sends it
    /// when its own wait after the failure has passed. A call that succeeded rings at once.
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

/// The key of the `Prepare` of the source `index` of `run_id` to `provider`, whatever its ref
/// and mode (which [`ProviderCall::key`] leaves out).
fn prepare_key(provider: &str, run_id: &str, index: u32) -> Key {
    let call = ProviderCall::Prepare {
        run_id: run_id.to_owned(),
        index,
        reference: String::new(),
        mode: js_agent_wire::MountMode::ReadOnly,
    };
    flight_key(provider, &call)
}

/// The `Mount` of the source `index` of `run_id`.
fn mount_of(run_id: &str, index: u32) -> ProviderCall {
    ProviderCall::Mount {
        run_id: run_id.to_owned(),
        index,
    }
}

/// The call the client sends itself once `call` has succeeded: a source's `Mount` after its
/// `Prepare`, so the `Mount` does not wait for the lifecycle's next step. It is sent only after
/// the `Prepare`'s answer is in, so the provider sees the two in that order.
fn then(call: &ProviderCall) -> Option<ProviderCall> {
    match call {
        ProviderCall::Prepare { run_id, index, .. } => Some(mount_of(run_id, *index)),
        ProviderCall::Mount { .. } | ProviderCall::Release { .. } | ProviderCall::Sweep { .. } => {
            None
        }
    }
}

/// What the task that carries a call to one provider holds.
struct Carrier {
    channel: Channel,
    agent: String,
    provider: String,
    bounds: CallBounds,
    flights: Arc<Flights>,
    wake: Wake,
}

impl Carrier {
    /// Sends `call` (its entry `key`, out as the call `sent`), records its answer and, when it
    /// succeeded and is followed ([`then`]), sends the call that follows in the same task,
    /// and so on; then rings the wake once, for the last call's answer.
    ///
    /// An answer the lifecycle goes on from rings at once. A refused or failed call rings
    /// after the retry pause: the step the ring brings is handed the failure, and the
    /// lifecycle sends the call again when its wait after the failure has passed. A call that
    /// is followed rings nothing of its own: the lifecycle has nothing to go on with until the
    /// call that follows it answers.
    async fn carry(self, mut key: Key, mut sent: u64, mut call: ProviderCall) {
        loop {
            let bound = self.bounds.of(&call);
            let answer = call::run(
                self.channel.clone(),
                self.agent.clone(),
                call.clone(),
                bound,
            )
            .await;
            let goes_on = matches!(answer, ProviderAnswer::Done(_));
            let follow = then(&call).map(|next| (flight_key(&self.provider, &next), next));
            let then_key = follow.as_ref().map(|(next_key, _)| next_key.clone());
            match self
                .flights
                .finish(key.clone(), sent, answer, then_key)
                .zip(follow)
            {
                Some((number, (next_key, next))) => {
                    key = next_key;
                    sent = number;
                    call = next;
                }
                None => {
                    if !goes_on {
                        tokio::time::sleep(self.bounds.retry).await;
                    }
                    self.wake.wake();
                    return;
                }
            }
        }
    }
}

impl Providers for ProviderClient {
    fn has(&self, name: &str) -> bool {
        self.clients.contains_key(name)
    }

    /// The names in alphabetical order.
    fn names(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }

    /// A `Release` first forgets the source's `Prepare` and `Mount`, so a `Prepare` still out
    /// is not followed by a `Mount` once the source is given up.
    fn ask(&mut self, provider: &str, call: &ProviderCall) -> ProviderAnswer {
        let Some(client) = self.clients.get(provider) else {
            return ProviderAnswer::Unavailable(format!(
                "this agent was given no mount provider named {provider}"
            ));
        };
        if let ProviderCall::Release { run_id, index } = call {
            let source = [
                prepare_key(provider, run_id, *index),
                flight_key(provider, &mount_of(run_id, *index)),
            ];
            self.flights.forget(&source);
        }
        let key = flight_key(provider, call);
        let sent = match self.flights.begin(key.clone()) {
            Begun::Settled(answer) => return answer,
            Begun::Send(sent) => sent,
        };
        let carrier = Carrier {
            channel: client.clone(),
            agent: self.agent.clone(),
            provider: provider.to_owned(),
            bounds: self.bounds,
            flights: Arc::clone(&self.flights),
            wake: self.wake.clone(),
        };
        self.handle.spawn(carrier.carry(key, sent, call.clone()));
        ProviderAnswer::Pending
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
