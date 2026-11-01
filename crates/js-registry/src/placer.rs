//! Choosing the host a start books, when more than one could take it.

use js_pool_static::Candidate;
use js_provider::StartSpec;

/// Picks the host a start books among the hosts that could take it.
///
/// The registry shows the placer every host that fits the request, has room for it beside
/// the leases already there, and is bookable and not set aside for another request. The
/// placer only chooses: the booking, the room and every refusal stay the registry's. A
/// registry with no placer ([`crate::Registry::set_placer`]) books the first candidate,
/// and refuses a class that books part of a host, since no host could share.
pub trait Placer: Send + Sync {
    /// The index in `open` of the candidate to book for `spec`, or `None` to book none (the
    /// start then answers `NoCapacity`). `open` is in host id order and is never empty: with
    /// no candidate the placer is not asked. An index past `open` books none either.
    fn pick(&self, spec: &StartSpec, open: &[Candidate]) -> Option<usize>;
}
