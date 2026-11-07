//! The registry's placer, with `js-sched` choosing the host.
//!
//! [`SchedPlacer`] keeps one scheduler for the life of the server ([`Engine`]), built once
//! from the server's configuration: the backfill ceiling (`server.max_lease_wall`), the
//! CPU slack (`--cpu-slack`, js-sched's default 1.1x unless given) and the share of each
//! host's memory it may book (`memory_schedulable_percent` of the pool). The registry
//! tells it of every hello, accepted heartbeat, stop and host it forgets as they happen
//! ([`Placer::host_up`], [`Placer::heard`], [`Placer::ended`], [`Placer::host_gone`]), so
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
//! without a placer (with the CPU slack, [`Placer::cpu_slack_permille`]).
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
//!   or the order the hosts are fed in: the host a registry with no placer books.
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
//! [`super::engine::whole`]): at each pass the registry hands it the wait list and its view
//! of the hosts, and shows the marks the engine answers as its reservations. A host one of
//! the engine's own requests has set aside is held by that request in the engine, not
//! outside it, and is backfilled by the same rule against the tail the mark was made with;
//! the hosts held outside are those of requests put back from the ledger and not presented
//! since. A whole-node request's start books the host of its pending grant.
//!
//! The same engine places the queued jobs when the server holds the job tables
//! ([`super::engine::jobs`]), and holds their allocations beside the leases: a start is
//! placed only where both leave room.
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

use js_lease::{Clock, Timestamp};
use js_provider::Candidate;
use js_provider::{RefusalCode, StartSpec};
use js_registry::{
    MarkedTails, Placer, PrepareFailure, ProbeHost, RequestRecord, Reservation, WholeKept,
    WholeRound,
};
use js_types::sched::FarmTime;
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, LeaseId, RequestId, Shape};

use tracing::warn;

use super::engine::{Engine, Settings};

/// Places the starts of a registry with `js-sched`, and counts the starts it could not
/// place and those that found its picture out of date.
pub struct SchedPlacer {
    settings: Settings,
    clock: Arc<dyn Clock + Send + Sync>,
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
        let clock = js_lease::ManualClock::new(js_lease::Timestamp::from_millis(0));
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

