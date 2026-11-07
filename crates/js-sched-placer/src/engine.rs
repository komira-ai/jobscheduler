//! The one `js-sched` scheduler a [`crate::place::SchedPlacer`] keeps for the life of the
//! server, and the picture of the registry's books it keeps beside it.
//!
//! The engine learns the registry's books from the registry's own events, not from a
//! snapshot per start: a host that says hello is a worker up ([`Engine::host_up`]), with
//! the capacity and caps its pool's shape gives ([`map::host`]); an accepted heartbeat is
//! a heartbeat, with the reading the registry vetted and converted ([`Engine::heard`]); a
//! lease the placer places is booked by the placement itself ([`Engine::grant`]), one
//! the registry stops is released ([`Engine::ended`]), and a host the registry forgets is
//! forgotten ([`Engine::host_gone`]).
//!
//! At each start the two pictures are compared on the hosts the start could book
//! ([`Engine::agrees`]): every one must be a worker of the same shape, holding exactly the
//! leases the registry books there. When they disagree (a lease the registry booked that
//! the engine never placed, as one put back after a restart; one the engine gave up for a
//! silent host; a host it was never told of) the engine is built again from the
//! registry's view of every host it lists, keeping each host's last three readings
//! ([`Engine::rebuild`]); that is the only way it is ever built again.
//!
//! The engine places a start only on the hosts the registry offered it
//! ([`Scheduler::place_only_on`]); once the start is booked, the operation is held to no
//! host, so a lease the engine gives up (its host silent past the engine's lease grace)
//! is never placed again by the engine alone: it waits, and the next start that meets
//! that host rebuilds the engine. Every host the registry offers is heard from at that
//! start ([`Event::Capacity`]), so the registry, not the engine's own grace, says which
//! hosts are live.
//!
//! A rebuild keeps the load the engine measured: the last readings of each host are fed
//! to the new scheduler again at the times they came ([`Engine::rebuild`]). A host the
//! registry forgets (lost past its retention, or given back) is forgotten by the engine
//! too, worker and all ([`Engine::host_gone`]).
//!
//! The engine also holds the job allocations of the job table, beside the registry's leases
//! ([`jobs`]): a pass over the queued jobs brings them to the table and places queued jobs.
//! A heartbeat lists them as running, a rebuild carries them over, and a host the registry
//! forgets is kept while it holds one, so the room a lease is offered is what the host's
//! shape leaves after both. A queued job that needs the whole host is queued in the engine
//! as a whole-machine request of the same scheduler ([`whole_jobs`]): the pass marks, grants
//! and cancels it from what the job tables hold, and a rebuild does not carry it.
//!
//! The engine is given no learned curves: each lease is charged what it books, and each
//! host by its load readings when it has fresh ones. Under whole-node settings the engine
//! also keeps the registry's whole-node queue: the wait list of the pool's whole-node
//! requests ([`wait`]) and their records ([`prepare`]), and it marks, grants and cancels
//! them ([`whole`]); its drain cost reads the registry's expected lives
//! of its leases through the scheduler's curve source ([`crate::lives::Lives`]), which
//! gives no curve.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use js_caps::NodeCaps;
use js_provider::Candidate;
use js_provider::StartSpec;
use js_registry::MarkedTails;
use js_sched::harness::feed;
use js_sched::{
    Backfill, DaemonInstance, Event, LoadConfig, MemoryPercent, OpState, Prediction, Recovered,
    RecoveredLease, Request, Scheduler,
};
use js_types::sched::usage::{LeaseClock, LeaseCost, LeaseMinute, NodeLoad};
use js_types::sched::{
    Effect, FarmTime, LeaseId as SchedLeaseId, LeaseKind, OperationId, Qos, WaiterId, WorkKey,
};
use js_types::{HostId, LeaseId, RequestId, Shape};

use crate::lives::Lives;
use crate::map::{self, MapError};

#[path = "jobs.rs"]
pub mod jobs;
#[path = "ledger.rs"]
pub mod ledger;
#[path = "prepare.rs"]
pub mod prepare;
#[path = "wait.rs"]
pub mod wait;
#[path = "whole.rs"]
pub mod whole;
#[path = "whole_jobs.rs"]
pub mod whole_jobs;

/// What the whole-host job tests share: the job tables on the in-memory fake, an engine over
/// hosts, and the settings of the rounds.
#[cfg(test)]
#[path = "whole_jobs_rig.rs"]
mod rig;

