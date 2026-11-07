use std::collections::BTreeMap;

use js_lease::Booking;
use js_provider::SeatError;
use js_provider::{HostCapacity, Observed, Pool, StartAck, StartSpec, StopAck};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass};

use crate::credential::Binding;
use crate::registry::Registry;

/// The [`Pool`] the lease table runs on for a pool of self-registered hosts: the
/// allocation port that books a lease on one of them. The machine-provider view of the
/// same hosts is [`crate::StaticCapacity`].
///
/// It answers from memory and never blocks, as the table calls it under its lock. The rules
/// a lease follows are the ones [`js_provider::HostBook`] holds for the static pool as well.
/// What differs is which host may be booked and what a read can say:
///
/// - **Start** books the host the registry's placer picks ([`crate::Placer::pick`]) among
///   those live (heard from within the grace period), serving, with room for the request
///   and whose booked shape fits it. A class that books part of a host is leasable: its
///   leases share a host that has room, and a host running its first lease takes a second.
///   A host that is lost, awaiting, draining, quarantined, tainted or in any other phase
///   than serving takes no new lease. With no host to book it answers `NoCapacity` for a
///   request the pool's shape fits, `NoFit` for one it cannot. A whole-node request
///   (`Admission::Whole`) that fits the pool answers `NoCapableHost` when no known host
///   could ever be given whole (none is untainted and of a fitting shape), and `Unknown`
///   (accepted, waiting) when its start carries a request id and some host could, however
///   busy; see [`crate::Registry::queue_position`]. Without a request id it answers
///   `NoCapacity` and the consumer retries. A request that used up its tries to get a host
///   ready answers `ExclusiveNotReady` once (that start removes its record, so the next
///   start of the request is a fresh one).
/// - **Stop** frees the booking at once and reads back `Absent`, as it does for a lost
///   host: the table asked for the end of the lease. A whole-node request the lease
///   presented and that still waits leaves the wait list, and the host set aside for it is
///   free again, at once.
/// - **Waits** says whether the lease's whole-node request is on the wait list: its
///   start answered `Unknown` because no host is free yet, not because a host was
///   committed. The table presents such a lease again on every tick.
/// - **Observe** says `Present` while the host is live and `Unknown` once it is lost
///   or only awaiting. It says `Absent` only for a lease with no booking: a host that
///   goes silent keeps its booking until the lease is stopped, because silence is not
///   proof the work is gone.
/// - **Seat** puts back a booking the server lost with its process.
#[derive(Clone)]
pub struct RegisteredProvider {
    registry: Registry,
    classes: Vec<ResourceClass>,
}

impl RegisteredProvider {
    pub(crate) fn new(registry: Registry, class: ResourceClass) -> Self {
        Self {
            registry,
            classes: vec![class],
        }
    }

    /// The same provider serving `classes` instead of the pool's one: `start` refuses a
    /// class not among them with `UnsupportedClass`, and leases one that books part of a
    /// host beside the leases already there.
    pub fn with_classes(mut self, classes: Vec<ResourceClass>) -> Self {
        self.classes = classes;
        self
    }

    /// The hosts that are booked and the lease holding each, read from the registry's
    /// own table.
    pub fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        self.registry.booked()
    }

    /// Books `host` for `lease` without a start: the way to put back a booking the
    /// server lost with its process, when the lease file still records it. A table drops
    /// a lease its provider does not know, so without this a server restart would end
    /// every lease on a registered host before its agent could say hello again.
    ///
    /// A host the registry does not know becomes an *awaiting* host: the lease observes
    /// `Unknown` (not `Present`, as the host has not been heard, and not `Absent`), the
    /// host is not bookable, and it counts toward the pool's limit. When the agent says
    /// hello, the host is admitted as any other and the lease observes `Present` on it.
    /// The refusals are those of [`js_provider::HostBook::seat`] except `Unlisted`,
    /// which this never returns.
    ///
    /// For a whole-node pool the booking is put back `Committed`, unless the host lists
    /// held runs, when it is put back `Ready`. That assumes the held runs are the
    /// lease's own. Leftover runs of a lease that has ended, followed by a registry
    /// restart, would seat `Ready` on a host that is not quiet; this is rare, and the
    /// registry does not tell the two cases apart.
    ///
    /// `binding` is what the awaiting host's hello must match: the caller passes the
    /// binding it persisted when it admitted the host, and a hello from another
    /// credential or instance is refused as host-bound. With `None` the first hello
    /// binds the host and takes the restored lease, whoever sends it: that is the
    /// restart gap, closed by a caller that persists bindings. `binding` is ignored for
    /// a host the registry already knows.
    pub fn seat(
        &self,
        host: &HostId,
        lease: LeaseId,
        binding: Option<Binding>,
    ) -> Result<(), SeatError> {
        self.registry.seat(host, lease, binding, None, None)
    }

    /// As [`RegisteredProvider::seat`], for `booking` as the lease file records it: its
    /// host, its lease, what it booked and when it started. `shares` set puts back a lease
    /// that shares its host, which books those resources and needs a host with no
    /// whole-host lease and room for them beside the leases seated before it; `None` books
    /// the whole shape, as [`RegisteredProvider::seat`] does. With a source of lives set
    /// ([`crate::Registry::set_lives`]) the lease keeps its start and its end bound
    /// (`started_at`, `ends_at`), so its drain cost is counted from them; without one it
    /// has neither, as a lease put back always had. A lease started here counts from the
    /// start call, so a whole-node lease, whose recorded start is the moment it was ready,
    /// is costed after a restart from up to its preparation time later. The
    /// booking's `state` is not read. The refusals are those of
    /// [`js_provider::HostBook::seat_as`] except `Unlisted`.
    pub fn seat_booked(
        &self,
        booking: &Booking,
        binding: Option<Binding>,
    ) -> Result<(), SeatError> {
        let times = (booking.started_at, booking.ends_at);
        let shares = booking.shares.clone();
        let registry = &self.registry;
        registry.seat(&booking.host, booking.lease, binding, shares, Some(times))
    }
}

impl Pool for RegisteredProvider {
    fn id(&self) -> ProviderId {
        self.registry.pool_id()
    }

    fn classes(&self) -> Vec<ResourceClass> {
        self.classes.clone()
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        self.registry.start(&self.classes, &spec)
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        StopAck::Stopped {
            read_back: self.registry.stop(lease),
        }
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.registry.observe(lease)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        self.registry.capacity()
    }

    fn waits(&self, lease: &LeaseId) -> bool {
        self.registry.waits(lease)
    }
}
