//! Choosing the host a start books, when more than one could take it.

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::Phase;
use js_lease::Timestamp;
use js_provider::Candidate;
use js_provider::{RefusalCode, StartSpec};
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, LeaseId, Request, RequestId, Shape};

use crate::WholeNodeConfig;
use crate::whole::requests::{Exclusion, NotReady, RequestKey, RequestRecord};
use crate::whole::reserve::Reservation;

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

/// How the registry stands a host for the whole-node queue ([`WholeHost`]), the first that
/// applies in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// It reported a taint: it holds no request until an operator deals with it.
    Tainted,
    /// It is being scaled away: it holds no request.
    Leaving,
    /// It has not been heard within the pool's grace (or never, since it was seated): it
    /// may come back, and holds no request until it does.
    Lost,
    /// It is live and an operator's cordon has not lapsed: it takes no start until then,
    /// and may still be set aside for a request, as a busy host may.
    Cordoned,
    /// It is live: it may hold a request.
    Live,
}

/// A host the registry lists, as the whole-node queue reads it ([`WholeRound`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WholeHost {
    /// The host.
    pub id: HostId,
    /// How the registry stands it.
    pub standing: Standing,
    /// Whether the registry would book a whole-node lease on it now: live, serving, clean,
    /// not cordoned, staying, clear of any whole-node lease, and with no lease or run on it.
    pub open: bool,
}

/// A whole-node request on the registry's wait list that has been presented since the
/// registry started, as the whole-node queue reads it ([`WholeRound`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingWhole {
    /// The request.
    pub id: RequestId,
    /// The longest its lease may run.
    pub max_wall: Duration,
    /// When it was first presented: its place on the wait list and the start of its
    /// `drain_after`.
    pub since: Timestamp,
    /// Deprecated, always zero: no request gives a host up.
    pub give_ups: u32,
    /// The hosts left out of its placements after failed prepares, and until when.
    pub exclusions: BTreeMap<HostId, Exclusion>,
    /// The host the registry holds set aside for it now: the mark the last round left, or
    /// one put back from the ledger.
    pub mark: Option<Reservation>,
}

/// What the registry hands a placer that runs the whole-node queue, once per pass
/// ([`Placer::whole_round`]). Built under the registry's lock from its own books.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WholeRound {
    /// The pool's whole-node settings, with the caps lowered by the hosts of
    /// [`WholeRound::outside`]: each is one host whole less under `max_whole_node` and one
    /// more under `keep_for_ordinary`, and each that drains a busy host one less under
    /// `max_draining`. A placer that keeps the wait list ([`Placer::keeps_wait_list`]) is
    /// handed them as the pool has them, and holds nothing outside it.
    pub settings: WholeNodeConfig,
    /// The presented requests of the wait list, in its order: first presentation first.
    /// Empty for a placer that keeps the wait list: the requests are its own.
    pub waiting: Vec<WaitingWhole>,
    /// Every host the registry lists, in host id order.
    pub hosts: Vec<WholeHost>,
    /// The hosts set aside for requests on the wait list that have not been presented since
    /// a restart (marks put back from the ledger), with the time left until each one's
    /// tail ends: no other request may take them. Empty for a placer that keeps the wait
    /// list, which holds the marks put back itself.
    pub outside: MarkedTails,
    /// For each lease booked now that has one, the P90 of the wall minutes of the profile
    /// of its keyed runs that lasts longest (the registry's source of lives,
    /// [`crate::Registry::set_lives`]): what the lease is expected to last from its start.
    /// Empty when no source is set.
    pub lives: BTreeMap<LeaseId, u32>,
}

