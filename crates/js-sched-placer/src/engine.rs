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
//! host by its load readings when it has fresh ones.

use std::collections::{BTreeMap, BTreeSet};

use js_caps::NodeCaps;
use js_sched::harness::feed;
use js_sched::{
    Backfill, DaemonInstance, Event, LoadConfig, MemoryPercent, OpState, Prediction, Recovered,
    RecoveredLease, Request, Scheduler,
};
use js_types::sched::usage::{LeaseCost, NodeLoad};
use js_types::sched::{
    Effect, FarmTime, LeaseId as SchedLeaseId, LeaseKind, OperationId, Qos, WaiterId, WorkKey,
};
use js_types::{HostId, Shape};

use crate::map;

#[path = "jobs.rs"]
pub mod jobs;
#[path = "jobs_seen.rs"]
pub(crate) mod jobs_seen;
#[path = "whole.rs"]
pub mod whole;
#[path = "whole_jobs.rs"]
pub mod whole_jobs;

/// What the whole-host job tests share: the job tables on the in-memory fake, an engine over
/// hosts, and the settings of the rounds.
#[cfg(test)]
#[path = "whole_jobs_rig.rs"]
mod rig;

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
    /// The sequence the next grant of a whole-host job gets, in [`whole::WHOLE_TERM`].
    whole_seq: u64,
    /// How many placement rounds the walks over the queued jobs ran, for the tests that
    /// count them ([`jobs`]).
    #[cfg(any(test, feature = "test-support"))]
    job_rounds: u64,
}

impl Engine {
    /// An engine built with `settings` at `now`, over no host: the registry's hosts come with
    /// [`Engine::host_up`].
    pub fn build(settings: Settings, now: FarmTime) -> Self {
        let sched = Scheduler::new(TERM)
            .with_backfill(settings.backfill)
            .with_load_config(settings.load)
            .with_memory_percent(settings.memory);
        Self {
            sched,
            hosts: BTreeMap::new(),
            now,
            taken: 0,
            jobs: BTreeMap::new(),
            whole_jobs: BTreeMap::new(),
            whole_settings: None,
            whole_seq: 0,
            #[cfg(any(test, feature = "test-support"))]
            job_rounds: 0,
        }
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
    /// there as running, so none is given up for a beat; the per-lease parts of the reading
    /// name the leases of the previous release's protocol, which the engine holds none of, and
    /// are dropped. The scheduler takes no heartbeat of a host that is no worker of it.
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
        let running = self
            .job_leases_on(host)
            .chain(self.pending_on(&map::worker(host)))
            .collect();
        let translated = load.map(|load| NodeLoad {
            covered: Vec::new(),
            lease_clocks: Vec::new(),
            minutes: Vec::new(),
            ..load.clone()
        });
        let beat = Event::Heartbeat {
            worker: map::worker(host),
            running,
            load: translated,
        };
        let effects = self.feed(now, beat);
        let after = (self.sched.leases_held(), self.sched.queued_len());
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
}
