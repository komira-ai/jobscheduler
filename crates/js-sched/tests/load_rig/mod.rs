//! A small rig for tests of admission by load: a scheduler with workers, actions that
//! carry a predicted cost, and readings fed as a worker's heartbeat. The control log is
//! in process (a `Commit` is fed straight back). Time is the rig's own, in
//! milliseconds, moved only by the test.

#![allow(dead_code, reason = "each test binary uses some of the rig")]

use std::collections::BTreeMap;

use js_caps::NodeCaps;
use js_sched::harness::feed;
use js_sched::{DaemonInstance, Event, LoadConfig, Prediction, Request, Scheduler};
use js_types::sched::usage::NodeLoad;
use js_types::sched::{
    Effect, FarmTime, LeaseId, LeaseKind, OperationId, Outcome, Qos, Resources, ResultRef,
    StartLease, WaiterId, WorkKey, WorkerId,
};

pub const GIB: u64 = 1 << 30;
pub const MIB: u64 = 1 << 20;
/// The window `W` of the default configuration, in milliseconds.
pub const W_MS: u64 = 5_000;

pub fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

pub fn caps(driver: &str) -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers([driver])
}

/// What a worker's agent counts: the sequence and the two cumulative clocks.
#[derive(Clone, Copy, Debug, Default)]
struct Gauge {
    seq: u64,
    mono_usec: u64,
    cpu_usec: u64,
}

pub struct Rig {
    pub s: Scheduler,
    pub ms: u64,
    next: u64,
    gauges: BTreeMap<String, Gauge>,
}

impl Rig {
    /// A scheduler configured with `config`, at time 0.
    pub fn new(config: LoadConfig) -> Self {
        Self {
            s: Scheduler::new(1).with_load_config(config),
            ms: 0,
            next: 0,
            gauges: BTreeMap::new(),
        }
    }

    /// A scheduler with the default configuration (the predicate on).
    pub fn on() -> Self {
        Self::new(LoadConfig::default())
    }

    pub fn now(&self) -> FarmTime {
        FarmTime::from_millis(self.ms)
    }

    pub fn feed(&mut self, event: Event) -> Vec<Effect> {
        let now = self.now();
        feed(&mut self.s, now, event)
    }

    /// Registers a container worker of `capacity`.
    pub fn up(&mut self, name: &str, capacity: Resources) {
        self.up_with(name, capacity, caps("container"));
    }

    pub fn up_with(&mut self, name: &str, capacity: Resources, caps: NodeCaps) {
        self.gauges.insert(name.to_owned(), Gauge::default());
        self.feed(Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity,
            caps,
        });
    }

    /// A request for an action of `cpu_millis` booked cores and `limit` bytes of
    /// memory limit, of a key no other request shares.
    pub fn request(&mut self, kind: LeaseKind, cpu_millis: u64, limit: u64) -> Request {
        let n = self.next;
        self.next += 1;
        Request {
            key: WorkKey::new("load", format!("k{n}")),
            qos: Qos::Ci,
            kind,
            resources: Resources::new(cpu_millis, limit),
            needs: js_caps::Request::default(),
            hermetic: true,
            do_not_cache: false,
        }
    }

    /// Submits `request`; the operation's id is the count of operations before it.
    pub fn submit(&mut self, request: Request) -> OperationId {
        let op = OperationId(self.s.operations() as u64);
        let waiter = WaiterId(op.0);
        self.feed(Event::Submit { waiter, request });
        op
    }

    /// Submits an action with the prediction `hint` fed for it.
    pub fn action(&mut self, cpu_millis: u64, limit: u64, hint: Option<Prediction>) -> OperationId {
        let request = self.request(LeaseKind::Action, cpu_millis, limit);
        let op = self.submit(request);
        if let Some(hint) = hint {
            self.s.set_cost(op, hint);
        }
        op
    }

    /// `count` actions of the same prediction.
    pub fn actions(&mut self, count: u64, hint: Prediction, limit: u64) -> Vec<OperationId> {
        (0..count)
            .map(|_| self.action(hint.cpu_millis, limit, Some(hint)))
            .collect()
    }

    /// One placement round; the leases it started.
    pub fn tick(&mut self) -> Vec<StartLease> {
        self.feed(Event::Tick)
            .into_iter()
            .filter_map(|e| match e {
                Effect::Start(start) => Some(start),
                _ => None,
            })
            .collect()
    }

    /// Moves time on by `ms` and runs a round.
    pub fn round(&mut self, ms: u64) -> Vec<StartLease> {
        self.ms += ms;
        self.tick()
    }

    /// A reading of `name` after one more window: `busy_milli` cores busy over it,
    /// `working_set` bytes, and `covered` leases. The agent's clocks move by one window
    /// whatever the farm time does.
    pub fn reading(
        &mut self,
        name: &str,
        busy_milli: u64,
        working_set: u64,
        covered: Vec<LeaseId>,
    ) -> NodeLoad {
        let g = self.gauges.get_mut(name).expect("registered worker");
        g.seq += 1;
        g.mono_usec += W_MS * 1000;
        g.cpu_usec += busy_milli * W_MS;
        NodeLoad {
            seq: g.seq,
            mono_usec: g.mono_usec,
            cpu_usage_usec: g.cpu_usec,
            working_set_bytes: working_set,
            covered,
            ..NodeLoad::default()
        }
    }

    /// Sends `load` as the heartbeat of `name`, listing every lease the scheduler holds
    /// on it as running.
    pub fn beat(&mut self, name: &str, load: Option<NodeLoad>) -> Vec<Effect> {
        let running = self.s.leases_on(&w(name));
        self.feed(Event::Heartbeat {
            worker: w(name),
            running,
            load,
        })
    }

    /// One window passes on `name`: the farm time moves by `W`, and it sends a reading
    /// of `busy_milli` cores, `working_set` bytes, `covered` leases.
    pub fn window(&mut self, name: &str, busy_milli: u64, working_set: u64, covered: &[LeaseId]) {
        self.ms += W_MS;
        let load = self.reading(name, busy_milli, working_set, covered.to_vec());
        self.beat(name, Some(load));
    }

    /// The worker reports that `start` ended with a result.
    pub fn finish(&mut self, start: &StartLease) {
        let outcome = Outcome::Completed {
            action_result: ResultRef::new([1; 32], start.operation.0),
        };
        self.end(start, outcome);
    }

    /// The worker reports that `start` ended with `outcome`.
    pub fn end(&mut self, start: &StartLease, outcome: Outcome) {
        self.feed(Event::Started {
            operation: start.operation,
            lease: start.lease,
        });
        self.feed(Event::Report {
            operation: start.operation,
            lease: start.lease,
            outcome,
        });
    }

    /// The leases held on `name`.
    pub fn held(&self, name: &str) -> Vec<LeaseId> {
        self.s.leases_on(&w(name))
    }
}

/// A warm prediction.
pub fn warm(cpu_millis: u64, memory_bytes: u64) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes,
        cold: false,
    }
}

/// A cold prediction of `cpu_millis` cores and `memory_bytes`.
pub fn cold(cpu_millis: u64, memory_bytes: u64) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes,
        cold: true,
    }
}

/// The leases of `starts`.
pub fn leases(starts: &[StartLease]) -> Vec<LeaseId> {
    starts.iter().map(|s| s.lease).collect()
}
