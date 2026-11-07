//! A host set aside for a waiting whole-node request, as the registry shows it
//! ([`Reservation`]), and the facts of each host the start's caps read ([`HostFacts`]).
//!
//! The placer keeps the marks (design `whole-node.md` sections 3, 4 and 6): which waiting
//! request holds which host, made by its pass ([`crate::Placer::whole_round`]) and read
//! back from it ([`crate::Placer::whole_reservations`]). A request never gives its host up:
//! a drain waits for the host's work to end, however long it runs, and the deadline only
//! says when the host is expected to be drained at the latest. The registry holds each
//! marked host for its request alone: a marked host takes no other request's start, except
//! as backfill, for work that ends by the end of its tail ([`crate::Placer::pick`]).

use js_lease::Timestamp;
use js_types::{HostId, RequestId};

use super::caps::Capability;

/// A host set aside for a request, as an operator sees it
/// ([`crate::Registry::reservations`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    /// The request the host is set aside for.
    pub request: RequestId,
    /// The host.
    pub host: HostId,
    /// When the host was set aside.
    pub at: Timestamp,
    /// When the host is expected to be drained at the latest; reaching it does not give the
    /// reservation up. Meaningless for an `assigned` reservation, which drains nothing.
    pub deadline: Timestamp,
    /// Whether the host was quiet when it was set aside: it drains nothing and waits for
    /// the request's next start.
    pub assigned: bool,
    /// The end of the host's tail when it was set aside, in milliseconds on the registry's
    /// clock: its leases' latest end, each cut to its bound. `None` for an `assigned`
    /// reservation, and for a host with a lease that had neither an estimate nor a bound.
    pub tail_end: Option<u64>,
}

/// One host as the start's caps and the placer's round read it.
#[derive(Debug, Clone)]
pub(crate) struct HostFacts {
    pub(crate) id: HostId,
    /// Capable, lost (counted, since it may come back) or tainted (never eligible).
    pub(crate) capability: Capability,
    /// Whether the host may take a new lease right now: live, serving, clean, quiet and
    /// holding no lease.
    pub(crate) open: bool,
    /// Whether a whole-node lease is committed on the host, and since when.
    pub(crate) held: Option<Timestamp>,
}
