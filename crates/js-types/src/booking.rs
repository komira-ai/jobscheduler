//! What a host holds, as the booking table, the placement engine and the host listing read
//! it: [`Held`], one lease on a host; [`Candidate`], a host a start could be booked on; and
//! [`HostCapacity`], one listed host and the leases it holds.

use crate::{HostId, LeaseId, MemoryHeadroom, Resources, Shape};

/// One lease on a host and what it books there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The lease.
    pub lease: LeaseId,
    /// What it books on the host: its own resources, or the whole shape for a lease that
    /// books the whole host.
    pub resources: Resources,
    /// Whether the lease books the whole host, whatever its resources add up to.
    pub whole: bool,
}

/// A host a start could be booked on: it fits the request, has room for it and is
/// eligible. The booking table (`js_provider::HostBook`) shows the candidates to the
/// caller's choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The host.
    pub host: HostId,
    /// What the host offers to be booked.
    pub shape: Shape,
    /// The leases booked on it, in the order they were booked.
    pub leases: Vec<Held>,
}

/// One host of a pool's capacity and who holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCapacity {
    /// The host.
    pub host: HostId,
    /// What the host offers to be booked: its memory is already cut to the pool's
    /// `memory_schedulable_percent`.
    pub shape: Shape,
    /// The memory the operator configured for the host, the percent of it that is booked
    /// and what that comes to (the memory of `shape`).
    pub memory: MemoryHeadroom,
    /// The lease booked first on the host, or `None` when it is free. A host that books
    /// many leases has more: read [`HostCapacity::leases`] to see them all.
    pub leased_to: Option<LeaseId>,
    /// Every lease booked on the host, in the order they were booked. Empty when the host
    /// is free; `leased_to` is the first of them.
    pub leases: Vec<LeaseId>,
}