/// What a placer that keeps the whole-node queue keeps of one request between its
/// placements, as the reservation ledger writes it ([`Placer::whole_ledger`]) and puts it
/// back ([`Placer::whole_restore`]): the failed prepares and the hosts left out of its
/// placements, with a give-up count that is always zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WholeKept {
    /// Deprecated, always zero when written: no hold is given up. A ledger written before
    /// may hold a count, which is read and ignored.
    pub give_ups: u32,
    /// The prepares of its leases that failed.
    pub prepare_failures: u32,
    /// The hosts it is kept off, each until when: the bars of failed prepares (and a
    /// back-off an older ledger kept), those that have not ended.
    pub exclusions: BTreeMap<HostId, Exclusion>,
}

impl WholeKept {
    /// Whether nothing is kept: no give-up counted, no failed prepare, no host left out.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        [
            self.give_ups == 0,
            self.prepare_failures == 0,
            self.exclusions.is_empty(),
        ]
        .into_iter()
        .all(|none| none)
    }
}

/// A committed whole-node lease whose prepare failed, as the registry hands it to a placer
/// that keeps the whole-node queue ([`Placer::prepare_failed`]): what the row of the cause
/// (`whole/reasons.rs`) does to the host for the request, and what its terminal refusal
/// would name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareFailure {
    /// How long the host is left out of the request's placements: until `reserve_backoff`
    /// after the failure, or for the request's life. `None` when the row taints the host
    /// instead, which keeps it from every request.
    pub exclusion: Option<Exclusion>,
    /// The code (`prepare-timeout` for a deadline that passed) and the agent's detail, as
    /// received.
    pub not_ready: NotReady,
}

/// How a wait list answers a presentation of a whole-node request
/// ([`Placer::whole_present`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presented {
    /// The request waits, at this place from the front (zero is next).
    Waiting(usize),
    /// The list holds [`crate::MAX_WAITING_REQUESTS`] requests and this one is not among
    /// them: it is not entered.
    Full,
    /// No host could ever serve the request and it has waited out `regain_grace` since it
    /// was last seen to have one, or it never had a place: it is refused and not kept.
    Unservable,
}

/// A whole-node request on a placer's wait list that the consumer has presented, as the
/// scaler reads it ([`Placer::whole_waiting`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingRequest {
    /// The request.
    pub id: RequestId,
    /// What it asks for, as first presented.
    pub request: Request,
    /// When it was first presented.
    pub since: Timestamp,
}

/// What a placer's wait list says of one waiting whole-node request for the record of how
/// it waited ([`Placer::whole_waiter`], [`Placer::whole_waiter_of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiter {
    /// The request.
    pub id: RequestId,
    /// When it was first presented.
    pub since: Timestamp,
    /// Deprecated, always zero: no hold is given up.
    pub give_ups: u32,
    /// Whether a host was set aside for it at any time while it waited.
    pub marked: bool,
}

