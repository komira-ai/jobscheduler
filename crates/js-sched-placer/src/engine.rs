//! The one `js-sched` scheduler a [`crate::place::SchedPlacer`] keeps for the life of the
//! server, and the picture of the registry's hosts it keeps beside it.
//!
//! The engine learns the registry's hosts from the registry's events, not from a snapshot:
//! a host that says hello is a worker up ([`Engine::host_up`]), with the capacity and caps
//! its pool's shape gives ([`map::host`]); an accepted heartbeat is a heartbeat, with the
//! reading the registry vetted and converted ([`Engine::heard`]); and a host the registry
//! forgets is forgotten ([`Engine::host_gone`]), worker and all, unless the engine holds a
//! job's allocation there.
//!
//! The engine holds the job allocations of the job table ([`jobs`]): a pass over the queued
//! jobs brings them to the table and places queued jobs. A heartbeat lists them as running,
//! and a host the registry forgets is kept while it holds one. A queued job that needs the
//! whole host is queued in the engine as a whole-machine request of the same scheduler
//! ([`whole_jobs`]): the pass marks, grants and cancels it from what the job tables hold.
//!
//! The engine is given no learned curves: each allocation is charged what it books, and each
//! host by its load readings when it has fresh ones. The leases a reading covers are the
//! engine's own: the job allocations it held on the host at the previous reading the scheduler
//! accepted and holds still, which ran through the whole window between the two. An allocation
//! the engine took before the newest reading but after the one before it ran for part of the
//! window at most: the reading has seen it, and it is charged in full on top of the window
//! before the newest, which holds none of its use, and not on top of the measurement. Only an
//! allocation taken after the newest reading is charged in full beside the measurement.
//!
//! Of the hosts with room for a job, the engine's scheduler takes the emptiest
//! ([`js_sched::FitShape::Spread`]), not the fullest one that fits: a job's booking is its
//! class's mean CPU, and the jobs of a build arrive together, so a host filled to its cores
//! by bookings runs far more than its cores while the hosts beside it stand idle. Hosts a
//! provider launches are still packed, so that the idle ones can be released
//! ([`js_sched::ProviderRank::launching`]).

use std::collections::{BTreeMap, BTreeSet};

use js_caps::NodeCaps;
use js_sched::harness::feed;
use js_sched::{
    Backfill, DaemonInstance, Event, FitShape, LoadConfig, MemoryPercent, OpState, Prediction,
    Recovered, RecoveredLease, Request, Scheduler,
};
use js_types::sched::usage::{LeaseCost, NodeLoad};
use js_types::sched::{
    Effect, FarmTime, LeaseId as SchedLeaseId, LeaseKind, OperationId, Qos, WaiterId, WorkKey,
};
use js_types::{HostId, Shape};

use crate::map;
use crate::room::Room;
use crate::sizes::{ColdMemoryOn, ColdSize, Curves, JobSizes};

#[path = "jobs.rs"]
pub mod jobs;
#[path = "jobs_seen.rs"]
pub(crate) mod jobs_seen;
pub use jobs_seen::TablesFrom;
#[path = "refusals.rs"]
pub mod refusals;
#[path = "whole.rs"]
pub mod whole;
#[path = "whole_jobs.rs"]
pub mod whole_jobs;

/// What the whole-host job tests share: the job tables on the in-memory fake, an engine over
/// hosts, and the settings of the rounds.
#[cfg(test)]
#[path = "whole_jobs_rig.rs"]
pub(crate) mod rig;

/// The term of the scheduler's leases the engine numbers itself: an allocation of the job
/// table it takes up is numbered in term 0, a job's placement in [`jobs::JOB_TERM`] and a
/// whole-host job's grant in [`whole::WHOLE_TERM`].
const TERM: u64 = 1;

/// How the engine is built: the settings of the server's configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// Whether a job may backfill a host set aside, and up to which wall time.
    pub backfill: Backfill,
    /// The load settings, CPU slack included (`--cpu-slack`).
    pub load: LoadConfig,
    /// The share of each host's memory the engine may book (`memory_schedulable_percent`).
    pub memory: MemoryPercent,
    /// What a job that books nothing books while its class has no estimate
    /// (`server.cold_cpu_millis`, `server.cold_memory_bytes`); by default 22 cores and 4 GiB.
    pub cold: ColdSize,
    /// Whether the history learns a CPU and a memory profile for each class
    /// (`--placement-curves`); off by default. Placement is the same in both.
    pub curves: Curves,
}

