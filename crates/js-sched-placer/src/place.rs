//! The registry's placer, with `js-sched` choosing the host.
//!
//! [`SchedPlacer`] keeps one scheduler for the life of the server ([`Engine`]), built once
//! from the server's configuration: the backfill ceiling (`server.max_lease_wall`), the
//! CPU slack (`--cpu-slack`, js-sched's default 1.1x unless given) and the share of each
//! host's memory it may book (`memory_schedulable_percent` of the pool). The registry
//! tells it of every hello, accepted heartbeat, lease put back at a start, stop and host it
//! forgets as they happen ([`Placer::host_up`], [`Placer::heard`], [`Placer::seated`],
//! [`Placer::ended`], [`Placer::host_gone`]), so
//! the engine's picture of the hosts follows the registry's books without being rebuilt;
//! a heartbeat's load reading reaches the engine there, as the registry vetted and
//! converted it. When the engine is built again it is given the last three readings kept
//! for each host (its two windows), so a rebuild loses no measured load.
//!
//! At a start the registry shows the placer the hosts that could take it (`open`), each
//! with the leases already on it. The engine first checks that its picture agrees with
//! the registry's on those hosts; when it does not, it is built again from every host the
//! registry lists, and the start is counted as `resynced`. That is the only way the engine
//! is built again: a start in the normal course costs time in the hosts it is offered and
//! their leases, not in every host and lease. The engine then places the start among
//! `open` alone, and the registry books the host it names, checking the room as it does
//! (with the CPU slack, [`Placer::cpu_slack_permille`]).
//!
//! How it picks depends on the lease's class:
//!
//! - A class that books part of a host: the hosts' load readings choose among the hosts
//!   that still fit (a host with no fresh reading is charged by what it books), and among
//!   equal hosts the one the lease fits most tightly (so the most booked first), equal ones
//!   by a hash of the lease and the host, so two leases can break the same tie differently.
//! - A class that books the whole host: no score. The registry offers such a lease only
//!   hosts that carry no lease, and the scheduler gives a worker that a request would own
//!   whole no score and takes the first by worker name, which is the host id here
//!   ([`map::worker`]). So the lease takes the first open host by id, whatever its number
//!   or the order the hosts are fed in: the first host by id, whatever the hosts' loads.
//!
//! A host a whole-node request has set aside (a mark of the registry) is shown only to a
//! start that may backfill it (a class that books part of a host, not a probe), with the
//! time left until its tail ends as the mark took it. The scheduler holds that host for the
//! request outside it ([`js_sched::Scheduler::reserve_outside`], ended when the mark goes)
//! and places the start there only if the start's max wall time (which the lease table
//! enforces) ends, with a minute to spare, by then ([`js_sched::Scheduler::with_backfill`]).
//! A start it places elsewhere or nowhere never falls back to a marked host.
//!
//! The engine also keeps the registry's whole-node queue ([`Placer::whole_round`],
//! [`super::engine::whole`]) over its wait list and request records
//! ([`super::engine::wait`]): at each pass the registry hands it
//! its view of the hosts, and shows the marks the engine holds as its reservations. A host
//! one of the engine's own requests has set aside is held by that request in the engine,
//! not outside it, and is backfilled by the same rule against the tail the mark was made
//! with; the hosts held outside are those of the marks a restart put back for requests the
//! engine does not queue yet. A whole-node request's start books the host of its pending
//! grant.
//!
//! The same engine places the queued jobs when the server holds the job tables
//! ([`super::engine::jobs`]), and holds their allocations beside the leases: a start is
//! placed only where both leave room.
//! A job that needs the whole host is queued, marked and committed by the same engine
//! ([`super::engine::whole_jobs`]), from what the job tables hold, with the server's start
//! ([`SchedPlacer::boot`]) deciding how long a stored mark waits for its host to say hello.
//!
//! A host the registry removes is told to the engine and also recorded for the job pass of
//! the tick, which reads it absent and settles the job allocations left on it
//! ([`SchedPlacer::gone`]); a host the engine holds a job allocation on is never forgotten for
//! silence ([`Placer::holds_work_on`]).
//!
//! Locks: the placer's mutex is taken only inside a call the registry makes under its own
//! lock (registry, then placer), and the placer never calls the registry, so the order is
//! fixed. A pass over the queued jobs ([`super::engine::jobs`]) takes its own lock, then the
//! engine's, and reads the registry before either. The record of removed hosts has a lock of
//! its own, a leaf.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_registry::{
    MarkedTails, Placer, PrepareFailure, Presented, ProbeHost, RequestKey, RequestRecord,
    Reservation, SeatedLease, Waiter, WaitingRequest, WholeKept, WholeRound,
};
use js_types::Candidate;
use js_types::sched::FarmTime;
use js_types::sched::usage::NodeLoad;
use js_types::{Clock, Timestamp};
use js_types::{HostId, LeaseId, Request, RequestId, Shape};
use js_types::{RefusalCode, StartSpec};