/// The term the engine grants the registry's starts in. A start it places is numbered
/// `TERM-<seq>`, with the sequence of the registry's lease, so the tie-break between equal
/// hosts (a hash of the lease and the host) varies by lease as it does in the registry. A
/// lease it takes up when it is built is numbered in term 0, a job's allocation in
/// [`jobs::JOB_TERM`] and a whole-node request's lease in [`whole::WHOLE_TERM`].
const TERM: u64 = 1;

/// How the engine is built: the settings of the server's configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// Whether a start may backfill a host set aside, and up to which wall time.
    pub backfill: Backfill,
    /// The load settings, CPU slack included (`--cpu-slack`).
    pub load: LoadConfig,
    /// The share of each host's memory the engine may book (`memory_schedulable_percent`).
    pub memory: MemoryPercent,
}

/// The scheduler form of a host: its capacity and the capabilities its node report gives.
pub type Node = (js_types::sched::Resources, NodeCaps);

/// A lease of the registry, or a job's allocation, the engine holds.
#[derive(Debug, Clone, Copy)]
pub struct Placed {
    /// The engine's lease.
    pub lease: SchedLeaseId,
    /// The engine's operation.
    pub operation: OperationId,
}

/// The scheduler and what ties it to the registry's books.
#[derive(Debug)]
pub struct Engine {
    sched: Scheduler,
    /// Every host the engine was told of, with the shape the registry books it as.
    hosts: BTreeMap<HostId, Shape>,
    /// The registry's leases on each host, as the engine placed or took them up.
    on: BTreeMap<HostId, BTreeMap<LeaseId, Placed>>,
    /// The registry's lease of each engine lease.
    back: BTreeMap<SchedLeaseId, (HostId, LeaseId)>,
    /// The hosts held for a whole-node request outside the engine.
    outside: BTreeSet<HostId>,
    /// The farm time last fed.
    now: FarmTime,
    /// The number the next lease the engine takes up gets, in term 0.
    taken: u64,
    /// The last readings each host's heartbeats brought in its present session, with
    /// when: what a rebuild feeds the new scheduler again, so that it loses no measured
    /// load ([`Engine::rebuild`]). The three newest: the scheduler charges a host by the
    /// larger of its last two windows, and two windows take three readings.
    readings: BTreeMap<HostId, Vec<(FarmTime, NodeLoad)>>,
    /// The job allocations the engine holds, each with its host and what it books
    /// ([`jobs`]): the live allocations of the job table, and the placements of the pass
    /// under way until their commit answers.
    jobs: BTreeMap<js_jobs::JobId, jobs::JobHold>,
    /// The operation of each whole-node request the engine queues for the registry, until
    /// the registry commits its lease ([`whole`]).
    whole: BTreeMap<RequestId, OperationId>,
    /// The operation of each queued whole-host job the engine queues, until the job is
    /// placed, cancelled or ended ([`whole_jobs`]). A rebuild does not carry it: the next
    /// pass over the job tables queues the jobs again.
    whole_jobs: BTreeMap<js_jobs::JobId, OperationId>,
    /// The whole-node settings the scheduler has now, if it has any.
    whole_settings: Option<js_registry::WholeNodeConfig>,
    /// The give-ups and bars of whole-node requests a rebuild carried over, until each is
    /// queued again ([`Engine::carry_whole`]).
    carried: BTreeMap<RequestId, whole::Carried>,
    /// The whole-node requests a restart's ledger put back ([`Engine::whole_restore`]) and
    /// not queued since, each with when: their give-ups and bars wait in `carried` until
    /// the request is queued, or until the registry's idle horizon has passed.
    restored: BTreeMap<RequestId, FarmTime>,
    /// The prepares of the whole-node leases committed from the queue, and the records of
    /// the requests whose prepare failed ([`prepare`]).
    prepare: prepare::Prepare,
    /// The wait list of the registry's whole-node requests ([`wait`]).
    wait: wait::WaitList,
    /// The last pass of the whole-node queue the registry handed over, which a rebuild
    /// hands to the new engine; it also holds the whole-node settings the scheduler has.
    round: Option<js_registry::WholeRound>,
    /// The sequence the next lease of a whole-node request gets, in [`whole::WHOLE_TERM`].
    whole_seq: u64,
    /// The hosts the engine cordons because the registry stands them lost, or would not book
    /// them now while they carry nothing.
    cordoned: BTreeSet<HostId>,
    /// The hosts the engine gives a phase other than serving because the registry stands
    /// them tainted (quarantined) or leaving (draining).
    phased: BTreeSet<HostId>,
    /// The expected life of each lease the registry knows one for, which the scheduler
    /// reads as its learned curves.
    lives: Arc<Lives>,
}

/// How many readings of a host are kept for a rebuild: two windows, the most the
/// scheduler reads a host's measured load from, take three.
const KEPT_READINGS: usize = 3;

