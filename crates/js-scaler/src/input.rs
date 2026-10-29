//! What one step reads: demand, pools and their offers, hosts, and the answers the
//! caller got from the provider since the last step.

use js_elastic::{AcquireId, Acquired, Demand, Held, Offer, Released, SchedulerId};
use js_types::{HostId, RequestId};

use crate::settings::{Health, Settings};

/// One whole-node request on the registry's wait list (`js_registry::Waiting`), in queue
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// The request id.
    pub id: RequestId,
    /// What the request needs of a host.
    pub demand: Demand,
    /// The host set aside for it, free or busy, if it holds one. Such a request already
    /// has a host coming and is never given a machine.
    pub reserved: Option<HostId>,
}

/// How a host is set aside (`js_registry::HostMark`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Set aside for a waiting request.
    Reserved,
    /// A whole-node lease is committed on it.
    Held,
    /// Marked by the scaler: it takes no start of any kind.
    Draining,
    /// Marked by the scaler: it is being given back.
    Releasing,
}

/// Whether the registry hears from a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    /// Live.
    Live,
    /// Silent past grace. Silence alone releases nothing.
    Lost,
}

/// One host the registry knows, as the scaler needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    /// The host id. A launched machine registers under the name given to it, so a record
    /// is found by this.
    pub id: HostId,
    /// The pool the host belongs to.
    pub pool: String,
    /// Whether the host is live.
    pub state: HostState,
    /// The shape the host announced, in the form of an offer so one rule
    /// ([`Offer::fit`]) judges a host and an offer alike. Its price and availability are
    /// not read.
    pub shape: Offer,
    /// How the host is set aside, if it is.
    pub mark: Option<Mark>,
    /// Whether the host holds a lease, a run or a stream.
    pub busy: bool,
}

/// One pool: its settings, the state of its checks, its offers and what its provider
/// holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolInput {
    /// The pool's settings.
    pub settings: Settings,
    /// What the pool's checks say.
    pub health: Health,
    /// What the provider could launch now.
    pub offers: Vec<Offer>,
    /// The machines the provider lists. `None` when the list failed: that is never read as
    /// an empty list.
    pub held: Option<Vec<Held>>,
}

/// The provider's answer to an acquire the caller ran for an earlier step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireResult {
    /// The acquire it answers.
    pub acquire: AcquireId,
    /// The answer.
    pub answer: Acquired,
}

/// The provider's answer to a release the caller ran for an earlier step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseResult {
    /// The host name of the machine that was released.
    pub host: String,
    /// The answer, read back.
    pub answer: Released,
}

/// Everything one step reads, except the clock and the state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// The scheduler's id, which names the hosts it launches.
    pub scheduler: SchedulerId,
    /// The elastic pools, in the order requests fall through them.
    pub pools: Vec<PoolInput>,
    /// The wait list, in queue order.
    pub waiting: Vec<Waiting>,
    /// The hosts the registry knows.
    pub hosts: Vec<HostFacts>,
    /// Answers to acquires.
    pub acquired: Vec<AcquireResult>,
    /// Answers to releases.
    pub released: Vec<ReleaseResult>,
}
