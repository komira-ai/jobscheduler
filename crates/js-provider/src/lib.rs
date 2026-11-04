//! The scheduler's internal allocation port: book an allocation on a host, stop it,
//! observe it.
//!
//! [`Pool`] is the synchronous lease-level surface the lease table calls under its lock:
//! [`Pool::start`], [`Pool::stop`], [`Pool::observe`] and [`Pool::capacity`]. It is not a
//! provider of machines: what supplies machines, launched or registered, is
//! `js_elastic::CapacityProvider`. A pool answers every call. The calls are synchronous
//! because no pool yet leaves the process; they become async at the edge with the first
//! one that calls a network.
//!
//! The wall-time and reserved-label rules of the start contract of design 5.2, and the
//! class checks this crate adds to it, live in [`StartSpec::new`], the one constructor of
//! the only value [`Pool::start`] accepts. A request that breaks one of them cannot reach
//! a pool, and a pool cannot relax it. Two rules of 5.2 are not modelled yet: the fence
//! must be stated, and retries are never passed to a pool. `Request` has no fence or retry
//! field, so nothing checks either.
//!
//! Observed state follows 5.6: [`Observed::Present`] and [`Observed::Absent`] are
//! answers, [`Observed::Unknown`] is the lack of one. Only `Absent` frees a booking
//! ([`Observed::frees_booking`]).
//!
//! [`HostBook`] is the booking table a pool of hosts keeps: the leases each host holds
//! and what each books there, and every lease it has stopped. The static pool (`js-pool-static`) and the
//! registry of self-registered hosts (`js-registry`) both book through it, so neither
//! copies its rules.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod ack;
mod book;
mod spec;

pub use ack::{Call, HostCapacity, Observed, StartAck, StopAck};
pub use book::{Candidate, Held, HostBook, SeatError};
pub use spec::{RefusalCode, StartSpec};

use js_types::{LeaseId, ProviderId, ResourceClass};

/// Hosts an allocation can be booked on: the allocation port. See the crate docs for the
/// contract.
pub trait Pool {
    /// A stable id. The scheduler never branches on its text.
    fn id(&self) -> ProviderId;

    /// The resource classes this pool can serve.
    fn classes(&self) -> Vec<ResourceClass>;

    /// Records the intent to start one attempt. Idempotent on the lease id: a second
    /// start of the same lease answers [`StartAck::Started`] again and books nothing
    /// more.
    ///
    /// A stopped lease id never starts again. A start that arrives after the stop of
    /// its lease (late, duplicated or reordered) is refused with
    /// [`RefusalCode::LeaseEnded`] and books nothing, and the lease then observes
    /// `Absent`. A retry that needs a new attempt takes a new lease id.
    fn start(&mut self, spec: StartSpec) -> StartAck;

    /// Stops one lease and reads its state back. The answer never claims more than
    /// the read-back showed. Stopping a lease that is already gone is a stop.
    fn stop(&mut self, lease: &LeaseId) -> StopAck;

    /// Whether the lease exists: [`Observed::Present`], [`Observed::Absent`] after an
    /// explicit not-found, or [`Observed::Unknown`] for no answer.
    fn observe(&mut self, lease: &LeaseId) -> Observed;

    /// Every host the pool lists, with its shape and the lease holding it.
    fn capacity(&self) -> Vec<HostCapacity>;

    /// Whether the pool holds the lease's request on a wait list: the request carried a
    /// request id ([`StartSpec::with_request_id`]), no host is booked for it yet, and the
    /// pool has neither placed nor refused it. A start of the same spec again is a poll
    /// that keeps its place, and the start that finds it a host books the lease. A stop
    /// takes it off the list. A pool that queues nothing answers `false`, the default.
    fn waits(&self, lease: &LeaseId) -> bool {
        let _ = lease;
        false
    }
}