impl Engine {
    /// An engine built with `settings` over `every`, the hosts the registry lists with
    /// the leases on each, at `now`. Every host with a scheduler form is a worker; every
    /// lease is taken up on its host, as a lease recovered after a restart is.
    pub fn build(settings: Settings, every: &[Candidate], now: FarmTime) -> Self {
        let lives = Arc::new(Lives::default());
        let mut sched = Scheduler::new(TERM)
            .with_backfill(settings.backfill)
            .with_load_config(settings.load)
            .with_memory_percent(settings.memory)
            .with_curves(lives.clone());
        let held = every
            .iter()
            .flat_map(|candidate| candidate.leases.iter().map(move |held| (candidate, held)));
        let mut operations = Vec::new();
        let mut holds = Vec::new();
        for (number, (candidate, held)) in (0..).zip(held) {
            let placed = Placed {
                lease: SchedLeaseId::new(0, number),
                operation: OperationId(number),
            };
            operations.push(recovered(placed, &candidate.host, held));
            holds.push((candidate.host.clone(), held.lease, placed));
        }
        let next = OperationId(u64::try_from(operations.len()).unwrap_or(u64::MAX));
        sched.recover(TERM, next, 0, operations);
        let mut engine = Self {
            sched,
            hosts: BTreeMap::new(),
            on: BTreeMap::new(),
            back: BTreeMap::new(),
            outside: BTreeSet::new(),
            now,
            taken: u64::try_from(holds.len()).unwrap_or(u64::MAX),
            readings: BTreeMap::new(),
            jobs: BTreeMap::new(),
            whole: BTreeMap::new(),
            whole_jobs: BTreeMap::new(),
            whole_settings: None,
            carried: BTreeMap::new(),
            restored: BTreeMap::new(),
            prepare: prepare::Prepare::default(),
            wait: wait::WaitList::default(),
            round: None,
            whole_seq: 0,
            cordoned: BTreeSet::new(),
            phased: BTreeSet::new(),
            lives,
        };
        for candidate in every {
            engine.host_up(&candidate.host, &candidate.shape, now);
        }
        for (host, lease, placed) in holds {
            engine.hold(&host, lease, placed);
        }
        engine
    }

    /// An engine built over `every` as [`Engine::build`] builds one, that keeps the load
    /// this one measured: it is built as of the oldest reading this one keeps of a host in
    /// `every`, and each kept reading is fed to it again at the time it came, so a host's
    /// readings are as fresh, or as stale, in the new engine as in this one. The
    /// readings' leases are named as the new engine numbers them. Every host in `every`
    /// is then heard from at `now`, without load: the registry lists it, so it is live,
    /// whatever the age of its last reading.
    pub fn rebuild(&self, settings: Settings, every: &[Candidate], now: FarmTime) -> Self {
        let listed: BTreeSet<&HostId> = every.iter().map(|candidate| &candidate.host).collect();
        let mut replay: Vec<(FarmTime, &HostId, &NodeLoad)> = self
            .readings
            .iter()
            .filter(|(host, _)| listed.contains(host))
            .flat_map(|(host, kept)| kept.iter().map(move |(at, load)| (*at, host, load)))
            .collect();
        replay.sort_by_key(|(at, _, _)| *at);
        let start = replay.first().map_or(now, |(at, _, _)| (*at).min(now));
        let mut engine = Self::build(settings, every, start);
        // The job allocations are not in the registry's books: they are carried over.
        engine.take_jobs(&self.jobs);
        for (at, host, load) in replay {
            engine.heard(host, Some(load), at);
        }
        // Every host the registry lists is live now, as `build` at `now` would have it,
        // whether or not its last beats brought a reading: a host heard only without load
        // since its kept readings is not taken for silent and its leases are not given up.
        for candidate in every {
            engine.heard(&candidate.host, None, now);
        }
        // The whole-node queue is not in the registry's books either: it is carried over.
        engine.carry_whole(self, now);
        engine
    }

    /// `host` is no longer listed by the registry: it is forgotten, worker and all, unless
    /// the engine still holds a lease there (it then waits for the next rebuild) or a job's
    /// allocation (which only its host's confirmation or the provider's absence frees).
    pub fn host_gone(&mut self, host: &HostId) {
        // A whole-node request granted the host and not committed there waits again.
        let worker = map::worker(host);
        self.unplace_whole_where(|on| *on == worker);
        let leased = self.on.get(host).is_some_and(|leases| !leases.is_empty());
        if leased | self.holds_jobs_on(host) {
            return;
        }
        let _ = self.sched.forget_worker(&map::worker(host));
        self.hosts.remove(host);
        self.on.remove(host);
        self.outside.remove(host);
        self.readings.remove(host);
        self.cordoned.remove(host);
        self.phased.remove(host);
    }

