//! What the whole-node tests share: a scheduler driven by farm time, with workers,
//! actions and whole-machine requests. Every time is a value fed to the scheduler.

use std::time::Duration;

use crate::digest::{Digest, DigestFunction};
use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Event, Input, Request, Scheduler};
use js_types::sched::{
    ControlRecord, Effect, FarmTime, LeaseGrant, LeaseId, LeaseKind, OperationId, Outcome, Qos,
    Resources, StateMachine, WaiterId, WorkerId,
};

pub const GIB: u64 = 1 << 30;
pub const CAPACITY: Resources = Resources::new(8_000, 16 * GIB);

pub fn request(n: u8, kind: LeaseKind) -> Request {
    Request {
        key: crate::digest::work_key(
            "main",
            Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n)),
        ),
        qos: Qos::Ci,
        kind,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: js_caps::Request::default(),
    }
}

pub fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

pub fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

pub fn caps() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container", "native-whole-machine"])
}

pub fn up(s: &mut Scheduler, at_ms: u64, name: &str) {
    let event = Event::WorkerUp {
        worker: w(name),
        instance: DaemonInstance::new(name),
        predecessor_stopped: false,
        capacity: CAPACITY,
        caps: caps(),
    };
    s.apply(Input::new(FarmTime::from_millis(at_ms), event));
}

/// A scheduler and the farm time its next input carries.
pub struct Farm {
    pub s: Scheduler,
    pub now: u64,
    pub next_n: u8,
    /// The workers registered, for [`Farm::beat`].
    pub workers: Vec<WorkerId>,
    /// The leases started and not finished, with their workers.
    pub running: Vec<(WorkerId, LeaseId)>,
}

impl Farm {
    pub fn new(s: Scheduler) -> Self {
        Self {
            s,
            now: 0,
            next_n: 0,
            workers: Vec::new(),
            running: Vec::new(),
        }
    }

    pub fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s
            .apply(Input::new(FarmTime::from_millis(self.now), event))
    }

    /// Registers `name`, at the farm time now.
    pub fn worker(&mut self, name: &str) {
        up(&mut self.s, self.now, name);
        self.workers.push(w(name));
    }

    /// Moves the farm time to `ms`.
    pub fn at(&mut self, ms: u64) {
        self.now = ms;
    }

    /// Every worker reports the leases it runs, at the farm time now.
    pub fn beat(&mut self) {
        for worker in self.workers.clone() {
            let running = self
                .running
                .iter()
                .filter(|(w, _)| *w == worker)
                .map(|(_, lease)| *lease)
                .collect();
            self.feed(Event::Heartbeat {
                worker,
                running,
                load: None,
            });
        }
    }

    /// Moves the farm time to `ms` in steps of ten seconds, the workers reporting at each,
    /// so none is taken for silent; the placement rounds' grants.
    pub fn advance(&mut self, ms: u64) -> Vec<LeaseGrant> {
        let mut grants = Vec::new();
        while self.now + 10_000 < ms {
            self.now += 10_000;
            self.beat();
            grants.extend(self.grants());
        }
        self.now = ms;
        self.beat();
        grants.extend(self.grants());
        grants
    }

    /// Submits an action that may run `wall`, places it and starts it now; the grant.
    pub fn lease(&mut self, wall: Duration) -> LeaseGrant {
        self.lease_after(wall, 0)
    }

    /// As [`Farm::lease`], the worker reporting the run started `delay_ms` after the grant.
    pub fn lease_after(&mut self, wall: Duration, delay_ms: u64) -> LeaseGrant {
        self.next_n += 1;
        let operation = self.s.next_operation();
        self.s.set_max_wall(operation, wall);
        self.submit(request(self.next_n, LeaseKind::Action));
        let grant = self.grants().pop().expect("the action is placed");
        assert_eq!(grant.operation, operation);
        let granted = self.now;
        self.now += delay_ms;
        self.start(&grant);
        self.now = granted;
        grant
    }

    /// Submits a whole-machine request that fits nowhere and ticks: the operation.
    pub fn whole(&mut self) -> OperationId {
        self.next_n += 1;
        let operation = self.s.next_operation();
        self.submit(request(self.next_n, LeaseKind::WholeMachine));
        assert!(self.grants().is_empty(), "no worker is empty");
        operation
    }

    /// Submits a whole-machine request that may run `wall` (none: no wall time given)
    /// without ticking: the operation.
    pub fn submit_whole(&mut self, wall: Option<Duration>) -> OperationId {
        self.next_n += 1;
        let operation = self.s.next_operation();
        if let Some(wall) = wall {
            self.s.set_max_wall(operation, wall);
        }
        self.submit(request(self.next_n, LeaseKind::WholeMachine));
        operation
    }

    pub fn submit(&mut self, request: Request) {
        let waiter = WaiterId(u64::from(self.next_n));
        self.feed(Event::Submit { waiter, request });
    }

    pub fn grants(&mut self) -> Vec<LeaseGrant> {
        self.feed(Event::Tick)
            .into_iter()
            .filter_map(|e| match e {
                Effect::Commit(ControlRecord::Lease(g)) => Some(g),
                _ => None,
            })
            .collect()
    }

    pub fn start(&mut self, grant: &LeaseGrant) {
        self.running.push((grant.worker.clone(), grant.lease));
        self.feed(Event::Committed(ControlRecord::Lease(grant.clone())));
        self.feed(Event::Started {
            operation: grant.operation,
            lease: grant.lease,
        });
    }

    pub fn finish(&mut self, grant: &LeaseGrant) {
        self.running.retain(|(_, lease)| *lease != grant.lease);
        let proposed = self.feed(Event::Report {
            operation: grant.operation,
            lease: grant.lease,
            outcome: Outcome::Completed {
                action_result: Digest::new(DigestFunction::Sha256, [200; 32], 200).into(),
            },
        });
        let [Effect::Commit(record)] = proposed.as_slice() else {
            panic!("{proposed:?}");
        };
        self.feed(Event::Committed(record.clone()));
    }

    pub fn held_for(&self, operation: OperationId) -> Option<&WorkerId> {
        self.s.reservation(operation)
    }
}
