//! The provider interface of the job scheduler, delegated flavour only.
//!
//! This crate is design section 5.2 for a provider the scheduler calls and that
//! answers each call: [`Provider::start`], [`Provider::stop`], [`Provider::observe`]
//! and [`Provider::capacity`]. The agent flavour (a daemon holding one stream to the
//! scheduler, answering through events) is a future [`Flavour`] variant and is not
//! implemented here. The calls are synchronous in this slice because no provider yet
//! leaves the process; they become async at the edge with the first provider that
//! calls a network.
//!
//! The start contract of 5.2 lives in [`StartSpec::new`], the one constructor of the
//! only value [`Provider::start`] accepts. A request that breaks a rule therefore
//! cannot reach a provider, and a provider cannot relax a rule.
//!
//! Observed state follows 5.6: [`Observed::Present`] and [`Observed::Absent`] are
//! answers, [`Observed::Unknown`] is the lack of one. Only `Absent` frees a booking
//! ([`Observed::frees_booking`]).

mod ack;
mod spec;

pub use ack::{Call, HostCapacity, Observed, StartAck, StopAck};
pub use spec::{RefusalCode, StartSpec};

use js_types::{LeaseId, ProviderId, ResourceClass};

/// Which side initiates contact (design 5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Flavour {
    /// A cloud service the scheduler calls. Every call is answered.
    Delegated,
}

/// A place work can be leased. See the crate docs for the contract.
pub trait Provider {
    /// A stable id. The scheduler never branches on its text.
    fn id(&self) -> ProviderId;

    /// Which flavour of provider this is.
    fn flavour(&self) -> Flavour;

    /// The resource classes this provider can serve.
    fn classes(&self) -> Vec<ResourceClass>;

    /// Records the intent to start one attempt. Idempotent on the lease id: a second
    /// start of the same lease answers [`StartAck::Started`] again and books nothing
    /// more.
    fn start(&mut self, spec: StartSpec) -> StartAck;

    /// Stops one lease and reads its state back. The answer never claims more than
    /// the read-back showed. Stopping a lease that is already gone is a stop.
    fn stop(&mut self, lease: &LeaseId) -> StopAck;

    /// Whether the lease exists: [`Observed::Present`], [`Observed::Absent`] after an
    /// explicit not-found, or [`Observed::Unknown`] for no answer.
    fn observe(&mut self, lease: &LeaseId) -> Observed;

    /// Every host the provider lists, with its shape and the lease holding it.
    fn capacity(&self) -> Vec<HostCapacity>;
}