    /// Feeds `event` at `now`, or at the last time fed if that is later. A whole-node
    /// request it grants a worker outside its own round is put back
    /// ([`Engine::settle_whole`]).
    pub fn feed(&mut self, now: FarmTime, event: Event) -> Vec<Effect> {
        self.now = self.now.max(now);
        let effects = feed(&mut self.sched, self.now, event);
        self.settle_whole();
        effects
    }

    /// Records that the engine holds `lease` of the registry on `host` as `placed`, and
    /// holds its operation to no host: if the engine gives the lease up, it never places
    /// it again by itself.
    fn hold(&mut self, host: &HostId, lease: LeaseId, placed: Placed) {
        self.sched.place_only_on(placed.operation, BTreeSet::new());
        self.on
            .entry(host.clone())
            .or_default()
            .insert(lease, placed);
        self.back.insert(placed.lease, (host.clone(), lease));
    }

    /// Takes up `spec`'s lease, which the registry booked on `candidate` without the
    /// engine placing it (the fallback of a start the engine cannot describe): it is held
    /// there as a lease recovered after a restart is, booking what the registry books.
    pub fn adopt(&mut self, candidate: &Candidate, spec: &StartSpec) {
        // A lease that books the whole host is taken up as a request that owns its worker.
        let held = js_provider::Held {
            lease: spec.lease(),
            resources: spec.request().resources.clone(),
            whole: spec.class().admission().books_whole_host(),
        };
        let placed = Placed {
            lease: SchedLeaseId::new(0, self.taken),
            operation: self.sched.next_operation(),
        };
        self.taken = self.taken.saturating_add(1);
        let _ = self
            .sched
            .take_in(recovered(placed, &candidate.host, &held));
        self.hold(&candidate.host, spec.lease(), placed);
    }

    /// `host` said hello, booked as `shape`: a worker up, of the capacity and caps the
    /// shape gives. A shape with no scheduler form is remembered, and the host is no
    /// worker: a start offered it falls back ([`Engine::nodes`]).
    pub fn host_up(&mut self, host: &HostId, shape: &Shape, now: FarmTime) {
        if let Ok((capacity, caps)) = map::host(shape) {
            let up = Event::WorkerUp {
                worker: map::worker(host),
                instance: DaemonInstance::new(host.as_str()),
                predecessor_stopped: false,
                capacity,
                caps,
            };
            self.feed(now, up);
        }
        self.hosts.insert(host.clone(), shape.clone());
        // A hello opens a new session: the readings of the last one are no window now.
        self.readings.remove(host);
    }

    /// A heartbeat of `host` the registry accepted, with the reading it brought. The heartbeat
    /// lists every lease, job allocation and pending grant of a whole-node request the engine
    /// holds there as running, so none is given up for a beat; the reading's leases are named
    /// as the engine numbers them, and a lease the engine does not hold is left out of it. The
    /// scheduler takes no heartbeat of a host that is no worker of it.
    pub fn heard(&mut self, host: &HostId, load: Option<&NodeLoad>, now: FarmTime) {
        let here = self.on.get(host);
        let ours = |lease: &SchedLeaseId| {
            let registry = LeaseId::new(lease.term, lease.seq);
            here.and_then(|leases| leases.get(&registry))
                .map(|placed| placed.lease)
        };
        let running = here
            .into_iter()
            .flat_map(|leases| leases.values().map(|placed| placed.lease))
            .chain(self.job_leases_on(host))
            .chain(self.pending_on(&map::worker(host)))
            .collect();
        let translated = load.map(|load| NodeLoad {
            covered: load.covered.iter().filter_map(ours).collect(),
            lease_clocks: load
                .lease_clocks
                .iter()
                .filter_map(|clock| ours(&clock.lease).map(|lease| clock_of(clock, lease)))
                .collect(),
            minutes: load
                .minutes
                .iter()
                .filter_map(|minute| ours(&minute.lease).map(|lease| minute_of(minute, lease)))
                .collect(),
            ..load.clone()
        });
        let beat = Event::Heartbeat {
            worker: map::worker(host),
            running,
            load: translated,
        };
        self.feed(now, beat);
        if let Some(load) = load {
            let kept = self.readings.entry(host.clone()).or_default();
            kept.push((self.now, load.clone()));
            let over = kept.len().saturating_sub(KEPT_READINGS);
            *kept = kept.split_off(over);
        }
    }

