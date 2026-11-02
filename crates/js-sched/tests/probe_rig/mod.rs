//! The rig of the probe tests (`Request::pin`): requests, a probe of the same shape, and
//! a harness that drives one scheduler through its public inputs.

use crate::digest::{Digest, DigestFunction};

use js_caps::NodeCaps;
use js_sched::{
    DaemonInstance, Event, Input, OpState, ProbeRefusal, Request, Scheduler, Unservable,
};
use js_types::sched::{
    ControlRecord, Effect, FarmTime, LeaseGrant, LeaseKind, OperationId, Outcome, Qos,
    RefusalRecord, Resources, StateMachine, WaiterId, Waiting, WorkerId,
};

pub const GIB: u64 = 1 << 30;

pub fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// An ordinary request of one core and one GiB, hermetic and cacheable (so joinable).
pub fn request(n: u8) -> Request {
    Request {
        key: crate::digest::work_key("main", digest(n)),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        whole_worker: false,
        pin: None,
        needs: js_caps::Request::default(),
    }
}

/// [`request`] `n`, pinned to `host`: the same shape, a probe.
pub fn probe(n: u8, host: &str) -> Request {
    Request {
        pin: Some(w(host)),
        ..request(n)
    }
}

pub fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

pub fn secs(s: u64) -> FarmTime {
    FarmTime::from_millis(s * 1_000)
}

pub fn linux() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"])
}

pub struct Harness {
    pub s: Scheduler,
    pub now: FarmTime,
    next_waiter: u64,
}

impl Harness {
    pub fn new() -> Self {
        Self::with(Scheduler::new(1))
    }

    pub fn with(s: Scheduler) -> Self {
        Self {
            s,
            now: FarmTime::default(),
            next_waiter: 0,
        }
    }

    pub fn at(&mut self, s: u64) {
        self.now = secs(s);
    }

    pub fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s.apply(Input::new(self.now, event))
    }

    /// Registers `name` with 4 CPUs and 8 GiB.
    pub fn worker(&mut self, name: &str) {
        self.worker_of(name, Resources::new(4_000, 8 * GIB), linux());
    }

    /// Registers (or registers again) `name` with `capacity` and `caps`.
    pub fn worker_of(&mut self, name: &str, capacity: Resources, caps: NodeCaps) {
        self.feed(Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity,
            caps,
        });
    }

    /// Heartbeats `name` with the leases it holds and no load reading.
    pub fn heartbeat(&mut self, name: &str) {
        let running = self.s.leases_on(&w(name));
        self.feed(Event::Heartbeat {
            worker: w(name),
            running,
            load: None,
        });
    }

    /// Submits `request`; the operation it is queued as, or the twin it joined.
    pub fn submit(&mut self, request: Request) -> OperationId {
        let waiter = WaiterId(self.next_waiter);
        self.next_waiter += 1;
        let next = self.s.twin(&request).unwrap_or(self.s.next_operation());
        let effects = self.feed(Event::Submit { waiter, request });
        assert!(effects.is_empty(), "{effects:?}");
        next
    }

    /// Ticks; the grants proposed, and every other effect.
    pub fn tick(&mut self) -> (Vec<LeaseGrant>, Vec<Effect>) {
        let mut grants = Vec::new();
        let mut other = Vec::new();
        for e in self.feed(Event::Tick) {
            match e {
                Effect::Commit(ControlRecord::Lease(g)) => grants.push(g),
                e => other.push(e),
            }
        }
        (grants, other)
    }

    /// Ticks, commits every grant and reports each started; the grants.
    pub fn place(&mut self) -> Vec<LeaseGrant> {
        let (grants, _) = self.tick();
        for g in &grants {
            self.feed(Event::Committed(ControlRecord::Lease(g.clone())));
            self.feed(Event::Started {
                operation: g.operation,
                lease: g.lease,
            });
        }
        grants
    }

    /// Reports `grant` done and commits the result.
    pub fn finish(&mut self, grant: &LeaseGrant) {
        let outcome = Outcome::Completed {
            action_result: digest(200).into(),
        };
        self.report(grant, outcome);
    }

    /// Reports `grant`'s run ended with `outcome` and commits what that proposes.
    pub fn report(&mut self, grant: &LeaseGrant, outcome: Outcome) {
        let proposed = self.feed(Event::Report {
            operation: grant.operation,
            lease: grant.lease,
            outcome,
        });
        for record in proposed.into_iter().filter_map(|e| match e {
            Effect::Commit(record) => Some(record),
            _ => None,
        }) {
            self.feed(Event::Committed(record));
        }
    }

    pub fn cordon(&mut self, name: &str) {
        assert!(self.feed(Event::Cordon { worker: w(name) }).is_empty());
    }

    pub fn state(&self, op: OperationId) -> &OpState {
        self.s.state(op).expect("the operation exists")
    }

    /// Ticks once and expects exactly the refusal of `op`, proposed in that round with
    /// the probe reason `why`, its waiters told the reason first. Commits it; the effects.
    pub fn refused_at_once(&mut self, op: OperationId, why: &ProbeRefusal) {
        let (grants, effects) = self.tick();
        assert!(grants.is_empty(), "{grants:?}");
        let text = why.to_string();
        let expected = [
            Effect::Waiting(Waiting {
                operation: op,
                reason: Some(text.clone()),
            }),
            Effect::Commit(ControlRecord::Refusal(RefusalRecord {
                operation: op,
                reason: text,
            })),
        ];
        assert_eq!(effects, expected);
        assert_eq!(self.s.unservable(op), Some(&Unservable::Probe(why.clone())));
        let [_, Effect::Commit(record)] = expected else {
            unreachable!("built above");
        };
        let answered = self.feed(Event::Committed(record));
        assert!(
            matches!(answered.as_slice(), [Effect::Refuse(r)] if r.operation == op),
            "{answered:?}"
        );
        assert!(matches!(self.state(op), OpState::Refused { .. }));
    }
}