/// Picks the host a start books among the hosts that could take it.
///
/// The registry shows the placer every host that fits the request, has room for it beside
/// the leases already there, and is bookable and not set aside for another request (a
/// start that may backfill is shown the hosts set aside too, [`Placer::pick`]). The
/// placer only chooses: the booking, the room and every refusal stay the registry's. A
/// registry with no placer ([`crate::Registry::set_placer`]) books the first candidate,
/// and refuses a class that books part of a host, since no host could share.
///
/// Why a trait here: the placer that ships, `SchedPlacer` of `js-sched-placer`, keeps a
/// `js-sched` scheduler for the life of the server and maps hosts and requests into it with
/// that crate's own conversions; it learns the registry's books from the three calls that
/// do not place ([`Placer::host_up`], [`Placer::heard`], [`Placer::ended`]). That crate
/// depends on this one, so the registry cannot name that placer; it can only hold
/// something the server hands it, which is this seam. A boxed `Fn` would be the same seam with the same one
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

    /// Runs one pass of the whole-node queue in place of the registry's own, for a placer
    /// that keeps the queue itself: the hosts set aside for the presented requests of
    /// `round` after the pass, each with when it was set aside, its deadline, whether it
    /// was quiet (`assigned`) and the end of its tail. The registry shows them as its
    /// reservations and holds each host for its request alone (a start of a class that
    /// shares a host is still shown it, as backfill), keeping the marks of requests not yet
    /// presented as they are. A request's next start is then shown the host set aside for
    /// it, and the placer books it there ([`Placer::pick`]) once the host is quiet. A
    /// request that has left the wait list is not in `round`: the placer lets it go.
    /// Asked under the registry's lock at every pass: each tick and each start. A placer
    /// that keeps the wait list ([`Placer::keeps_wait_list`]) is handed the round with no
    /// request and holds the marks it makes: the registry reads them from
    /// [`Placer::whole_reservations`] and does not take the answer.
    ///
    /// The default, `None`, keeps no queue: the registry runs its own pass.
    fn whole_round(&self, round: &WholeRound) -> Option<Vec<Reservation>> {
        let _ = round;
        None
    }

    /// Whether the prepare of `lease`, a whole-node lease committed on its host and not
    /// reported ready, has passed its deadline: `Some` for a lease the placer committed
    /// from its whole-node queue (it keeps the commit time and `prepare_timeout`), `None`
    /// for any other, whose deadline the registry keeps. Asked under the registry's lock
    /// as the lease table's tick reads the pending leases, never at a heartbeat, so a ready
    /// report that arrives before that tick wins. The default, `None`, keeps no deadline.
    fn prepare_overdue(&self, lease: &LeaseId) -> Option<bool> {
        let _ = lease;
        None
    }

    /// The agent reported `lease` ready on a clean host committed to it: the lease's
    /// request is placed, so its failed prepares and bars are let go. Told under
    /// the registry's lock, at every heartbeat that carries the report. The default does
    /// nothing.
    fn prepare_ready(&self, lease: &LeaseId) {
        let _ = lease;
    }

    /// The host of `lease`, ready, said hello again: its preparation is void, and the agent
    /// has a whole `prepare_timeout` from now to report it ready again (the registry's
    /// `Ready` turned `Committed`). Told under the registry's lock. The default does
    /// nothing.
    fn prepare_again(&self, lease: &LeaseId) {
        let _ = lease;
    }

    /// The prepare of `lease`, committed and not ready, failed as `failure` says (a typed
    /// refusal of the agent, or the deadline passed). `true` when the placer keeps the
    /// lease's request: it counts the failure toward `max_replace`, keeps the host from the
    /// request as `failure` says, and lets the lease go; the registry then counts nothing
    /// itself. `false` for a lease it does not keep, which the registry counts (a placer that
    /// keeps the wait list keeps every committed lease it was told of,
    /// [`Placer::whole_commit`], and the registry counts none). Either way
    /// the registry ends the lease (its runs, the host's restore and taint, the book).
    /// Told under the registry's lock. The default, `false`, keeps no request.
    fn prepare_failed(&self, lease: &LeaseId, failure: &PrepareFailure) -> bool {
        let _ = (lease, failure);
        false
    }

    /// A whole-node start of the request under `key`: `true` once for a request that used
    /// up `max_replace` (its start is then refused `ExclusiveNotReady`, and its record and
    /// bars are let go, so the start after it is a fresh request); `false` for any other,
    /// whose record, if it has one, is marked as in use now. Asked under the registry's
    /// lock before the request is presented: for a request with an id, and for one told
    /// apart by its content only of a placer that keeps the wait list
    /// ([`Placer::keeps_wait_list`]). The default, `false`, keeps no record.
    fn whole_start(&self, key: &RequestKey) -> bool {
        let _ = key;
        false
    }

    /// What the placer keeps of the request under `key` between its placements: its failed
    /// prepares, the hosts left out of its placements, the last failure and
    /// whether it is terminal. `None` for a request it keeps no record of. A request told
    /// apart by its content is asked of a placer that keeps the wait list only. The default
    /// keeps none.
    fn whole_record(&self, key: &RequestKey) -> Option<RequestRecord> {
        let _ = key;
        None
    }

    /// Request `id` was cancelled while it waited (the lease that presented it was stopped):
    /// it leaves no trace, so the id names a fresh request if it is used again; its record
    /// and bars go. Told under the registry's lock. The default does nothing.
    fn whole_forget(&self, id: &RequestId) {
        let _ = id;
    }

    /// Drops the records of the requests that have no committed lease and have not
    /// started or failed for `horizon` (the registry's idle horizon), with their bars,
    /// those for life included. Told under the registry's lock whenever it locks. The
    /// default does nothing.
    fn whole_sweep(&self, horizon: Duration) {
        let _ = horizon;
    }

    /// The hosts the requests this placer queues ([`Placer::whole_round`]) hold now, as the
    /// registry shows them: a quiet host granted, as an assigned reservation, and a mark
    /// with its deadline and the end of its tail. The registry reads them for its
    /// reservations, the hosts listing and the ledger, for each host it still holds for the
    /// same request (a retire or a stop lets a host go at once); of a placer that keeps the
    /// wait list ([`Placer::keeps_wait_list`]) they are every host held, marks put back from
    /// the ledger included, and the only ones. Asked under the registry's lock.
    ///
    /// The default, `None`, is a placer that keeps no queue: the registry's own marks are
    /// read.
    fn whole_reservations(&self) -> Option<Vec<Reservation>> {
        None
    }

    /// What this placer keeps of each whole-node request for the reservation ledger
    /// ([`crate::Registry::reservation_ledger`]), those it keeps anything of: the requests
    /// it queues, those whose prepare failed, and those put back by
    /// [`Placer::whole_restore`] and not queued since. The registry writes each beside its
    /// own counts, the larger of each count and every host either keeps the request off.
    /// Asked under the registry's lock. The default keeps none.
    fn whole_ledger(&self) -> BTreeMap<RequestId, WholeKept> {
        BTreeMap::new()
    }

    /// Puts back what a restart's ledger kept of request `id`
    /// ([`crate::Registry::restore_reservations`]): `kept`, the request first presented at
    /// `since`, and `terminal` when its failed prepares reached `max_replace`. `true` when
    /// the placer keeps it: it queues the request with its bars when it is
    /// presented again, and answers its failed prepares and terminal start at once
    /// ([`Placer::whole_record`], [`Placer::whole_start`]); the registry then keeps
    /// nothing of it. Told under the
    /// registry's lock, before any host says hello.
    ///
    /// The default, `false`, keeps nothing: the registry puts the record back itself.
    fn whole_restore(
        &self,
        id: &RequestId,
        kept: &WholeKept,
        terminal: bool,
        since: Timestamp,
    ) -> bool {
        let _ = (id, kept, terminal, since);
        false
    }

    /// Whether the placer keeps the whole-node wait list and the request records itself
    /// (the engine of `--placement js-sched`): the registry then keeps neither, runs none of
    /// its own reservation machine (the wait list, the request records, the marks and the
    /// pass), and hands the placer every call of it through the methods below, which it asks
    /// of no other placer. The placer then runs the pass ([`Placer::whole_round`], handed
    /// the round with no request and nothing held outside, which are its own), holds the
    /// marks ([`Placer::whole_reservations`]) and counts every failed prepare
    /// ([`Placer::prepare_failed`]). Asked under the registry's lock. The default, `false`,
    /// leaves them to the registry.
    fn keeps_wait_list(&self) -> bool {
        false
    }

    /// The consumer presents whole-node request `id`, which asks for `request`, under
    /// `lease`: the wait list's answer. `capable` says whether some host could ever be given
    /// whole to it. A request not on the list enters at the back if `capable` and the list
    /// holds fewer than [`crate::MAX_WAITING_REQUESTS`] (else it is [`Presented::Full`]),
    /// and is [`Presented::Unservable`] at once if not `capable`; one on the list keeps its
    /// place, is seen again now and belongs to `lease` from now on, and once no host has
    /// been capable for `regain_grace` it is `Unservable` and leaves the list. Asked under
    /// the registry's lock of a placer that keeps the wait list. The default enters
    /// nothing: `Full`.
    fn whole_present(
        &self,
        id: &RequestId,
        lease: LeaseId,
        request: &Request,
        capable: bool,
        regain_grace: Duration,
    ) -> Presented {
        let _ = (id, lease, request, capable, regain_grace);
        Presented::Full
    }

    /// Drops every waiting request not presented for `regain_grace`, and how many
    /// reservations put back by a restart were dropped since this was last asked: requests
    /// put back that were never presented again, and marks whose host never said hello
    /// within `regain_grace` of the restart ([`crate::Registry::dropped_reservations`]).
    /// Told under the registry's lock, whenever it locks, of a placer that keeps the wait
    /// list. The default drops nothing.
    fn whole_expire(&self, regain_grace: Duration) -> u64 {
        let _ = regain_grace;
        0
    }

    /// Takes request `id` off the wait list at once, and the host set aside for it with it;
    /// whether it was waiting. Its record stays. Told under the registry's lock of a placer
    /// that keeps the wait list. The default takes nothing.
    fn whole_cancel(&self, id: &RequestId) -> bool {
        let _ = id;
        false
    }

    /// Takes off the wait list, at once, the request `lease` presented last; its id, if one
    /// waited. Told under the registry's lock at a stop of `lease`, before
    /// [`Placer::whole_forget`] of that id, of a placer that keeps the wait list. The default
    /// takes nothing.
    fn whole_cancel_lease(&self, lease: &LeaseId) -> Option<RequestId> {
        let _ = lease;
        None
    }

    /// Binds the waiting request `id` to `lease`, as if `lease` had presented it: a stop of
    /// `lease` then takes it off the list. Whether `id` waits. Told under the registry's
    /// lock of a placer that keeps the wait list. The default binds nothing.
    fn whole_bind(&self, id: &RequestId, lease: LeaseId) -> bool {
        let _ = (id, lease);
        false
    }

    /// How waiting request `id` waits, if it does. Asked under the registry's lock of a
    /// placer that keeps the wait list. The default knows of none.
    fn whole_waiter(&self, id: &RequestId) -> Option<Waiter> {
        let _ = id;
        None
    }

    /// How the request `lease` presented last waits, if one does. Asked under the
    /// registry's lock of a placer that keeps the wait list. The default knows of none.
    fn whole_waiter_of(&self, lease: &LeaseId) -> Option<Waiter> {
        let _ = lease;
        None
    }

    /// Every request on the wait list, front first, presented or put back by a restart and
    /// not presented since: its id and when it was first presented. A request's place is
    /// its index here. Asked under the registry's lock of a placer that keeps the wait
    /// list. The default lists none.
    fn whole_places(&self) -> Vec<(RequestId, Timestamp)> {
        Vec::new()
    }

    /// The requests on the wait list the consumer has presented, front first. Asked under
    /// the registry's lock of a placer that keeps the wait list. The default lists none.
    fn whole_waiting(&self) -> Vec<WaitingRequest> {
        Vec::new()
    }

    /// Puts request `id`, first presented at `since`, back at the back of the wait list
    /// from a restart's ledger, as if seen now; what it asks for is filled in by its next
    /// presentation. `false` when the list is full. Told under the registry's lock, in
    /// queue order and before any host says hello, of a placer that keeps the wait list.
    /// The default puts nothing back.
    fn whole_restore_place(&self, id: &RequestId, since: Timestamp) -> bool {
        let _ = (id, since);
        false
    }

    /// Puts back from a restart's ledger `mark`, a host set aside for a request put back on
    /// the wait list: it stands while its host is unknown until `claim_by`, after which it
    /// is dropped and counted ([`Placer::whole_expire`]). `false` when the host is set
    /// aside already. Told under the registry's lock, after the request's place, of a
    /// placer that keeps the wait list. The default puts nothing back.
    fn whole_revive(&self, mark: &Reservation, claim_by: Timestamp) -> bool {
        let _ = (mark, claim_by);
        false
    }

    /// `host` is retired: a request it is set aside for loses it at once, and is placed
    /// again at the next pass. Told under the registry's lock of a placer that keeps the
    /// wait list. The default does nothing.
    fn whole_drop_host(&self, host: &HostId) {
        let _ = host;
    }

    /// The registry committed `lease`, a whole-node lease, for the request under `key`:
    /// it stands for the request until it ends ([`Placer::ended`]) or its prepare fails.
    /// Told under the registry's lock of a placer that keeps the wait list. The default
    /// does nothing.
    fn whole_commit(&self, lease: LeaseId, key: &RequestKey) {
        let _ = (lease, key);
    }

    /// Whether a lease other than `lease` is committed for request `id`: a start of it
    /// then waits. Asked under the registry's lock of a placer that keeps the wait list.
    /// The default holds none.
    fn whole_held_by_other(&self, id: &RequestId, lease: &LeaseId) -> bool {
        let _ = (id, lease);
        false
    }
}