    /// The registry stopped `lease`: the engine releases it, whether it still holds it or
    /// gave it up and waits with it, and a whole-node lease's prepare ends with it, counting
    /// nothing ([`prepare`]). A lease the engine does not know changes nothing.
    pub fn ended(&mut self, lease: LeaseId) {
        self.prepare.end(&lease);
        let found = self
            .on
            .iter_mut()
            .find_map(|(_, leases)| leases.remove(&lease));
        let Some(placed) = found else {
            return;
        };
        self.back.remove(&placed.lease);
        // The lease of a whole-node request the engine queued ends with its request.
        match placed.lease.term == whole::WHOLE_TERM {
            true => {
                let _ = self.sched.cancel_whole(placed.operation);
            }
            false => self.release(placed),
        }
    }

    /// Lets go of what `placed` holds in the scheduler: its booking and its operation.
    pub fn release(&mut self, placed: Placed) {
        let queued = self.sched.unplace(placed.lease).unwrap_or(placed.operation);
        let _ = self.sched.forget_queued(queued);
    }

    /// Whether the engine's picture agrees with the registry's on `open`, the hosts a
    /// start numbered `sequence` could book: each is a worker of the shape the registry
    /// books it as, and holds exactly the registry's leases there, none given up; and the
    /// lease the engine would grant for the start is not one it holds.
    pub fn agrees(&self, open: &[Candidate], sequence: u64) -> bool {
        let fresh = !self.back.contains_key(&SchedLeaseId::new(TERM, sequence));
        fresh && open.iter().all(|candidate| self.agrees_on(candidate))
    }

    fn agrees_on(&self, candidate: &Candidate) -> bool {
        let same = self.hosts.get(&candidate.host) == Some(&candidate.shape);
        let empty = BTreeMap::new();
        let here = self.on.get(&candidate.host).unwrap_or(&empty);
        let worker = map::worker(&candidate.host);
        let held = |placed: &Placed| {
            let leased = OpState::Leased {
                lease: placed.lease,
                worker: worker.clone(),
                committed: true,
            };
            self.sched.state(placed.operation) == Some(&leased)
        };
        let listed = candidate.leases.len() == here.len()
            && candidate
                .leases
                .iter()
                .all(|booked| here.get(&booked.lease).is_some_and(held));
        same & listed
    }

    /// The scheduler form of each host of `open`, in its order.
    ///
    /// # Errors
    /// [`MapError`] of the first host that has none.
    pub fn nodes(open: &[Candidate]) -> Result<Vec<Node>, MapError> {
        open.iter()
            .map(|candidate| map::host(&candidate.shape))
            .collect()
    }

    /// Places `spec` on one of `open`, the hosts the registry offers it, as the engine
    /// decides: the index of the host, or `None` when the engine places it nowhere. A host
    /// in `marked` is held for the request that set it aside, until the end of its tail,
    /// and takes the start only as backfill. The engine is assumed to agree with the
    /// registry on `open` ([`Engine::agrees`]).
    ///
    /// The start is a submitted request held to `open`, charged its CPU and memory as it
    /// books them, with the max wall time the lease table enforces; one placement round
    /// later the engine has placed it or not. Placed, it is booked in the engine as the
    /// registry books it; not placed, it is forgotten.
    ///
    /// # Errors
    /// [`MapError`] when the request has no scheduler form.
    pub fn grant(
        &mut self,
        spec: &StartSpec,
        (open, nodes): (&[Candidate], Vec<Node>),
        marked: &MarkedTails,
        now: FarmTime,
    ) -> Result<Option<usize>, MapError> {
        let mut request = map::job(spec.request(), &spec.lease().to_string())?;
        request.whole_worker = spec.class().admission().books_whole_host();
        // Every host offered is live, the registry says, and is heard from now; `nodes`
        // are their scheduler forms ([`Engine::nodes`]).
        for (candidate, (capacity, caps)) in open.iter().zip(nodes) {
            let worker = map::worker(&candidate.host);
            let heard = Event::Capacity {
                worker,
                capacity,
                caps,
            };
            self.feed(now, heard);
        }
        self.mark(marked);
        let pin = request.pin.clone();
        // A probe's host is shown only once it was admitted there: it is cordoned, for
        // this start only.
        if let Some(pin) = &pin {
            self.feed(
                now,
                Event::Cordon {
                    worker: pin.clone(),
                },
            );
        }
        // The whole-node requests are placed first, numbered in their own term.
        self.whole_tick(now);
        let operation = self.sched.next_operation();
        let lease = spec.lease();
        self.sched
            .recover(TERM, operation, lease.sequence(), Vec::new());
        let resources = &spec.request().resources;
        let charge = Prediction {
            cpu_millis: resources.cpu_millis(),
            memory_bytes: resources.memory_bytes(),
            cold: false,
            life_minutes: 1,
        };
        let only = open.iter().map(|c| map::worker(&c.host)).collect();
        // The lease table ends the lease at its wall time: the end backfill reads.
        let granted = self.place_one(
            (operation, request),
            (spec.request().max_wall, charge),
            only,
            now,
        );
        let at = granted.and_then(|(granted, worker)| {
            let on = open.iter().position(|c| map::worker(&c.host) == worker);
            on.map(|at| (at, granted))
        });
        match at {
            Some((index, granted)) => {
                let placed = Placed {
                    lease: granted,
                    operation,
                };
                self.hold(&open[index].host, lease, placed);
            }
            None => {
                let _ = self.sched.forget_queued(operation);
            }
        }
        // A host the engine cordons for the registry stays cordoned.
        let cordoned: BTreeSet<_> = self.cordoned.iter().map(map::worker).collect();
        if let Some(pin) = pin.filter(|pin| !cordoned.contains(pin)) {
            self.feed(now, Event::Uncordon { worker: pin });
        }
        Ok(at.map(|(index, _)| index))
    }