use tracing::warn;

use super::engine::{Engine, Settings};

/// Places the starts of a registry with `js-sched`, and counts the starts it could not
/// place and those that found its picture out of date.
pub struct SchedPlacer {
    settings: Settings,
    clock: Arc<dyn Clock + Send + Sync>,
    /// The placer's clock when it was made: the server's start, which the marks of the
    /// whole-host jobs the store held then wait from for their hosts' hellos.
    boot: FarmTime,
    engine: Mutex<Engine>,
    /// Held for each pass over the queued jobs, so two passes never decide at once
    /// ([`super::engine::jobs`]); taken before the engine's lock, never under it.
    passes: Mutex<()>,
    unmappable: AtomicU64,
    declined: AtomicU64,
    resynced: AtomicU64,
    /// The hosts the registry removed (given back after the provider read them gone), each
    /// with the number of its removal, until the job pass has settled the job allocations
    /// left on them ([`SchedPlacer::gone`]). Its lock is a leaf.
    gone: Mutex<BTreeMap<HostId, u64>>,
    /// How many removals were recorded.
    removals: AtomicU64,
    /// How many engines this placer built, counted for the tests that hold it to one.
    #[cfg(any(test, feature = "test-support"))]
    built: AtomicU64,
}

/// How many starts fell back to the registry's first host or were placed nowhere, by
/// cause.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, PartialEq, Eq)]
pub struct Fallbacks {
    /// The scheduler could not take the hosts or the request: the first host was booked.
    pub unmappable: u64,
    /// The scheduler placed the start nowhere: none was booked.
    pub declined: u64,
}

impl SchedPlacer {
    /// A placer whose engine is built with `settings`, on `clock` (the registry's).
    pub fn new(settings: Settings, clock: impl Clock + Send + Sync + 'static) -> Self {
        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(clock);
        let now = farm_time(&*clock);
        Self {
            settings,
            engine: Mutex::new(Engine::build(settings, &[], now)),
            passes: Mutex::new(()),
            clock,
            boot: now,
            unmappable: AtomicU64::new(0),
            declined: AtomicU64::new(0),
            resynced: AtomicU64::new(0),
            gone: Mutex::default(),
            removals: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-support"))]
            built: AtomicU64::new(1),
        }
    }

    /// The engine, under its lock.
    pub fn engine(&self) -> MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lock of one pass over the queued jobs.
    pub(super) fn pass(&self) -> MutexGuard<'_, ()> {
        self.passes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The farm time of the placer's clock when the placer was made: the server's start.
    pub fn boot(&self) -> FarmTime {
        self.boot
    }

    /// The farm time of the placer's clock now.
    pub fn now(&self) -> FarmTime {
        farm_time(&*self.clock)
    }

    fn removed(&self) -> MutexGuard<'_, BTreeMap<HostId, u64>> {
        self.gone.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The hosts the registry removed that the job pass has not settled yet, each with the
    /// number of its removal: the provider reads each of them absent (design section 6.3).
    /// A host that says hello again is taken off at its hello.
    pub fn gone(&self) -> BTreeMap<HostId, u64> {
        self.removed().clone()
    }

    /// Takes off each of `settled`, removals the job pass has settled (every job allocation on
    /// the host freed), unless the host was removed again since.
    pub fn settle_gone(&self, settled: &BTreeMap<HostId, u64>) {
        self.removed()
            .retain(|host, removal| settled.get(host) != Some(&*removal));
    }