/// The scheduler form of a host: its capacity and the capabilities its node report gives.
pub type Node = (js_types::sched::Resources, NodeCaps);

/// A job's allocation the engine holds.
#[derive(Debug, Clone, Copy)]
pub struct Placed {
    /// The engine's lease.
    pub lease: SchedLeaseId,
    /// The engine's operation.
    pub operation: OperationId,
}

/// The scheduler and what ties it to the registry's hosts.
#[derive(Debug)]
pub struct Engine {
    sched: Scheduler,
    /// Every host the engine was told of, with the shape the registry books it as.
    hosts: BTreeMap<HostId, Shape>,
    /// The farm time last fed.
    now: FarmTime,
    /// The number the next allocation the engine takes up gets, in term 0.
    taken: u64,
    /// The job allocations the engine holds, each with its host and what it books
    /// ([`jobs`]): the live allocations of the job table, and the placements of the pass
    /// under way until their commit answers.
    jobs: BTreeMap<js_jobs::JobId, jobs::JobHold>,
    /// The operation of each queued whole-host job the engine queues, until the job is
    /// placed, cancelled or ended ([`whole_jobs`]).
    whole_jobs: BTreeMap<js_jobs::JobId, OperationId>,
    /// The whole-node settings the scheduler has now, if it has any.
    whole_settings: Option<js_registry::WholeNodeConfig>,
    /// The job leases the engine held on each host when the scheduler last accepted a load
    /// reading of it: the leases the next reading can cover. A host whose reading the
    /// scheduler dropped has none.
    at_reading: BTreeMap<HostId, BTreeSet<SchedLeaseId>>,
    /// The history the jobs that book nothing are sized from ([`crate::sizes`]).
    sizes: JobSizes,
    /// Why hosts refused the queued jobs the walks offered them, until a pass announces it
    /// ([`refusals`]).
    refused: refusals::Book,
    /// The sequence the next grant of a whole-host job gets, in [`whole::WHOLE_TERM`].
    whole_seq: u64,
    /// What each open host has room for in the walk under way, which a placement is held to
    /// ([`crate::room`]); empty (every host has room) until a pass sets it.
    room: Room,
    /// How many placement rounds the walks over the queued jobs ran, for the tests that
    /// count them ([`jobs`]).
    #[cfg(any(test, feature = "test-support"))]
    job_rounds: u64,
}

impl Engine {
    /// An engine built with `settings` at `now`, over no host: the registry's hosts come with
    /// [`Engine::host_up`]. Its scheduler spreads the jobs over the hosts with room
    /// ([`FitShape::Spread`]), but for the hosts of a provider that launches them.
    pub fn build(settings: Settings, now: FarmTime) -> Self {
        let sched = Scheduler::new(TERM)
            .with_backfill(settings.backfill)
            .with_load_config(settings.load)
            .with_memory_percent(settings.memory)
            .with_score_fit(FitShape::Spread);
        Self {
            sched,
            hosts: BTreeMap::new(),
            now,
            taken: 0,
            jobs: BTreeMap::new(),
            whole_jobs: BTreeMap::new(),
            whole_settings: None,
            at_reading: BTreeMap::new(),
            sizes: JobSizes::with_cold(settings.cold).with_curves(settings.curves),
            refused: refusals::Book::default(),
            whole_seq: 0,
            room: Room::default(),
            #[cfg(any(test, feature = "test-support"))]
            job_rounds: 0,
        }
    }

    /// The same engine, a job of an operating system `on` names booking that cold memory while
    /// its class has no estimate, in place of [`Settings::cold`]'s ([`ColdMemoryOn`]).
    #[must_use]
    pub fn with_cold_memory_on(mut self, on: ColdMemoryOn) -> Self {
        self.sizes = std::mem::take(&mut self.sizes).with_cold_memory_on(on);
        self
    }