#[cfg(test)]
mod tests {
    use js_types::Resources;

    use super::*;

    /// A placer that picks nothing and takes every default.
    struct Plain;

    impl Placer for Plain {
        fn pick(&self, _: &StartSpec, _: &[Candidate], _: &MarkedTails) -> Option<usize> {
            None
        }
    }

    /// Catches a default that keeps a wait list or a record (the registry would then hand
    /// a placer that keeps neither the calls of its own machine and lose them): a placer
    /// that takes the defaults keeps no wait list, enters, binds, lists, puts back and holds
    /// nothing, and answers no request as held or waiting.
    #[test]
    fn a_placer_that_takes_the_defaults_keeps_no_wait_list() {
        let id = RequestId::new("r").unwrap();
        let lease = LeaseId::new(1, 1);
        let host = HostId::new("a").unwrap();
        let resources = Resources::new(1000, 1 << 30, 0, None).unwrap();
        let class = js_types::ResourceClass::whole_node();
        let request = Request {
            class: class.id().clone(),
            resources,
            capabilities: BTreeMap::new(),
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
            probe_host: None,
        };
        let key = RequestKey::Id(id.clone());
        let at = Timestamp::from_millis(5);
        let mark = Reservation {
            request: id.clone(),
            host: host.clone(),
            at,
            deadline: at,
            assigned: false,
            tail_end: None,
        };
        let grace = Duration::from_secs(1);
        let placer = Plain;
        let spec = StartSpec::new(lease, request.clone(), &[class]).unwrap();
        assert_eq!(placer.pick(&spec, &[], &MarkedTails::new()), None);
        assert!(!placer.keeps_wait_list());
        let presented = placer.whole_present(&id, lease, &request, true, grace);
        assert_eq!(presented, Presented::Full);
        assert_eq!(placer.whole_expire(grace), 0);
        assert!(!placer.whole_cancel(&id));
        assert_eq!(placer.whole_cancel_lease(&lease), None);
        assert!(!placer.whole_bind(&id, lease));
        assert_eq!(placer.whole_waiter(&id), None);
        assert_eq!(placer.whole_waiter_of(&lease), None);
        assert!(placer.whole_places().is_empty());
        assert!(placer.whole_waiting().is_empty());
        assert!(!placer.whole_restore_place(&id, at));
        assert!(!placer.whole_revive(&mark, at));
        placer.whole_drop_host(&host);
        placer.whole_commit(lease, &key);
        assert!(!placer.whole_held_by_other(&id, &lease));
        assert!(!placer.whole_start(&key));
        assert_eq!(placer.whole_record(&key), None);
    }
}
