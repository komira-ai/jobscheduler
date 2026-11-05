//! Choosing the host a start books, when more than one could take it.

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::Phase;
use js_provider::Candidate;
use js_provider::{RefusalCode, StartSpec};
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, LeaseId, Shape};

/// What the registry knows of the host a probe ([`js_types::Request::probe_host`]) names,
/// for [`Placer::admit_probe`]. Facts only: the rule that judges them is the placer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeHost {
    /// The shape the host is booked as: the pool's booked shape (its memory after the
    /// pool's memory_schedulable_percent cut, all of it when the pool cuts none).
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
/// Why a trait here: the placer that ships, `SchedPlacer` of `js-lease-server`, keeps a
/// `js-sched` scheduler for the life of the server and maps hosts and requests into it with
/// the server's own conversions; it learns the registry's books from the three calls that
/// do not place ([`Placer::host_up`], [`Placer::heard`], [`Placer::ended`]). This crate is
/// in the runtime layer (`layers.toml`) and the server is above it, so the registry cannot
/// name that placer; it can only hold something the server
/// hands it, which is this seam. A boxed `Fn` would be the same seam with the same one
/// call; the trait is used instead so that the contract of the call (the index, the order
/// of `open`, never asked with no candidate) is written once on [`Placer::pick`], and so
/// that an implementation can carry state of its own, as `SchedPlacer` keeps its scheduler
/// and its counters. The registry's own tests place with small implementations of it.
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

    /// [`Placer::pick`], for a placer that keeps a picture of the registry's books between
    /// starts: `every` reads every host the registry lists, eligible or not, with its
    /// shape and the leases booked on it, in host id order. It costs time in hosts plus
    /// leases, so a placer reads it only when its picture and `open` disagree. The
    /// registry asks this; the default ignores `every` and asks [`Placer::pick`].
    fn pick_over(
        &self,
        spec: &StartSpec,
        open: &[Candidate],
        marked: &MarkedTails,
        every: &dyn Fn() -> Vec<Candidate>,
    ) -> Option<usize> {
        let _ = every;
        self.pick(spec, open, marked)
    }

    /// The thousandths above a host's cores that the leases sharing it may book together:
    /// the registry's book lets them, and books no more of any other dimension. Read once,
    /// when the placer is set ([`crate::Registry::set_placer`]). The default, 0, books no
    /// more CPU than the shape has.
    fn cpu_slack_permille(&self) -> u32 {
        0
    }

    /// `host` said hello and is listed, booked as `shape`, the pool's shape: a new host,
    /// or one that comes back. Told under the registry's lock. The default does nothing.
    fn host_up(&self, host: &HostId, shape: &Shape) {
        let _ = (host, shape);
    }

    /// The registry accepted a heartbeat of `host`; `load` is the reading it brought, as
    /// the registry vetted and converted it, when it brought a new one that passed (a beat
    /// with no reading, a repeated one, or one the vet drops gives `None`). Told under the
    /// registry's lock. The default does nothing.
    fn heard(&self, host: &HostId, load: Option<&NodeLoad>) {
        let _ = (host, load);
    }

    /// `host` is no longer listed: it was lost past the pool's retention holding no booking,
    /// or given back ([`crate::Registry::remove`]). Told under the registry's lock. The
    /// default does nothing.
    fn host_gone(&self, host: &HostId) {
        let _ = host;
    }

    /// `lease` was stopped: its booking is gone. Told under the registry's lock. The
    /// default does nothing.
    fn ended(&self, lease: LeaseId) {
        let _ = lease;
    }

    /// Whether the placer books work on `host` that the registry's book does not hold (a
    /// job's allocation): such a host is not forgotten however long it is silent, since only
    /// its own confirmation or the provider's absence frees that work. Asked under the
    /// registry's lock. The default, `false`, holds nothing there.
    fn holds_work_on(&self, host: &HostId) -> bool {
        let _ = host;
        false
    }
}
