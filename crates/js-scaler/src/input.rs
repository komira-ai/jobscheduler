//! What one step reads: demand, pools and their offers, hosts, and the answers the
//! caller got from the provider since the last step.

use js_elastic::{AcquireId, Acquired, Demand, Held, Offer, Released, SchedulerId};
use js_types::{HostId, RequestId};

use crate::settings::{Health, Settings};

/// One whole-node request on the registry's wait list (`js_registry::Waiting`), in queue
/// order. The registry's `since` is not here: no rule of the step reads how long a request
/// has waited.
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

/// How a host is set aside (`js_registry::HostMark`, variant for variant).
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

/// Whether the registry hears from a host (`js_registry::HostState`, variant for variant).
///
/// Only `Live` is acted on: a host in either other state covers no request, is never
/// drained and is never released on its silence. The registry keeps the host's booking in
/// both, and so does the scaler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    /// Said hello or heartbeat within grace.
    Live,
    /// Silent past grace. Silence alone releases nothing.
    Lost,
    /// Put back after a restart of the registry and not heard from since: its booking is
    /// held and cannot be seen, so it is treated as `Lost`.
    Awaiting,
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
    /// Whether the registry books nothing on the host: its lease grace gave up the leases
    /// it held, or it held none. The shell sets it for a silent host from the lease table,
    /// and it is `true` for a silent host that has no bookings at all (one that never held
    /// a lease included); it is `false` for a host that still holds a lease or whose
    /// bookings the shell cannot read. A silent host marked `Releasing` needs no flag: the
    /// registry booked nothing on it when it set the mark. The scaler also reads `busy` and
    /// `mark` and never releases a host that shows work, whatever this says.
    pub lease_given_up: bool,
    /// Whether the registry has heard a heartbeat from the host since its hello. The shell
    /// sets it from the registry's record of the host; it is `false` only for a host that
    /// said hello and has not been heard from since (a host whose Welcome was lost cannot
    /// heartbeat, for it never got the credential), and `true` for any host that has
    /// heartbeated at least once, and for a host the shell cannot tell about.
    pub heard_since_hello: bool,
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
    /// The acquire ids the provider holds an unresolved call for: an earlier call of the
    /// acquire got no answer and may have landed (a provider that answers `Deferred` for
    /// such an acquire does so with this set). The shell fills it from the provider; a
    /// `Planned` record with one of these ids is kept when no request waits for it, so its
    /// retry settles the call instead of leaving a machine nobody counts.
    pub unresolved: Vec<AcquireId>,
}

/// The provider's answer to an acquire the caller ran for an earlier step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireResult {
    /// The acquire it answers.
    pub acquire: AcquireId,
    /// The attempt it answers: [`AcquirePlan::attempt`](crate::AcquirePlan::attempt) of
    /// the call that was made. A `Deferred` for an earlier attempt than the record's is a
    /// replay and is ignored.
    pub attempt: u32,
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
    /// Whether `hosts` is the registry's whole, settled view: every host it holds, read
    /// after it finished loading its state. The shell sets it `false` for a view it cannot
    /// vouch for (the registry has just started, a read failed or was partial). A machine
    /// missing from `hosts` is released only on a complete view, seen missing twice at
    /// least `unknown_hold` apart, and a booting machine is given up on at `boot_timeout`
    /// only on a complete view (its host may have said hello and be missing from a partial
    /// one); on an incomplete one everything is held and counted.
    pub registry_complete: bool,
    /// Answers to acquires.
    pub acquired: Vec<AcquireResult>,
    /// Answers to releases.
    pub released: Vec<ReleaseResult>,
    /// Host names of machines a read by id found the provider no longer has
    /// (`get_instance` answered not found). Read with a host the registry does not hear
    /// from, it settles the machine: revoked, forgotten and removed.
    pub not_found: Vec<String>,
    /// Random bytes for the acquire ids of this step's launches, one value for each
    /// launch the step may make (the sum of the pools' `max_step`). The step has no
    /// randomness of its own; the caller draws these from a source of entropy and never
    /// repeats one.
    pub entropy: Vec<[u8; 16]>,
}