    /// The engine keeps the whole-node queue ([`super::engine::whole`]): it runs the pass.
    fn whole_round(&self, round: &WholeRound) -> Option<Vec<Reservation>> {
        let now = self.now();
        Some(self.engine().whole_round(round.clone(), now))
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

    fn whole_start(&self, id: &RequestId) -> bool {
        let now = self.now();
        self.engine().whole_start(id, now)
    }

    fn whole_record(&self, id: &RequestId) -> Option<RequestRecord> {
        self.engine().whole_record(id)
    }

    fn whole_forget(&self, id: &RequestId) {
        self.engine().whole_forget(id);
    }

    fn whole_sweep(&self, horizon: Duration) {
        let now = self.now();
        self.engine().whole_sweep(horizon, now);
    }

    /// The holds the engine's whole-node requests have given up at their deadline
    /// ([`Engine::whole_give_ups`]).
    fn whole_give_ups(&self) -> BTreeMap<RequestId, u32> {
        self.engine().whole_give_ups()
    }

    /// The marks and pending grants of the engine's whole-node requests
    /// ([`super::engine::ledger`]).
    fn whole_reservations(&self) -> Option<Vec<Reservation>> {
        let now = self.now();
        Some(self.engine().whole_reservations(now))
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
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use js_provider::Held;
    use js_sched::Backfill;
    use js_types::{Admission, ClassId, HostId, LeaseId, Request as HostRequest};
    use js_types::{ResourceClass, Resources, Shape};

    use super::*;

    /// No host set aside.
    fn unmarked() -> MarkedTails {
        MarkedTails::new()
    }

    fn shape(arch: &[(&str, &str)]) -> Shape {
        let caps = arch
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Shape::new(Resources::new(8000, 16 << 30, 0, None).unwrap(), caps)
    }

    fn x86() -> Shape {
        shape(&[("arch", "x86_64")])
    }

    /// A host named `name` carrying one lease of each of `cpus` millis, which share it.
    fn host(name: &str, shape: Shape, cpus: &[u64]) -> Candidate {
        let leases = (1..).zip(cpus).map(|(n, cpu)| Held {
            lease: LeaseId::new(1, n),
            resources: Resources::new(*cpu, 1 << 20, 0, None).unwrap(),
            whole: false,
        });
        Candidate {
            host: HostId::new(name).unwrap(),
            shape,
            leases: leases.collect(),
        }
    }

    fn sharing() -> ResourceClass {
        let id = ClassId::new("share").unwrap();
        ResourceClass::new(id, Admission::Book(Default::default()), 0)
    }

    fn spec_of(class: &ResourceClass, sequence: u64) -> StartSpec {
        spec_with(class, sequence, &[])
    }

    fn spec_with(class: &ResourceClass, sequence: u64, caps: &[(&str, &str)]) -> StartSpec {
        let request = HostRequest {
            class: class.id().clone(),
            resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
            capabilities: caps
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
            probe_host: None,
        };
        let known = std::slice::from_ref(class);
        StartSpec::new(LeaseId::new(2, sequence), request, known).unwrap()
    }

    fn spec() -> StartSpec {
        spec_of(&sharing(), 1)
    }

    /// Catches: the first host by name taken in place of the scheduler's rank, or the
    /// leases on a host left out of it. The scheduler's score prefers the host the lease
    /// fits most tightly, which here is the one that carries most: `c`, then `b`.
    #[test]
    fn the_host_the_lease_fits_most_tightly_is_picked() {
        let open = [
            host("a", x86(), &[2000]),
            host("b", x86(), &[1000]),
            host("c", x86(), &[3000]),
        ];
        assert_eq!(
            SchedPlacer::default().pick(&spec(), &open, &unmarked()),
            Some(2)
        );
        let open = [
            host("a", x86(), &[1000]),
            host("b", x86(), &[1000, 1000]),
            host("c", x86(), &[]),
        ];
        assert_eq!(
            SchedPlacer::default().pick(&spec(), &open, &unmarked()),
            Some(1)
        );
    }

    /// Catches: equal hosts always broken the same way (the first by name, or the lease
    /// number left out of the scheduler's tie-break hash): the scheduler breaks a tie by a
    /// hash of the lease and the host, which prefers `b` for lease 1 and `a` for lease 3.
    /// A lease that books the whole host takes the first by name.
    #[test]
    fn equal_hosts_are_told_apart_by_the_lease() {
        let open = [host("a", x86(), &[]), host("b", x86(), &[])];
        assert_eq!(
            SchedPlacer::default().pick(&spec_of(&sharing(), 1), &open, &unmarked()),
            Some(1)
        );
        assert_eq!(
            SchedPlacer::default().pick(&spec_of(&sharing(), 3), &open, &unmarked()),
            Some(0)
        );
        let id = ClassId::new("box").unwrap();
        let whole = ResourceClass::new(id, Admission::BookAll, 0);
        assert_eq!(
            SchedPlacer::default().pick(&spec_of(&whole, 1), &open, &unmarked()),
            Some(0)
        );
    }

    /// Catches: the module header's account of a whole-host lease going stale. A lease that
    /// books the whole host is offered only empty hosts, and takes the first by host id
    /// whatever the order they are offered in and whatever its number, where a lease that
    /// shares is placed by the hash tie-break and so lands on more than one of them.
    #[test]
    fn a_whole_host_lease_takes_the_first_open_host_by_id() {
        let id = ClassId::new("box").unwrap();
        let whole = ResourceClass::new(id, Admission::BookAll, 0);
        let orders = [["a", "b", "c"], ["c", "b", "a"], ["b", "c", "a"]];
        let mut shared_hosts = std::collections::BTreeSet::new();
        for names in orders {
            let open: Vec<Candidate> = names.iter().map(|name| host(name, x86(), &[])).collect();
            let first = names.iter().position(|name| *name == "a");
            for sequence in 1..=16 {
                let placer = SchedPlacer::default();
                let picked = placer.pick(&spec_of(&whole, sequence), &open, &unmarked());
                assert_eq!(picked, first, "order {names:?}, lease {sequence}");
                let shared = placer
                    .pick(&spec_of(&sharing(), sequence), &open, &unmarked())
                    .unwrap();
                shared_hosts.insert(names[shared]);
                let none = Fallbacks {
                    unmappable: 0,
                    declined: 0,
                };
                assert_eq!(placer.fallbacks(), none);
            }
        }
        assert!(shared_hosts.len() > 1, "{shared_hosts:?}");
    }

    /// Catches: a host the scheduler cannot describe (no `arch`) failing the start or being
    /// skipped silently: the registry's first host is booked, as without a placer, and the
    /// fallback is counted as unmappable. With no host there is nothing to book or count.
    #[test]
    fn hosts_that_do_not_describe_a_node_fall_back_to_the_first_and_are_counted() {
        let placer = SchedPlacer::default();
        let open = [host("a", shape(&[]), &[3000]), host("b", x86(), &[])];
        assert_eq!(placer.pick(&spec(), &open, &unmarked()), Some(0));
        assert_eq!(placer.pick(&spec(), &[], &unmarked()), None);
        let counted = Fallbacks {
            unmappable: 1,
            declined: 0,
        };
        assert_eq!(placer.fallbacks(), counted);
    }

    /// Catches: a request the scheduler cannot describe (a capability that is not a
    /// platform requirement) failing the start or being placed nowhere: the first host is
    /// booked, where the scheduler would have taken `b`, which carries the more, and the
    /// fallback is counted.
    #[test]
    fn a_request_that_is_not_a_platform_requirement_falls_back_to_the_first() {
        let placer = SchedPlacer::default();
        let open = [host("a", x86(), &[]), host("b", x86(), &[3000])];
        assert_eq!(placer.pick(&spec(), &open, &unmarked()), Some(1));
        let odd = spec_with(&sharing(), 1, &[("colour", "red")]);
        assert_eq!(placer.pick(&odd, &open, &unmarked()), Some(0));
        assert_eq!(placer.fallbacks().unmappable, 1);
    }

    /// Catches: a lease the fallback booked left out of the engine, so that the next start
    /// meeting its host finds the two pictures apart and rebuilds the engine. The first
    /// start (the engine knows neither host yet) builds it once; the odd request falls back
    /// to `a`, and the next start, shown `a` with that lease on it, agrees with the engine
    /// and builds nothing more.
    #[test]
    fn a_lease_the_fallback_booked_is_taken_up_without_a_rebuild() {
        let placer = SchedPlacer::default();
        let open = [host("a", x86(), &[]), host("b", x86(), &[])];
        let odd = spec_with(&sharing(), 1, &[("colour", "red")]);
        assert_eq!(placer.pick(&odd, &open, &unmarked()), Some(0));
        assert_eq!((placer.resynced(), placer.built()), (1, 2));
        let booked = Held {
            lease: odd.lease(),
            resources: odd.request().resources.clone(),
            whole: false,
        };
        let mut after = open.clone();
        after[0].leases.push(booked);
        assert_eq!(
            placer.pick(&spec_of(&sharing(), 2), &after, &unmarked()),
            Some(0)
        );
        assert_eq!((placer.resynced(), placer.built()), (1, 2));
        assert_eq!(placer.fallbacks().unmappable, 1);
    }

    /// Catches: a start the engine places nowhere booked anyway on the registry's first
    /// host, past the engine's own rules (here its one host is booked in full by a lease,
    /// which the candidate lists though the registry offered it): once the engine agrees
    /// with the registry's books, its decline stands, no host is booked and the start is
    /// counted as declined, not as unmappable.
    #[test]
    fn a_start_the_scheduler_declines_books_nothing_and_is_counted() {
        let placer = SchedPlacer::default();
        let open = [host("a", x86(), &[8000])];
        assert_eq!(placer.pick(&spec(), &open, &unmarked()), None);
        let counted = Fallbacks {
            unmappable: 0,
            declined: 1,
        };
        assert_eq!(placer.fallbacks(), counted);
    }

    /// `spec_with` numbered `sequence` with `caps`, a probe pinned to `pin`.
    fn probe_with(sequence: u64, pin: &str, caps: &[(&str, &str)]) -> StartSpec {
        let mut request = spec_with(&sharing(), sequence, caps).request().clone();
        request.probe_host = Some(HostId::new(pin).unwrap());
        StartSpec::new(LeaseId::new(2, sequence), request, &[sharing()]).unwrap()
    }

    /// Catches: a probe that falls back to the first host when the scheduler places it
    /// nowhere (its host full), which would oversubscribe the host, and a probe the
    /// scheduler refuses because its host is not cordoned in the scheduler (it would never
    /// be placed). Neither is counted as a fallback.
    #[test]
    fn a_probe_is_placed_on_its_cordoned_host_or_nowhere_and_never_falls_back() {
        let placer = SchedPlacer::default();
        let probe = probe_with(1, "a", &[]);
        assert_eq!(
            placer.pick(&probe, &[host("a", x86(), &[3000])], &unmarked()),
            Some(0)
        );
        assert_eq!(
            placer.pick(&probe, &[host("a", x86(), &[8000])], &unmarked()),
            None
        );
        let odd = probe_with(2, "a", &[("colour", "red")]);
        assert_eq!(
            placer.pick(&odd, &[host("a", x86(), &[])], &unmarked()),
            None
        );
        let none = Fallbacks {
            unmappable: 0,
            declined: 0,
        };
        assert_eq!(placer.fallbacks(), none);
    }

    /// Catches: a probe whose request or host the scheduler cannot describe judged by
    /// some other rule, or admitted: it is refused as a probe the server cannot place.
    #[test]
    fn a_probe_with_no_scheduler_form_is_refused_as_unsupported() {
        let placer = SchedPlacer::default();
        let a = HostId::new("a").unwrap();
        let known = |shape| ProbeHost {
            shape,
            cordoned: true,
            phase: js_agent_wire::Phase::Serving,
            taint: None,
            leaving: false,
        };
        let probe = probe_with(1, "a", &[]);
        assert_eq!(placer.admit_probe(&probe, &a, Some(&known(x86()))), Ok(()));
        let unsupported = Err(RefusalCode::ProbeUnsupported);
        let no_arch = known(shape(&[]));
        assert_eq!(placer.admit_probe(&probe, &a, Some(&no_arch)), unsupported);
        let odd = probe_with(2, "a", &[("colour", "red")]);
        assert_eq!(
            placer.admit_probe(&odd, &a, Some(&known(x86()))),
            unsupported
        );
        let unknown = Err(RefusalCode::ProbeHostUnknown);
        assert_eq!(placer.admit_probe(&probe, &a, None), unknown);
    }

    /// A placer that backfills, trusting any wall time.
    fn backfilling() -> SchedPlacer {
        let settings = Settings {
            backfill: Backfill::enforced(Duration::MAX),
            ..Settings::default()
        };
        let clock = js_lease::ManualClock::new(js_lease::Timestamp::from_millis(0));
        SchedPlacer::new(settings, clock)
    }

    /// `hosts` marked, each with the time left until its tail ends in seconds (`None`: not
    /// known).
    fn marks(hosts: &[(&str, Option<u64>)]) -> MarkedTails {
        let left = |s: &Option<u64>| s.map(Duration::from_secs);
        let named = hosts
            .iter()
            .map(|(h, s)| (HostId::new(h).unwrap(), left(s)));
        named.collect()
    }

    /// Catches: a start the scheduler cannot build (a host with no `arch`) falling back
    /// onto a marked host. With the marked `a` first and the unmarked `b` after it, `b` is
    /// booked and counted; with every host marked, none is booked and nothing is counted.
    #[test]
    fn an_unmappable_start_falls_back_to_the_first_unmarked_host_only() {
        let placer = backfilling();
        let open = [host("a", x86(), &[]), host("b", shape(&[]), &[])];
        let a_marked = marks(&[("a", Some(600))]);
        assert_eq!(placer.pick(&spec(), &open, &a_marked), Some(1));
        let both = marks(&[("a", Some(600)), ("b", Some(600))]);
        assert_eq!(placer.pick(&spec(), &open, &both), None);
        let counted = Fallbacks {
            unmappable: 1,
            declined: 0,
        };
        assert_eq!(placer.fallbacks(), counted);
    }

    /// Catches: a start the scheduler declines booked on a marked host or on the first
    /// unmarked one. `a` is marked with a tail too short for the 60 s lease and `b` is full
    /// in the scheduler's eyes: the scheduler places it nowhere and nothing is booked; with
    /// `b` marked too, the same. Each decline is counted.
    #[test]
    fn a_declined_start_books_no_host_marked_or_not() {
        let placer = backfilling();
        let open = [host("a", x86(), &[1000]), host("b", x86(), &[8000])];
        let short = marks(&[("a", Some(100))]);
        assert_eq!(placer.pick(&spec(), &open, &short), None);
        let both = marks(&[("a", Some(100)), ("b", Some(600))]);
        assert_eq!(placer.pick(&spec(), &open, &both), None);
        let counted = Fallbacks {
            unmappable: 0,
            declined: 2,
        };
        assert_eq!(placer.fallbacks(), counted);
    }

    /// Catches: one marked host's tail read for another. Four marked hosts, each with a
    /// lease: `a`'s tail has 100 s left (too short for a 60 s lease and its minute), `b`'s
    /// is unknown, `c`'s is past, and `d`'s has 600 s left. In every order they are offered
    /// in, the lease goes to `d`; with `d` unmarked and full it goes nowhere.
    #[test]
    fn each_marked_host_is_held_to_its_own_tail() {
        let tails = marks(&[
            ("a", Some(100)),
            ("b", None),
            ("c", Some(0)),
            ("d", Some(600)),
        ]);
        let orders = [
            ["a", "b", "c", "d"],
            ["d", "c", "b", "a"],
            ["c", "a", "d", "b"],
        ];
        for names in orders {
            let open: Vec<Candidate> = names.iter().map(|n| host(n, x86(), &[1000])).collect();
            let picked = backfilling().pick(&spec(), &open, &tails).unwrap();
            assert_eq!(names[picked], "d", "{names:?}");
        }
        let open = [host("a", x86(), &[1000]), host("b", x86(), &[1000])];
        let none_fit = marks(&[("a", Some(100)), ("b", Some(119))]);
        assert_eq!(backfilling().pick(&spec(), &open, &none_fit), None);
        let fits = marks(&[("a", Some(100)), ("b", Some(120))]);
        assert_eq!(backfilling().pick(&spec(), &open, &fits), Some(1));
    }

    /// Catches: the start of a whole-node request the engine queues placed as an ordinary
    /// start (it would book the first host it is shown, not the one granted to it), or
    /// counted as a decline while it waits. The placer keeps the queue: `a` is not
    /// bookable, so `r` is granted the quiet `b`. Shown `a` alone, its start books
    /// nothing; shown both, it books `b`. Nothing is counted.
    #[test]
    fn a_whole_node_start_books_the_host_of_its_pending_grant() {
        use js_lease::Timestamp;
        use js_registry::{Standing, WaitingWhole, WholeHost, WholeNodeConfig};
        let placer = SchedPlacer::default();
        let open = [host("a", x86(), &[]), host("b", x86(), &[])];
        for candidate in &open {
            placer.host_up(&candidate.host, &candidate.shape);
        }
        let id = js_types::RequestId::new("r").unwrap();
        let waiting = WaitingWhole {
            id: id.clone(),
            max_wall: Duration::from_secs(600),
            since: Timestamp::from_millis(0),
            give_ups: 0,
            exclusions: BTreeMap::new(),
            mark: None,
        };
        let stand = |name: &str, open: bool| WholeHost {
            id: HostId::new(name).unwrap(),
            standing: Standing::Live,
            open,
        };
        let round = WholeRound {
            settings: WholeNodeConfig::dedicated(2),
            waiting: vec![waiting],
            hosts: vec![stand("a", false), stand("b", true)],
            outside: MarkedTails::new(),
            lives: BTreeMap::new(),
        };
        let marks = placer.whole_round(&round).unwrap();
        let held: Vec<(String, bool)> = marks
            .iter()
            .map(|mark| (mark.host.to_string(), mark.assigned))
            .collect();
        assert_eq!(held, [("b".to_owned(), true)]);
        // A grant gives nothing up: the registry is told of no give-up.
        assert!(placer.whole_give_ups().is_empty());
        let start = spec_of(&ResourceClass::whole_node(), 7).with_request_id(id);
        assert_eq!(placer.pick(&start, &open[..1], &unmarked()), None);
        assert_eq!(placer.pick(&start, &open, &unmarked()), Some(1));
        let none = Fallbacks {
            unmappable: 0,
            declined: 0,
        };
        assert_eq!(placer.fallbacks(), none);
    }

    /// Catches: the default placer backfilling: it books no marked host, whatever its
    /// tail.
    #[test]
    fn the_default_placer_books_no_marked_host() {
        let open = [host("a", x86(), &[1000]), host("b", x86(), &[1000])];
        let tails = marks(&[("a", Some(3_600)), ("b", Some(3_600))]);
        assert_eq!(SchedPlacer::default().pick(&spec(), &open, &tails), None);
    }
}