    /// Submits `request` as `operation`, which ends at the latest `max_wall` after it starts
    /// and is charged `charge`, held to the workers `only`, and runs one placement round:
    /// the lease and the worker it was placed on, or `None` when it was placed nowhere (it
    /// is then still queued, and the caller forgets it).
    pub fn place_one(
        &mut self,
        (operation, request): (OperationId, Request),
        (max_wall, charge): (std::time::Duration, Prediction),
        only: BTreeSet<js_types::sched::WorkerId>,
        now: FarmTime,
    ) -> Option<(SchedLeaseId, js_types::sched::WorkerId)> {
        self.sched.set_max_wall(operation, max_wall);
        let submit = Event::Submit {
            waiter: WaiterId(operation.0),
            request,
        };
        self.feed(now, submit);
        self.sched.set_cost(operation, charge);
        self.sched.place_only_on(operation, only);
        self.feed(now, Event::Tick);
        match self.sched.state(operation) {
            Some(OpState::Leased { lease, worker, .. }) => Some((*lease, worker.clone())),
            _ => None,
        }
    }

    /// Holds the hosts of `marked` outside the engine, each until the end of its tail as
    /// the mark took it, and ends the hold of a host no longer marked. A host one of the
    /// engine's own whole-node requests holds is held by that request, not outside.
    fn mark(&mut self, marked: &MarkedTails) {
        let own = self.whole_hosts();
        let marked: MarkedTails = marked
            .iter()
            .filter(|(host, _)| !own.contains(*host))
            .map(|(host, left)| (host.clone(), *left))
            .collect();
        let gone: Vec<HostId> = self
            .outside
            .iter()
            .filter(|host| !marked.contains_key(*host))
            .cloned()
            .collect();
        for host in gone {
            self.sched.release_outside(&map::worker(&host));
            self.outside.remove(&host);
        }
        for (host, left) in &marked {
            let end = left.map(|left| millis(left).saturating_add(self.now.as_millis()));
            let start = end.map(FarmTime::from_millis);
            self.sched.reserve_outside(map::worker(host), start);
            self.outside.insert(host.clone());
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Engine {
    /// The engine's scheduler, for a test that reads it.
    pub fn sched(&self) -> &Scheduler {
        &self.sched
    }

    /// The engine's scheduler, for a test that drives it.
    pub fn sched_mut(&mut self) -> &mut Scheduler {
        &mut self.sched
    }

    /// The scheduler's load view of `host`.
    pub fn load_of(&self, host: &str) -> Option<js_sched::LoadView> {
        self.sched.load_view(&js_types::sched::WorkerId::new(host))
    }

    /// How many hosts the engine keeps, as hosts, workers and kept readings: the largest.
    pub fn hosts_kept(&self) -> usize {
        let workers = self
            .hosts
            .keys()
            .filter(|host| self.sched.capacity(&map::worker(host)).is_some());
        let workers = workers.count();
        self.hosts.len().max(self.readings.len()).max(workers)
    }
}

/// The recovered operation that stands for the lease `held` on `host`, numbered as
/// `placed` says: charged its CPU and memory as it books them.
fn recovered(placed: Placed, host: &HostId, held: &js_provider::Held) -> Recovered {
    let key = WorkKey::new("held", held.lease.to_string());
    let booked = map::booking(&held.resources);
    recovered_as(placed, host, key, (booked, held.whole))
}

/// The recovered operation named `key` that stands for a booking of `booked` on `host`, the
/// whole host when `whole`, numbered as `placed` says: charged its CPU and memory as it books
/// them.
fn recovered_as(
    placed: Placed,
    host: &HostId,
    key: WorkKey,
    (booked, whole): (js_types::sched::Resources, bool),
) -> Recovered {
    let number = placed.operation.0;
    let request = Request {
        key,
        qos: Qos::Batch,
        kind: LeaseKind::Action,
        resources: booked,
        needs: js_caps::Request::default(),
        hermetic: false,
        do_not_cache: true,
        whole_worker: whole,
        pin: None,
    };
    let cost = LeaseCost {
        cpu_millis: booked.cpu_millis,
        memory_bytes: booked.memory_bytes,
        limit_bytes: booked.memory_bytes,
        cold: false,
        retry: false,
        key_hash: 0,
    };
    Recovered {
        operation: placed.operation,
        request,
        waiter: WaiterId(number),
        lease: Some(RecoveredLease {
            lease: placed.lease,
            worker: map::worker(host),
            cost: Some(cost),
        }),
        losses: 0,
        fate: None,
    }
}

/// `clock` of the engine's `lease`.
fn clock_of(clock: &LeaseClock, lease: SchedLeaseId) -> LeaseClock {
    LeaseClock { lease, ..*clock }
}

/// `minute` of the engine's `lease`.
fn minute_of(minute: &LeaseMinute, lease: SchedLeaseId) -> LeaseMinute {
    LeaseMinute { lease, ..*minute }
}

/// `duration` in milliseconds, saturating.
fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use js_provider::Held;
    use js_types::sched::WorkerId;
    use js_types::sched::usage::{LeaseClock, LeaseMinute};
    use js_types::{Resources, Shape};

    use super::*;

    /// A shape of 8 cores and 16 GiB, with an `arch` when `arch` says so.
    fn shape(arch: bool) -> Shape {
        let caps = arch
            .then(|| ("arch".to_owned(), "x86_64".to_owned()))
            .into_iter()
            .collect();
        Shape::new(Resources::new(8000, 16 << 30, 0, None).unwrap(), caps)
    }

    /// `name` booked as `shape`, carrying the one-core leases `1-<seq>` of `seqs`.
    fn candidate(name: &str, shape: Shape, seqs: &[u64]) -> Candidate {
        let leases = seqs.iter().map(|seq| Held {
            lease: LeaseId::new(1, *seq),
            resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
            whole: false,
        });
        Candidate {
            host: HostId::new(name).unwrap(),
            shape,
            leases: leases.collect(),
        }
    }

    fn at(ms: u64) -> FarmTime {
        FarmTime::from_millis(ms)
    }

    /// A reading numbered `seq` naming the registry's leases `leases` as covered, with a
    /// clock and a minute of each.
    fn reading(seq: u64, leases: &[SchedLeaseId]) -> NodeLoad {
        NodeLoad {
            seq,
            mono_usec: seq * 5_000_000,
            cpu_usage_usec: seq * 5_000_000,
            memory_working_set_bytes: 1 << 30,
            covered: leases.to_vec(),
            lease_clocks: leases
                .iter()
                .map(|lease| LeaseClock {
                    lease: *lease,
                    started_mono_usec: 0,
                })
                .collect(),
            minutes: leases
                .iter()
                .map(|lease| LeaseMinute {
                    lease: *lease,
                    minute: 0,
                    cpu_mean_millis: 500,
                    cpu_peak_window_millis: 900,
                    ws_max_bytes: 1 << 20,
                })
                .collect(),
            ..NodeLoad::default()
        }
    }

    /// Catches: a host whose shape has no scheduler form made a worker anyway, or its
    /// leases dropped from the picture. Built over `a` (no `arch`) and `b`, the engine has
    /// a worker for `b` alone, holding `b`'s lease as the second it took up, and still
    /// records `a`'s lease on `a`.
    #[test]
    fn a_host_with_no_scheduler_form_is_no_worker() {
        let every = [
            candidate("a", shape(false), &[1]),
            candidate("b", shape(true), &[2]),
        ];
        let engine = Engine::build(Settings::default(), &every, at(0));
        assert!(engine.sched.capacity(&WorkerId::new("a")).is_none());
        assert!(engine.sched.capacity(&WorkerId::new("b")).is_some());
        let b = HostId::new("b").unwrap();
        assert_eq!(
            engine.sched.leases_on(&map::worker(&b)),
            [SchedLeaseId::new(0, 1)]
        );
        let a = HostId::new("a").unwrap();
        assert_eq!(engine.on[&a].len(), 1);
        assert!(Engine::nodes(&every).is_err());
        assert_eq!(Engine::nodes(&every[1..]).map(|nodes| nodes.len()), Ok(1));
    }

    /// Catches: a reading's leases passed in the registry's numbering (the engine would
    /// see none of its own covered and charge each lease again on top of the working set)
    /// and a lease the engine does not hold kept in it. After two readings that cover the
    /// registry's lease `1-2`, the engine counts it covered; readings that name only a
    /// lease it does not hold leave its lease uncovered.
    #[test]
    fn a_reading_names_the_engines_own_leases() {
        let every = [candidate("b", shape(true), &[2])];
        let b = HostId::new("b").unwrap();
        let worker = map::worker(&b);
        let ours = SchedLeaseId::new(1, 2);
        let other = SchedLeaseId::new(7, 7);
        let mut covered = Engine::build(Settings::default(), &every, at(0));
        let mut stray = Engine::build(Settings::default(), &every, at(0));
        for seq in 1..=2 {
            let now = at(seq * 5_000);
            covered.heard(&b, Some(&reading(seq, &[ours, other])), now);
            stray.heard(&b, Some(&reading(seq, &[other])), now);
        }
        let view = |engine: &Engine| engine.sched.load_view(&worker).unwrap();
        assert_eq!(view(&covered).uncovered_cpu_millis, 0);
        assert_eq!(view(&stray).uncovered_cpu_millis, 1000);
        assert!(!view(&covered).fallback);
    }

    /// Catches: a heartbeat of a host the engine was never told of, or the stop of a lease
    /// it never held, changing what it holds or panicking.
    #[test]
    fn what_the_engine_does_not_know_changes_nothing() {
        let every = [candidate("b", shape(true), &[2])];
        let mut engine = Engine::build(Settings::default(), &every, at(0));
        let stranger = HostId::new("z").unwrap();
        engine.heard(&stranger, Some(&reading(1, &[])), at(1));
        engine.heard(&stranger, None, at(2));
        engine.ended(LeaseId::new(5, 5));
        let b = HostId::new("b").unwrap();
        assert_eq!(engine.sched.leases_on(&map::worker(&b)).len(), 1);
        assert_eq!(engine.on.len(), 1);
        assert!(engine.agrees(&every, 3));
    }

    /// Catches: a rebuild that dates a host's liveness by its kept readings, so that a host
    /// whose later beats brought no load is taken for silent and gives its leases up.
    /// `a` holds a lease and sent readings up to 15 s, then beats without load; rebuilt at
    /// 200 s, well past the lease grace after its last reading, the engine still holds
    /// `a`'s lease after a tick and agrees with the registry, so the next start needs no
    /// second rebuild.
    #[test]
    fn a_rebuild_keeps_a_host_heard_without_load_live() {
        let every = [
            candidate("a", shape(true), &[2]),
            candidate("b", shape(true), &[]),
        ];
        let a = HostId::new("a").unwrap();
        let mut engine = Engine::build(Settings::default(), &every, at(0));
        for seq in 1..=3 {
            engine.heard(&a, Some(&reading(seq, &[])), at(seq * 5_000));
        }
        for second in (20..=200).step_by(5) {
            engine.heard(&a, None, at(second * 1_000));
        }
        let mut rebuilt = engine.rebuild(Settings::default(), &every, at(200_000));
        rebuilt.feed(at(200_000), Event::Tick);
        assert_eq!(rebuilt.sched.leases_on(&map::worker(&a)).len(), 1);
        assert!(rebuilt.agrees(&every, 9));
    }

    /// Catches: an agreement check that counts the leases on a host and does not read
    /// them, or reads the engine's maps and not its scheduler. Shown `b` with another lease
    /// in place of the one the engine holds there, the engine disagrees; after `b` is
    /// silent past the engine's lease grace, so that the engine gives its lease up while
    /// the registry still lists it, it disagrees too, and the given-up lease is not placed
    /// again by the engine alone.
    #[test]
    fn the_engine_disagrees_on_other_leases_and_on_a_lease_it_gave_up() {
        let every = [candidate("b", shape(true), &[2])];
        let mut engine = Engine::build(Settings::default(), &every, at(0));
        assert!(engine.agrees(&every, 9));
        let other = [candidate("b", shape(true), &[3])];
        assert!(!engine.agrees(&other, 9));
        engine.feed(at(120_000), Event::Tick);
        let b = HostId::new("b").unwrap();
        assert!(engine.sched.leases_on(&map::worker(&b)).is_empty());
        assert!(!engine.agrees(&every, 9));
        engine.heard(&b, None, at(121_000));
        engine.feed(at(122_000), Event::Tick);
        assert!(engine.sched.leases_on(&map::worker(&b)).is_empty());
    }
}