    /// The fallbacks so far.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fallbacks(&self) -> Fallbacks {
        Fallbacks {
            unmappable: self.unmappable.load(Ordering::Relaxed),
            declined: self.declined.load(Ordering::Relaxed),
        }
    }

    /// The starts at which the engine was built again because it disagreed with the
    /// registry.
    #[cfg(any(test, feature = "test-support"))]
    pub fn resynced(&self) -> u64 {
        self.resynced.load(Ordering::Relaxed)
    }

    /// How many engines this placer built.
    #[cfg(any(test, feature = "test-support"))]
    pub fn built(&self) -> u64 {
        self.built.load(Ordering::Relaxed)
    }

    /// What the engine charges `host` by its load, for a test that reads it: `None` for a
    /// host that is no worker of the engine.
    #[cfg(any(test, feature = "test-support"))]
    pub fn load_of(&self, host: &str) -> Option<js_sched::LoadView> {
        self.engine().load_of(host)
    }

    /// How many hosts the engine keeps, for a test that holds it to the registry's.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hosts_kept(&self) -> usize {
        self.engine().hosts_kept()
    }

    /// The host the engine gives `spec` among `open`, rebuilding the engine from `every`
    /// first when the two pictures disagree on `open`.
    fn grant(
        &self,
        spec: &StartSpec,
        open: &[Candidate],
        marked: &MarkedTails,
        every: &dyn Fn() -> Vec<Candidate>,
    ) -> Result<Option<usize>, super::map::MapError> {
        let nodes = Engine::nodes(open)?;
        let now = self.now();
        let mut engine = self.engine();
        if !engine.agrees(open, spec.lease().sequence()) {
            let rebuilt = engine.rebuild(self.settings, &every(), now);
            *engine = rebuilt;
            #[cfg(any(test, feature = "test-support"))]
            self.built.fetch_add(1, Ordering::Relaxed);
            let count = self.resynced.fetch_add(1, Ordering::Relaxed) + 1;
            warn!(lease = %spec.lease(), count, "the placement engine disagreed with the registry and was rebuilt");
        }
        engine.grant(spec, (open, nodes), marked, now)
    }
}

