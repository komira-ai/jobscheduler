//! Choosing the host a start books, when more than one could take it.

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::Phase;
use js_pool_static::Candidate;
use js_provider::{RefusalCode, StartSpec};
use js_types::{HostId, Shape};

/// What the registry knows of the host a probe ([`js_types::Request::probe_host`]) names,
/// for [`Placer::admit_probe`]. Facts only: the rule that judges them is the placer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeHost {
    /// The shape the host is booked as: its memory after the memory_schedulable_percent
    /// cut.
    pub shape: Shape,
    /// Whether the host is cordoned now: its own cordon has not lapsed.
    pub cordoned: bool,
    /// The host's phase.
    pub phase: Phase,
    /// The reason of the host's taint, if it is tainted.
    pub taint: Option<String>,
    /// Whether the host is being scaled away.
    pub leaving: bool,
}

/// The hosts a waiting whole-node request has set aside, each with how long until the end
/// of its tail as the mark took it: zero once that is past, `None` when it is not known.
pub type MarkedTails = BTreeMap<HostId, Option<Duration>>;

/// Picks the host a start books among the hosts that could take it.
///
/// The registry shows the placer every host that fits the request, has room for it beside
/// the leases already there, and is bookable and not set aside for another request (a
/// start that may backfill is shown the hosts set aside too, [`Placer::pick`]). The
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
    ///
    /// For a probe, `open` holds the probe's host alone, and the placer is asked only once
    /// [`Placer::admit_probe`] has admitted the probe there: the host is then cordoned.
    ///
    /// `marked` names the hosts a waiting whole-node request has set aside, with the time
    /// left until each one's tail ends. It is empty unless `spec` is of a class that shares
    /// a host and is not a probe: only such a start is shown a marked host, beside the
    /// others in `open`. The placer books a marked host only as backfill, for work that
    /// ends by the end of its tail; a placer that does not judge that books none of them.
    fn pick(&self, spec: &StartSpec, open: &[Candidate], marked: &MarkedTails) -> Option<usize>;

    /// Whether the probe `spec` may be booked on `pin`, the host it names, given what the
    /// registry knows of that host (`None`: the registry lists no live host of that name).
    /// Asked under the registry's lock before the book is, for a start not yet booked. On
    /// `Ok` the host is a candidate though it is cordoned, and is booked if it has room
    /// ([`Placer::pick`]); on `Err` it is not, and a start no host took answers that code.
    /// Room is not part of this answer.
    ///
    /// The default places no probe: [`RefusalCode::ProbeUnsupported`].
    ///
    /// # Errors
    /// The code of the refusal.
    fn admit_probe(
        &self,
        spec: &StartSpec,
        pin: &HostId,
        host: Option<&ProbeHost>,
    ) -> Result<(), RefusalCode> {
        let _ = (spec, pin, host);
        Err(RefusalCode::ProbeUnsupported)
    }
}
