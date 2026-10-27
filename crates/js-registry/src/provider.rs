use std::collections::BTreeMap;

use js_pool_static::SeatError;
use js_provider::{Flavour, HostCapacity, Observed, Provider, StartAck, StartSpec, StopAck};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass};

use crate::credential::Binding;
use crate::registry::Registry;

/// The [`Provider`] the lease table runs on for a pool of self-registered hosts.
///
/// It answers from memory and never blocks, as the table calls it under its lock. Each
/// host is leased whole, as the static pool does, and the rules a lease follows are the
/// ones [`js_pool_static::HostBook`] holds for both. What differs is which host may be
/// booked and what a read can say:
///
/// - **Start** books the first host by id that is unbooked, live (heard from within the
///   grace period) and serving, and whose booked shape fits the request. A host that
///   is lost, awaiting, draining, quarantined, tainted or in any other phase than
///   serving takes no new lease. With no host to book it answers `NoCapacity` for a
///   request the pool's shape fits, `NoFit` for one it cannot.
/// - **Stop** frees the booking at once and reads back `Absent`, as it does for a lost
///   host: the table asked for the end of the lease.
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

    /// The same provider serving `classes` instead of the pool's one. As for the static
    /// pool, `start` refuses, with `UnsupportedClass`, a class whose admission does
    /// not book a whole host (`BookAll` or `Whole`), because every lease holds one.
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
    /// The refusals are those of [`js_pool_static::StaticPool::seat`] except `Unlisted`,
    /// which this never returns.
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
        self.registry.seat(host, lease, binding)
    }
}

impl Provider for RegisteredProvider {
    fn id(&self) -> ProviderId {
        self.registry.pool_id()
    }

    fn flavour(&self) -> Flavour {
        Flavour::Delegated
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
}