impl SchedPlacer {
    /// Whether `spec` fits on `candidate` within the engine's share of its memory, beside
    /// the leases already there: what the engine would hold a start it could describe to.
    /// The registry books the whole memory under this placement, so a start that falls
    /// back is held to the share here.
    fn within_share(&self, candidate: &Candidate, spec: &StartSpec) -> bool {
        let booked = candidate.leases.iter().fold(0u64, |sum, held| {
            sum.saturating_add(held.resources.memory_bytes())
        });
        let asked = spec.request().resources.memory_bytes();
        let share = self
            .settings
            .memory
            .of_bytes(candidate.shape.resources().memory_bytes());
        booked.saturating_add(asked) <= share
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for SchedPlacer {
    /// A placer with the default settings on a clock that stands still at zero.
    fn default() -> Self {
        let clock = js_types::ManualClock::new(js_types::Timestamp::from_millis(0));
        Self::new(Settings::default(), clock)
    }
}

/// The farm time of `clock`: its milliseconds.
fn farm_time(clock: &dyn Clock) -> FarmTime {
    FarmTime::from_millis(clock.now().millis())
}

impl Placer for SchedPlacer {
    /// [`Placer::pick_over`] with the hosts it is shown as all the registry lists.
    fn pick(&self, spec: &StartSpec, open: &[Candidate], marked: &MarkedTails) -> Option<usize> {
        self.pick_over(spec, open, marked, &|| open.to_vec())
    }

    /// The host the engine gives the start, as an index of `open`.
    ///
    /// When the scheduler cannot take these hosts or this request (a shape that does not
    /// describe a node, a capability that is not a platform requirement), the first host
    /// not in `marked` with room for it within the engine's share of its memory is booked,
    /// as a registry with no placer books the first host, and the start is counted as
    /// unmappable and logged: the registry's check of the room stands (CPU up to the
    /// slack), the share is held here, and the engine takes the lease up on that host
    /// ([`Engine::adopt`]). With no such host none is booked. When the
    /// engine, agreeing with the registry's books, places the start nowhere (its memory
    /// percent or CPU slack is reached, its readings say the hosts are full), that stands:
    /// no host is booked (`NoCapacity`; the client retries), and the start is counted as
    /// declined and logged. A host in `marked` is booked only when the engine places the
    /// start there as backfill.
    ///
    /// The start of a whole-node request the engine queues ([`Placer::whole_round`]) books
    /// the host of the request's pending grant when it is among `open`, and none otherwise
    /// (the request waits); nothing is counted.
    ///
    /// A probe never falls back: the engine places it on its host, or the start books
    /// none (`NoCapacity`). Its host is cordoned in the engine for that start, as the
    /// registry found it when the probe was admitted there ([`Placer::admit_probe`]).
    fn pick_over(
        &self,
        spec: &StartSpec,
        open: &[Candidate],
        marked: &MarkedTails,
        every: &dyn Fn() -> Vec<Candidate>,
    ) -> Option<usize> {
        // The registry never asks with no host; there is nothing to place or count.
        if open.is_empty() {
            return None;
        }
        // A whole-node request the engine queues books the host of its pending grant, or
        // none while it has none among `open`: it waits, which is no decline.
        let now = self.now();
        let committed = self.engine().commit_whole(spec, open, now);
        if let Some(committed) = committed {
            return committed;
        }
        let granted = self.grant(spec, open, marked, every);
        if spec.request().probe_host.is_some() {
            return granted.ok().flatten();
        }
        match granted {
            Ok(Some(at)) => Some(at),
            Ok(None) => {
                let count = self.declined.fetch_add(1, Ordering::Relaxed) + 1;
                warn!(lease = %spec.lease(), count, "the placement engine placed the start nowhere");
                None
            }
            Err(err) => {
                let first = open
                    .iter()
                    .position(|c| !marked.contains_key(&c.host) && self.within_share(c, spec));
                first.inspect(|at| {
                    // The registry books it there: the engine holds it from now on.
                    self.engine().adopt(&open[*at], spec);
                    let count = self.unmappable.fetch_add(1, Ordering::Relaxed) + 1;
                    let cause = err.to_string();
                    warn!(lease = %spec.lease(), %cause, count, "placement fell back to the first host");
                })
            }
        }
    }

    /// The probe rule of the scheduler ([`js_sched::admit_probe`]) over what the registry
    /// knows of the probe's host ([`super::probe::admit`]), with the host's memory held to
    /// the engine's share of it.
    fn admit_probe(
        &self,
        spec: &StartSpec,
        pin: &HostId,
        host: Option<&ProbeHost>,
    ) -> Result<(), RefusalCode> {
        super::probe::admit(spec, pin, host, self.settings.memory)
    }

    /// The engine's CPU slack: the registry's book lets the leases that share a host book
    /// that far above its cores, so the room the engine may take is not refused there.
    fn cpu_slack_permille(&self) -> u32 {
        self.settings.load.s_cpu_permille
    }

    fn host_up(&self, host: &HostId, shape: &Shape) {
        self.removed().remove(host);
        let now = self.now();
        self.engine().host_up(host, shape, now);
    }

    fn heard(&self, host: &HostId, load: Option<&NodeLoad>) {
        let now = self.now();
        self.engine().heard(host, load, now);
    }

    fn ended(&self, lease: LeaseId) {
        self.engine().ended(lease);
    }

    /// The engine takes the lease up on its host ([`Engine::seat`]).
    fn seated(&self, seated: &SeatedLease) {
        self.engine().seat(seated);
    }

    /// Recorded for the job pass ([`SchedPlacer::gone`]) as well as told to the engine: a
    /// removed host's job allocations are freed only by the pass, never here.
    fn host_gone(&self, host: &HostId) {
        self.engine().host_gone(host);
        let removal = self.removals.fetch_add(1, Ordering::Relaxed);
        self.removed().insert(host.clone(), removal);
    }

    /// A host the engine holds a job allocation on is not forgotten for silence: only its
    /// confirmation or the provider's absence frees that allocation (design section 6.3).
    fn holds_work_on(&self, host: &HostId) -> bool {
        self.engine().holds_jobs_on(host)
    }

    /// The engine keeps the whole-node queue ([`super::engine::whole`]) over its own wait
    /// list ([`super::engine::wait`]): it runs the pass, and holds the marks it makes.
    fn whole_round(&self, round: &WholeRound) -> Option<Vec<Reservation>> {
        let now = self.now();
        let mut engine = self.engine();
        engine.whole_pass(round.clone(), now);
        Some(engine.whole_held(now))
    }

    /// The engine keeps the deadline of each lease it committed from its whole-node queue
    /// ([`super::engine::prepare`]).
    fn prepare_overdue(&self, lease: &LeaseId) -> Option<bool> {
        let now = self.now();
        self.engine().prepare_overdue(lease, now)
    }

    fn prepare_ready(&self, lease: &LeaseId) {
        self.engine().prepare_ready(lease);
    }

    fn prepare_again(&self, lease: &LeaseId) {
        let now = self.now();
        self.engine().prepare_again(lease, now);
    }

    /// The engine counts the failure, bars the host and lets the lease go for a lease it
    /// committed from its queue ([`super::engine::prepare`]).
    fn prepare_failed(&self, lease: &LeaseId, failure: &PrepareFailure) -> bool {
        let now = self.now();
        self.engine().prepare_failed(lease, failure, now)
    }

    fn whole_start(&self, key: &RequestKey) -> bool {
        let now = self.now();
        self.engine().whole_start(key, now)
    }

    fn whole_record(&self, key: &RequestKey) -> Option<RequestRecord> {
        self.engine().whole_record(key)
    }

    fn whole_forget(&self, id: &RequestId) {
        self.engine().whole_forget(id);
    }

    fn whole_sweep(&self, horizon: Duration) {
        let now = self.now();
        self.engine().whole_sweep(horizon, now);
    }

    /// The marks and pending grants of the engine's whole-node requests
    /// ([`super::engine::ledger`]).
    fn whole_reservations(&self) -> Option<Vec<Reservation>> {
        let now = self.now();
        Some(self.engine().whole_held(now))
    }

    /// What the engine keeps of its whole-node requests for the reservation ledger
    /// ([`super::engine::ledger`]).
    fn whole_ledger(&self) -> BTreeMap<RequestId, WholeKept> {
        let now = self.now();
        self.engine().whole_ledger(now)
    }

    /// The engine keeps what a restart's ledger kept of a request
    /// ([`super::engine::ledger`]).
    fn whole_restore(
        &self,
        id: &RequestId,
        kept: &WholeKept,
        terminal: bool,
        since: Timestamp,
    ) -> bool {
        let now = self.now();
        self.engine()
            .whole_restore(id, kept, (terminal, since), now);
        true
    }

    fn whole_present(
        &self,
        id: &RequestId,
        lease: LeaseId,
        request: &Request,
        capable: bool,
        regain_grace: Duration,
    ) -> Presented {
        let now = self.now();
        let presented = (id, lease, request);
        self.engine()
            .whole_present(presented, capable, regain_grace, now)
    }

    fn whole_expire(&self, regain_grace: Duration) -> u64 {
        let now = self.now();
        self.engine().whole_expire(regain_grace, now)
    }

    fn whole_cancel(&self, id: &RequestId) -> bool {
        self.engine().whole_cancel(id)
    }

    fn whole_cancel_lease(&self, lease: &LeaseId) -> Option<RequestId> {
        self.engine().whole_cancel_lease(lease)
    }

    fn whole_bind(&self, id: &RequestId, lease: LeaseId) -> bool {
        self.engine().whole_bind(id, lease)
    }

    fn whole_waiter(&self, id: &RequestId) -> Option<Waiter> {
        self.engine().whole_waiter(id)
    }

    fn whole_waiter_of(&self, lease: &LeaseId) -> Option<Waiter> {
        self.engine().whole_waiter_of(lease)
    }

    fn whole_places(&self) -> Vec<(RequestId, Timestamp)> {
        self.engine().whole_places()
    }

    fn whole_waiting(&self) -> Vec<WaitingRequest> {
        self.engine().whole_waiting()
    }

    fn whole_restore_place(&self, id: &RequestId, since: Timestamp) -> bool {
        let now = self.now();
        self.engine().whole_restore_place(id, since, now)
    }

    fn whole_revive(&self, mark: &Reservation, claim_by: Timestamp) -> bool {
        self.engine().whole_revive(mark, claim_by)
    }

    fn whole_drop_host(&self, host: &HostId) {
        let now = self.now();
        self.engine().whole_drop_host(host, now);
    }

    fn whole_commit(&self, lease: LeaseId, key: &RequestKey) {
        self.engine().whole_commit(lease, key);
    }

    fn whole_held_by_other(&self, id: &RequestId, lease: &LeaseId) -> bool {
        self.engine().whole_held_by_other(id, lease)
    }
}

#[cfg(test)]
#[path = "place_tests.rs"]
mod tests;
