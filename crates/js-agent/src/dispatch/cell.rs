//! What the session tells the poller: the route of the session it holds, and when to end.

use std::fmt;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_agent_wire::{HostCredential, Welcome};

use crate::session::CredentialState;

/// The session a poll belongs to, as its welcome gave it.
#[derive(Clone)]
pub struct Route {
    /// The host's id.
    pub host_id: String,
    /// The session epoch of the welcome.
    pub epoch: u64,
    /// The session secret of the welcome. A poll carries it as a heartbeat does.
    pub session: String,
    /// The host credential that authenticates the host, when the agent holds one: the one
    /// found in the state root at start, or the latest a welcome carried that the session
    /// adopted. `None` when the transport's own login authenticates the host.
    pub credential: Option<HostCredential>,
}

/// Every field but the secrets, which print as `".."`.
impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Route")
            .field("host_id", &self.host_id)
            .field("epoch", &self.epoch)
            .field("session", &"..")
            .field("credential", &self.credential.as_ref().map(|_| ".."))
            .finish()
    }
}

/// What both threads see.
struct State {
    /// Counts the welcomes: a route is told apart from the one before it by it.
    generation: u64,
    /// The route of the latest welcome, `None` when it did not say `dispatch_poll`.
    route: Option<Route>,
    /// The host credential in use, kept across the welcomes that carry none.
    credential: Option<HostCredential>,
    /// The agent is ending or draining: no poll is made any more.
    closed: bool,
}

/// The meeting place of the heartbeat loop, which publishes a route at every welcome, and
/// the poller, which waits for one. Every wait ends on an event (a welcome, the close); the
/// pause of a backoff also ends by its own time.
pub struct Cell {
    state: Mutex<State>,
    changed: Condvar,
}

impl Cell {
    /// A cell with no route. `credential` is the host credential the agent found in its
    /// state root at start, which its transports authenticate with from the first call.
    pub fn new(credential: Option<HostCredential>) -> Self {
        let state = State {
            generation: 0,
            route: None,
            credential,
            closed: false,
        };
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the welcome of host `host_id`'s session: the route to poll when it says
    /// [`Welcome::dispatch_poll`], none otherwise (which stops the poller). The credential
    /// the welcome carried counts when the session adopted it (`kept`: it was saved, or
    /// could not be saved but serves this process).
    pub fn publish(&self, host_id: &str, welcome: &Welcome, kept: CredentialState) {
        let mut state = self.lock();
        let adopted = match kept {
            CredentialState::Saved | CredentialState::Unsaved => welcome.host_credential.clone(),
            CredentialState::None | CredentialState::Refused => None,
        };
        state.credential = adopted.or_else(|| state.credential.take());
        state.generation = state.generation.saturating_add(1);
        state.route = welcome.dispatch_poll.then(|| Route {
            host_id: host_id.to_owned(),
            epoch: welcome.epoch,
            session: welcome.session.clone(),
            credential: state.credential.clone(),
        });
        drop(state);
        self.changed.notify_all();
    }

    /// Ends the poller: it takes no route from now on, and a pause ends at once.
    pub fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }

    /// The route of a generation later than `parked` (0 for any), with its generation, when
    /// there is one and the cell is open; never waits. A welcome without the flag withdraws
    /// the route.
    pub fn try_route(&self, parked: u64) -> Option<(u64, Route)> {
        Self::due(&self.lock(), parked)
    }

    /// [`Cell::try_route`], waiting for it: `None` once the cell is closed.
    pub fn wait_route(&self, parked: u64) -> Option<(u64, Route)> {
        let wait = |state: &mut State| !state.closed && Self::due(state, parked).is_none();
        let state = self.changed.wait_while(self.lock(), wait);
        Self::due(&state.unwrap_or_else(PoisonError::into_inner), parked)
    }

    fn due(state: &State, parked: u64) -> Option<(u64, Route)> {
        let route = state.route.clone().filter(|_| state.generation > parked);
        route
            .filter(|_| !state.closed)
            .map(|route| (state.generation, route))
    }

    /// Waits at most `limit`, and not at all past the close or a welcome later than
    /// `generation`: the backoff of the poller, which a new session or the end cuts short.
    pub fn pause(&self, limit: Duration, generation: u64) {
        let _ = self
            .changed
            .wait_timeout_while(self.lock(), limit, |state| {
                !state.closed && state.generation == generation
            });
    }
}
