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
///
/// Why a trait here: the placer that ships, `SchedPlacer` of `js-lease-server`, builds a
/// `js-sched` scheduler and maps hosts and requests into it with the server's own
/// conversions. This crate is in the runtime layer (`layers.toml`) and the server is above
/// it, so the registry cannot name that placer; it can only hold something the server
/// hands it, which is this seam. A boxed `Fn` would be the same seam with the same one
/// call; the trait is used instead so that the contract of the call (the index, the order
/// of `open`, never asked with no candidate) is written once on [`Placer::pick`], and so
/// that an implementation can carry state of its own, as `SchedPlacer` keeps its fallback
/// counters. The registry's own tests place with small implementations of it.
pub trait Placer: Send + Sync {
    /// The index in `open` of the candidate to book for `spec`, or `None` to book none (the
    /// start then answers `NoCapacity`). `open` is in host id order and is never empty: with
    /// no candidate the placer is not asked. An index past `open` books none either.
    fn pick(&self, spec: &StartSpec, open: &[Candidate]) -> Option<usize>;
}