    /// Holds the walks that follow to `room`, the ledger a pass builds ([`Room::of`]).
    pub(crate) fn set_room(&mut self, room: Room) {
        self.room = room;
    }

    /// Adds `booked`, an allocation the pass committed on `host` outside the walk (a whole-host
    /// job), to the ledger the walk that follows is held to.
    pub(crate) fn room_add(&mut self, host: &HostId, booked: js_jobs::Booked) {
        self.room.add(host, booked);
    }

    /// The history the jobs that book nothing are sized from.
    pub(crate) fn sizes(&mut self) -> &mut JobSizes {
        &mut self.sizes
    }

    /// The bookable capacity of every host the engine knows as a worker.
    pub(crate) fn capacities(&self) -> Vec<js_types::sched::Resources> {
        let workers = self.hosts.keys().map(map::worker);
        workers
            .filter_map(|worker| self.sched.capacity(&worker))
            .collect()
    }

    /// Whether the engine knows `host` booked as `shape`.
    pub fn knows(&self, host: &HostId, shape: &Shape) -> bool {
        self.hosts.get(host) == Some(shape)
    }

    /// `host` is no longer listed by the registry: it is forgotten, worker and all, unless
    /// the engine still holds a job's allocation there (which only its host's confirmation or
    /// the provider's absence frees).
    pub fn host_gone(&mut self, host: &HostId) {
        // A whole-host job granted the host and not committed there waits again.
        let worker = map::worker(host);
        self.unplace_whole_where(|on| *on == worker);
        if self.holds_jobs_on(host) {
            return;
        }
        let _ = self.sched.forget_worker(&map::worker(host));
        self.at_reading.remove(host);
        self.hosts.remove(host);
    }

    /// Feeds `event` at `now`, or at the last time fed if that is later. A whole-host job it
    /// grants a worker outside its own round is put back ([`Engine::settle_whole`]).
    pub fn feed(&mut self, now: FarmTime, event: Event) -> Vec<Effect> {
        self.now = self.now.max(now);
        let effects = feed(&mut self.sched, self.now, event);
        self.settle_whole();
        effects
    }

    /// `host` said hello, booked as `shape`: a worker up, of the capacity and caps the
    /// shape gives. A shape with no scheduler form is remembered, and the host is no
    /// worker: the jobs are never offered it.
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
    }

    /// A heartbeat of `host` the registry accepted, with the reading it brought. The heartbeat
    /// lists every job allocation and pending grant of a whole-host job the engine holds
    /// there as running, so none is given up for a beat. The per-lease parts of the reading
    /// name the leases of the previous release's protocol, which the engine holds none of, and
    /// are dropped; the reading covers instead the job leases the engine held on the host at
    /// the previous reading the scheduler accepted and still holds, which ran through the
    /// whole window between the two. A lease the engine took after that reading, a pending
    /// grant of a whole-host job and a host's first reading after a dropped one cover
    /// nothing. The scheduler takes no heartbeat of a host that is no worker of it.
    pub fn heard(&mut self, host: &HostId, load: Option<&NodeLoad>, now: FarmTime) {
        let _ = self.heard_changed(host, load, now);
    }

    /// [`Engine::heard`], answering whether the heartbeat changed what a pass over the queued
    /// jobs reads of the engine: it brought a load reading, or the scheduler gave an
    /// allocation up for it (moved it back to its queue, or answered with an effect). A
    /// heartbeat that does neither only says the host is alive, which every pass says again
    /// of the hosts it offers.
    pub(crate) fn heard_changed(
        &mut self,
        host: &HostId,
        load: Option<&NodeLoad>,
        now: FarmTime,
    ) -> bool {
        let before = (self.sched.leases_held(), self.sched.queued_len());
        let jobs: BTreeSet<SchedLeaseId> = self.job_leases_on(host).collect();
        let running = jobs
            .iter()
            .copied()
            .chain(self.pending_on(&map::worker(host)))
            .collect();
        let covered = self.at_reading.get(host).map_or_else(Vec::new, |earlier| {
            earlier.intersection(&jobs).copied().collect()
        });
        let translated = load.map(|load| NodeLoad {
            covered,
            lease_clocks: Vec::new(),
            minutes: Vec::new(),
            ..load.clone()
        });
        let accepted = self.sched.load_counters().readings_accepted;
        let beat = Event::Heartbeat {
            worker: map::worker(host),
            running,
            load: translated,
        };
        let effects = self.feed(now, beat);
        let after = (self.sched.leases_held(), self.sched.queued_len());
        if load.is_some() {
            if self.sched.load_counters().readings_accepted > accepted {
                self.at_reading.insert(host.clone(), jobs);
            } else {
                self.at_reading.remove(host);
            }
        }
        load.is_some() | !effects.is_empty() | (before != after)
    }

    /// Lets go of what `placed` holds in the scheduler: its booking and its operation.
    pub fn release(&mut self, placed: Placed) {
        let queued = self.sched.unplace(placed.lease).unwrap_or(placed.operation);
        let _ = self.sched.forget_queued(queued);
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

    /// How many hosts the engine keeps, as hosts and workers: the larger.
    pub fn hosts_kept(&self) -> usize {
        let workers = self
            .hosts
            .keys()
            .filter(|host| self.sched.capacity(&map::worker(host)).is_some());
        self.hosts.len().max(workers.count())
    }
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

#[cfg(test)]
#[path = "engine_measured_tests.rs"]
mod measured_tests;

#[cfg(test)]
mod tests {
    use js_types::Resources;
    use js_types::sched::WorkerId;
    use js_types::sched::usage::{LeaseClock, LeaseMinute};

    use super::*;

    /// A shape of 8 cores and 16 GiB, with an `arch` when `arch` says so.
    fn shape(arch: bool) -> Shape {
        let caps = arch
            .then(|| ("arch".to_owned(), "x86_64".to_owned()))
            .into_iter()
            .collect();
        Shape::new(Resources::new(8000, 16 << 30, 0, None).unwrap(), caps)
    }

    fn at(ms: u64) -> FarmTime {
        FarmTime::from_millis(ms)
    }

    fn host(name: &str) -> HostId {
        HostId::new(name).unwrap()
    }

    /// A reading numbered `seq` that names `leases` as covered, with a clock and a minute of
    /// each.
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

    /// A host whose shape has no scheduler form (no `arch`) is remembered but is no worker of
    /// the scheduler; one that has the form is a worker.
    #[test]
    fn a_host_with_no_scheduler_form_is_no_worker() {
        let mut engine = Engine::build(Settings::default(), at(0));
        engine.host_up(&host("a"), &shape(false), at(0));
        engine.host_up(&host("b"), &shape(true), at(0));
        assert!(engine.sched.capacity(&WorkerId::new("a")).is_none());
        assert!(engine.sched.capacity(&WorkerId::new("b")).is_some());
        assert_eq!(engine.hosts.len(), 2);
    }

    /// A heartbeat of a host the engine was never told of, or of one that has no worker,
    /// changes nothing and does not panic; a host that is forgotten leaves the engine whole.
    #[test]
    fn what_the_engine_does_not_know_changes_nothing() {
        let mut engine = Engine::build(Settings::default(), at(0));
        engine.host_up(&host("b"), &shape(true), at(0));
        let stranger = host("z");
        engine.heard(&stranger, Some(&reading(1, &[])), at(1));
        engine.heard(&stranger, None, at(2));
        engine.host_gone(&stranger);
        assert_eq!(engine.hosts.len(), 1);
        assert!(engine.sched.capacity(&WorkerId::new("b")).is_some());
    }

    /// A reading's per-lease parts name leases the engine holds none of, so they are dropped
    /// from the scheduler's reading: the host's load is charged with no lease covered.
    #[test]
    fn a_reading_keeps_no_lease_parts() {
        let mut engine = Engine::build(Settings::default(), at(0));
        let b = host("b");
        engine.host_up(&b, &shape(true), at(0));
        let named = [SchedLeaseId::new(1, 2)];
        for seq in 1..=2 {
            let changed = engine.heard_changed(&b, Some(&reading(seq, &named)), at(seq * 5_000));
            assert!(changed);
        }
        let view = engine.sched.load_view(&WorkerId::new("b")).unwrap();
        assert_eq!(view.uncovered_cpu_millis, 0);
        engine.host_gone(&b);
        assert_eq!(engine.hosts_kept(), 0);
    }

    /// A heartbeat with no reading that gives nothing up changes nothing the passes read.
    #[test]
    fn a_beat_without_a_reading_is_no_change() {
        let mut engine = Engine::build(Settings::default(), at(0));
        let b = host("b");
        engine.host_up(&b, &shape(true), at(0));
        assert!(engine.heard_changed(&b, Some(&reading(1, &[])), at(5_000)));
        assert!(!engine.heard_changed(&b, None, at(6_000)));
    }

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    /// A reading numbered `seq` of a host that has used `used` millicore-windows (a window is
    /// 5 s, a millicore-window 5 ms of one core) since the agent started, with a working set
    /// of `ws` and no per-lease parts: the difference of two readings is the cores busy over
    /// the window between them, in millicores.
    fn measured(seq: u64, used: u64, ws: u64) -> NodeLoad {
        NodeLoad {
            seq,
            mono_usec: seq * 5_000_000,
            cpu_usage_usec: used * 5_000,
            memory_working_set_bytes: ws,
            ..NodeLoad::default()
        }
    }

    /// The engine holding `n` jobs `held-<i>` on host `b` of the 8-core, 16 GiB shape, each
    /// booking `cpu` millicores and `memory` bytes.
    fn holding(n: u64, cpu: u64, memory: u64) -> (Engine, HostId) {
        let mut engine = Engine::build(Settings::default(), at(0));
        let b = host("b");
        engine.host_up(&b, &shape(true), at(0));
        for i in 0..n {
            let job = js_jobs::JobId::parse(&format!("held-{i}")).unwrap();
            let booked = js_jobs::Booked::Shape {
                cpu_millis: cpu,
                memory_bytes: memory,
                disk_bytes: 0,
                accelerators: 0,
            };
            engine.take_job(&job, &b, booked, Vec::new());
        }
        (engine, b)
    }

    /// Places the job `name` booking `cpu` and `memory` on host `b` as a pass does, and holds
    /// it: whether the engine placed it.
    fn place(engine: &mut Engine, b: &HostId, name: &str, (cpu, memory): (u64, u64)) -> bool {
        let job = js_jobs::JobId::parse(name).unwrap();
        let operation = engine.sched.next_operation();
        let request = Request {
            key: WorkKey::new("job", job.as_str()),
            qos: Qos::Batch,
            kind: LeaseKind::Action,
            resources: js_types::sched::Resources::new(cpu, memory),
            needs: js_caps::Request::default(),
            hermetic: false,
            do_not_cache: true,
            whole_worker: false,
            pin: None,
        };
        let charge = Prediction {
            cpu_millis: cpu,
            memory_bytes: memory,
            cold: false,
            life_minutes: 1,
        };
        let only = BTreeSet::from([map::worker(b)]);
        let wall = std::time::Duration::from_secs(3600);
        let now = engine.now;
        let Some((lease, _)) = engine.place_one((operation, request), (wall, charge), only, now)
        else {
            return false;
        };
        engine.hold_job(&job, b, Vec::new(), Placed { lease, operation });
        true
    }

    /// How many jobs of `cpu` millicores and 64 MiB the engine places on `b` before one is
    /// refused.
    fn fill(engine: &mut Engine, b: &HostId, cpu: u64) -> usize {
        let mut placed = 0;
        while place(engine, b, &format!("new-{placed}"), (cpu, 64 * MIB)) {
            placed += 1;
        }
        placed
    }

    /// A host whose jobs use exactly what they book (8 of 1 core on 8 cores) is filled up
    /// to the slack limit, 8.8 cores, once two readings have shown it: the jobs the engine
    /// held at the first reading and holds at the second are covered, so their use is not
    /// charged again beside their booking.
    #[test]
    fn jobs_that_use_their_booking_fill_the_host_to_the_slack_limit() {
        let (mut engine, b) = holding(8, 1000, 64 * MIB);
        engine.heard(&b, Some(&measured(1, 0, 0)), at(5_000));
        engine.heard(&b, Some(&measured(2, 8000, 8 * 64 * MIB)), at(10_000));
        let view = engine.load_of("b").unwrap();
        assert_eq!(view.uncovered_cpu_millis, 0);
        assert_eq!(view.left_cpu_millis, 8000);
        assert_eq!(fill(&mut engine, &b, 100), 8);
        assert_eq!(engine.load_of("b").unwrap().left_cpu_millis, 8800);
    }

    /// A job placed after the previous reading is charged in full beside the measurement
    /// at the next one, and covered from the one after.
    #[test]
    fn a_job_placed_after_the_previous_reading_is_charged_in_full() {
        let (mut engine, b) = holding(8, 1000, 64 * MIB);
        engine.heard(&b, Some(&measured(1, 0, 0)), at(5_000));
        assert!(place(&mut engine, &b, "late", (500, 64 * MIB)));
        engine.heard(&b, Some(&measured(2, 8000, 0)), at(10_000));
        let view = engine.load_of("b").unwrap();
        assert_eq!(view.uncovered_cpu_millis, 500);
        assert_eq!(view.left_cpu_millis, 8500);
        assert!(!place(&mut engine, &b, "over", (400, 64 * MIB)));
        assert!(place(&mut engine, &b, "fits", (300, 64 * MIB)));
        engine.heard(&b, Some(&measured(3, 16_500, 0)), at(15_000));
        let view = engine.load_of("b").unwrap();
        assert_eq!(view.uncovered_cpu_millis, 300);
        assert_eq!(view.left_cpu_millis, 8800);
    }

    /// Behaviour, at the figures a host of 88 cores logged: four jobs of 2.5 cores held
    /// through two readings (measured 9 cores), then a burst of 77.75 cores placed after
    /// the second. The third reading measures 38.457 cores, part of which the burst used:
    /// the burst is added to the held jobs' 10 cores (above the 9 of the window before
    /// it), 87.75 cores, and that is raised to the measurement; not the measurement plus
    /// the burst (116.207 cores, over the 96.8 cores allowed, which refused every job). A
    /// job placed after that reading is added on top, and when a reading measures more
    /// than the host books, the measurement plus the jobs placed after it counts.
    /// Catches the jobs a reading has seen charged again beside its measurement.
    #[test]
    fn jobs_placed_before_a_reading_are_not_charged_again_beside_its_measurement() {
        let mut engine = Engine::build(Settings::default(), at(0));
        let b = host("b");
        let caps = std::iter::once(("arch".to_owned(), "x86_64".to_owned())).collect();
        let shape = Shape::new(Resources::new(88_000, 256 * GIB, 0, None).unwrap(), caps);
        engine.host_up(&b, &shape, at(0));
        for i in 0..4 {
            let job = js_jobs::JobId::parse(&format!("held-{i}")).unwrap();
            let booked = js_jobs::Booked::Shape {
                cpu_millis: 2_500,
                memory_bytes: 64 * MIB,
                disk_bytes: 0,
                accelerators: 0,
            };
            engine.take_job(&job, &b, booked, Vec::new());
        }
        engine.heard(&b, Some(&measured(1, 0, GIB)), at(5_000));
        engine.heard(&b, Some(&measured(2, 9_000, GIB)), at(10_000));
        assert_eq!(engine.load_of("b").unwrap().left_cpu_millis, 10_000);
        for i in 0..31 {
            assert!(place(
                &mut engine,
                &b,
                &format!("burst-{i}"),
                (2_500, 64 * MIB)
            ));
        }
        assert!(place(&mut engine, &b, "burst-last", (250, 64 * MIB)));
        assert_eq!(engine.load_of("b").unwrap().left_cpu_millis, 87_750);
        engine.heard(&b, Some(&measured(3, 9_000 + 38_457, GIB)), at(15_000));
        let view = engine.load_of("b").unwrap();
        assert_eq!(view.uncovered_cpu_millis, 77_750);
        assert_eq!(view.left_cpu_millis, 87_750);
        assert!(place(&mut engine, &b, "after", (250, 64 * MIB)));
        assert_eq!(engine.load_of("b").unwrap().left_cpu_millis, 88_000);
        engine.heard(&b, Some(&measured(4, 47_457 + 95_000, GIB)), at(20_000));
        assert_eq!(engine.load_of("b").unwrap().left_cpu_millis, 95_000);
        assert!(place(&mut engine, &b, "fits", (1_500, 64 * MIB)));
        assert_eq!(engine.load_of("b").unwrap().left_cpu_millis, 96_500);
        assert!(!place(&mut engine, &b, "over", (500, 64 * MIB)));
        assert!(place(&mut engine, &b, "edge", (300, 64 * MIB)));
    }

    /// A reading the scheduler drops (its sequence goes back) leaves nothing covered: the
    /// reading after it is a baseline, and only the leases held at that one are covered by
    /// the next.
    #[test]
    fn a_dropped_reading_covers_nothing_until_two_are_accepted() {
        let (mut engine, b) = holding(2, 1000, 64 * MIB);
        engine.heard(&b, Some(&measured(5, 0, 0)), at(5_000));
        engine.heard(&b, Some(&measured(6, 2000, 0)), at(10_000));
        assert_eq!(engine.load_of("b").unwrap().uncovered_cpu_millis, 0);
        engine.heard(&b, Some(&measured(2, 6000, 0)), at(15_000));
        assert!(!engine.at_reading.contains_key(&b));
        engine.heard(&b, Some(&measured(7, 4000, 0)), at(20_000));
        assert_eq!(engine.load_of("b").unwrap().uncovered_cpu_millis, 2000);
        engine.heard(&b, Some(&measured(8, 6000, 0)), at(25_000));
        assert_eq!(engine.load_of("b").unwrap().uncovered_cpu_millis, 0);
    }

    /// A job the engine let go of leaves the covered set, and a host forgotten keeps no
    /// record of its readings.
    #[test]
    fn a_released_job_leaves_the_covered_set_and_a_forgotten_host_its_record() {
        let (mut engine, b) = holding(2, 1000, 64 * MIB);
        engine.heard(&b, Some(&measured(1, 0, 0)), at(5_000));
        assert_eq!(engine.at_reading.get(&b).map(BTreeSet::len), Some(2));
        let gone = engine.jobs.keys().next().cloned().unwrap();
        let lease = engine.job_leases_on(&b).next().unwrap();
        engine.jobs.remove(&gone).unwrap();
        engine.sched.unplace(lease).unwrap();
        engine.heard(&b, Some(&measured(2, 1000, 0)), at(10_000));
        let view = engine.load_of("b").unwrap();
        assert_eq!(view.predicted_cpu_millis, 1000);
        assert_eq!(view.uncovered_cpu_millis, 0);
        engine.jobs.clear();
        engine.host_gone(&b);
        assert!(engine.at_reading.is_empty());
    }

    /// Memory is filled to the bookings and not past them (the flat sum of the bookings is
    /// the limit), and a working set above the covered jobs' bookings (residue, kernel
    /// memory) still counts whole, so the memory it holds is not offered again.
    #[test]
    fn memory_is_filled_to_the_bookings_and_a_larger_working_set_still_refuses() {
        let (mut engine, b) = holding(3, 1000, 4 * GIB);
        engine.heard(&b, Some(&measured(1, 0, 0)), at(5_000));
        engine.heard(&b, Some(&measured(2, 3000, 12 * GIB)), at(10_000));
        assert!(!place(&mut engine, &b, "big", (500, 5 * GIB)));
        assert!(place(&mut engine, &b, "exact", (500, 4 * GIB)));
        assert!(!place(&mut engine, &b, "more", (500, 64 * MIB)));

        let (mut engine, b) = holding(2, 1000, 4 * GIB);
        engine.heard(&b, Some(&measured(1, 0, 0)), at(5_000));
        engine.heard(&b, Some(&measured(2, 2000, 12 * GIB)), at(10_000));
        assert!(!place(&mut engine, &b, "over", (500, 5 * GIB)));
        assert!(place(&mut engine, &b, "fits", (500, 4 * GIB)));
    }
}
