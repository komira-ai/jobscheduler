//! Probes (`Request::pin`): a request pinned to one named worker, the only new work a
//! worker cordoned without a drain takes. Through the scheduler's public inputs.

#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;

use crate::digest::{Digest, DigestFunction};

use js_caps::NodeCaps;
use js_sched::{
    Cordon, DaemonInstance, Event, Input, MemoryPercent, OpState, ProbeHost, ProbeRefusal, Request,
    Scheduler, Unservable, admit_probe,
};
use js_types::sched::{
    ControlRecord, Effect, FarmTime, LeaseGrant, LeaseKind, NodePhase, OperationId, Outcome, Qos,
    RefusalRecord, Resources, StateMachine, WaiterId, Waiting, WorkerId,
};

const GIB: u64 = 1 << 30;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// An ordinary request of one core and one GiB, hermetic and cacheable (so joinable).
fn request(n: u8) -> Request {
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
fn probe(n: u8, host: &str) -> Request {
    Request {
        pin: Some(w(host)),
        ..request(n)
    }
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn secs(s: u64) -> FarmTime {
    FarmTime::from_millis(s * 1_000)
}

fn linux() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"])
}

struct Harness {
    s: Scheduler,
    now: FarmTime,
    next_waiter: u64,
}

impl Harness {
    fn new() -> Self {
        Self::with(Scheduler::new(1))
    }

    fn with(s: Scheduler) -> Self {
        Self {
            s,
            now: FarmTime::default(),
            next_waiter: 0,
        }
    }

    fn at(&mut self, s: u64) {
        self.now = secs(s);
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s.apply(Input::new(self.now, event))
    }

    /// Registers `name` with 4 CPUs and 8 GiB.
    fn worker(&mut self, name: &str) {
        self.feed(Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: Resources::new(4_000, 8 * GIB),
            caps: linux(),
        });
    }

    /// Submits `request`; the operation it is queued as, or the twin it joined.
    fn submit(&mut self, request: Request) -> OperationId {
        let waiter = WaiterId(self.next_waiter);
        self.next_waiter += 1;
        let next = self.s.twin(&request).unwrap_or(self.s.next_operation());
        let effects = self.feed(Event::Submit { waiter, request });
        assert!(effects.is_empty(), "{effects:?}");
        next
    }

    /// Ticks; the grants proposed, and every other effect.
    fn tick(&mut self) -> (Vec<LeaseGrant>, Vec<Effect>) {
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
    fn place(&mut self) -> Vec<LeaseGrant> {
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
    fn finish(&mut self, grant: &LeaseGrant) {
        let outcome = Outcome::Completed {
            action_result: digest(200).into(),
        };
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

    fn cordon(&mut self, name: &str) {
        assert!(self.feed(Event::Cordon { worker: w(name) }).is_empty());
    }

    fn state(&self, op: OperationId) -> &OpState {
        self.s.state(op).expect("the operation exists")
    }

    /// Ticks once and expects exactly the refusal of `op`, proposed in that round with
    /// the probe reason `why`, its waiters told the reason first. Commits it; the effects.
    fn refused_at_once(&mut self, op: OperationId, why: &ProbeRefusal) {
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

/// Catches: a cordon that keeps a probe off the worker it names, and a probe that opens
/// the cordoned worker to ordinary work submitted beside it (the ordinary job of the same
/// shape, ahead of the probe in the queue, must keep waiting for the cordon).
#[test]
fn a_probe_lands_on_its_cordoned_worker_and_an_ordinary_job_of_its_shape_does_not() {
    let mut h = Harness::new();
    h.worker("a");
    h.cordon("a");
    let ordinary = h.submit(request(1));
    let probing = h.submit(probe(2, "a"));
    let (grants, effects) = h.tick();
    let [grant] = grants.as_slice() else {
        panic!("one grant expected: {grants:?}");
    };
    assert_eq!((grant.operation, &grant.worker), (probing, &w("a")));
    assert_eq!(
        effects,
        [Effect::Waiting(Waiting {
            operation: ordinary,
            reason: Some("every live worker that can run it is cordoned: a".to_owned()),
        })]
    );
    assert_eq!(h.state(ordinary), &OpState::Queued);
    assert_eq!(h.s.cordon(&w("a")), Some(&Cordon::Cordoned));
}

/// Catches: a probe placed on another worker when its own is full (it must wait for its
/// own), a probe that oversubscribes a full worker, a probe that never gets its worker
/// once a lease there ends, and a probe whose run changes or lifts the cordon.
#[test]
fn a_probe_waits_for_room_on_its_own_full_worker_and_leaves_the_cordon_as_it_was() {
    let mut h = Harness::new();
    h.worker("a");
    // Four one-core leases fill `a`'s 4 cores: a fifth is past its CPU slack.
    for n in 1..=4 {
        h.submit(request(n));
    }
    let filling = h.place();
    assert_eq!(filling.len(), 4);
    assert!(filling.iter().all(|g| g.worker == w("a")), "{filling:?}");
    h.worker("b");
    h.cordon("a");
    let probing = h.submit(probe(9, "a"));
    for _ in 0..3 {
        let (grants, effects) = h.tick();
        assert!(
            grants.is_empty(),
            "never on b, never past a's room: {grants:?}"
        );
        assert!(effects.is_empty(), "it waits with no reason: {effects:?}");
    }
    assert_eq!(h.state(probing), &OpState::Queued);
    assert_eq!(h.s.unservable(probing), None);
    assert_eq!(h.s.leases_on(&w("b")), []);

    h.finish(&filling[0]);
    let [grant] = h
        .place()
        .try_into()
        .expect("the probe takes the freed core");
    assert_eq!((grant.operation, &grant.worker), (probing, &w("a")));
    assert_eq!(h.s.leases_on(&w("a")).len(), 4);
    h.finish(&grant);
    assert!(matches!(h.state(probing), OpState::Completed { .. }));
    assert_eq!(h.s.cordon(&w("a")), Some(&Cordon::Cordoned));
    // The cordon still keeps ordinary work off `a`.
    h.submit(request(10));
    let [after] = h.place().try_into().expect("one grant");
    assert_eq!(after.worker, w("b"));
}

/// Catches: a probe placed on an open worker (it would prove nothing a cordon keeps
/// apart), refused only after the unservable wait instead of at once, or with a reason
/// that is not the probe's.
#[test]
fn a_probe_for_an_open_worker_is_refused_at_once() {
    let mut h = Harness::new();
    h.worker("a");
    let op = h.submit(probe(1, "a"));
    h.refused_at_once(op, &ProbeRefusal::NotCordoned { host: w("a") });
    // Ordinary work still places there.
    h.submit(request(2));
    assert_eq!(h.place().len(), 1);
}

/// Catches: a probe that waits for a worker that never registered (or that went silent)
/// instead of being refused, and one placed on another worker for want of its own.
#[test]
fn a_probe_for_an_unknown_or_silent_worker_is_refused_at_once() {
    let mut h = Harness::new();
    h.worker("a");
    h.cordon("a");
    let op = h.submit(probe(1, "nowhere"));
    h.refused_at_once(op, &ProbeRefusal::UnknownHost { host: w("nowhere") });
    assert_eq!(h.s.leases_on(&w("a")), []);

    // `a` is cordoned but silent past the lease grace: no longer a live worker.
    h.at(100);
    let op = h.submit(probe(2, "a"));
    h.refused_at_once(op, &ProbeRefusal::UnknownHost { host: w("a") });
}

/// Catches: a probe admitted to a worker that is being drained, emptied for its update
/// (new work there would hold the drain up), or to one whose drain is done or paused.
#[test]
fn a_probe_for_a_draining_drained_or_paused_worker_is_refused_at_once() {
    let mut h = Harness::new();
    h.worker("a");
    h.worker("b");
    h.submit(request(1));
    let [held] = h.place().try_into().expect("a takes it");
    assert_eq!(held.worker, w("a"));
    let deadline = secs(10);
    h.feed(Event::Drain {
        worker: w("a"),
        deadline,
    });
    let op = h.submit(probe(2, "a"));
    let draining = Cordon::Draining { deadline };
    h.refused_at_once(
        op,
        &ProbeRefusal::Draining {
            host: w("a"),
            cordon: draining,
        },
    );

    // Past the deadline with the lease still running, the drain pauses.
    h.at(11);
    h.feed(Event::Heartbeat {
        worker: w("a"),
        running: vec![held.lease],
        load: None,
    });
    h.tick();
    assert_eq!(h.s.cordon(&w("a")), Some(&Cordon::Paused { deadline }));
    let op = h.submit(probe(3, "a"));
    h.refused_at_once(
        op,
        &ProbeRefusal::Draining {
            host: w("a"),
            cordon: Cordon::Paused { deadline },
        },
    );

    // `b` holds nothing, so its drain is done at once.
    h.feed(Event::Drain {
        worker: w("b"),
        deadline: secs(50),
    });
    h.tick();
    assert_eq!(h.s.cordon(&w("b")), Some(&Cordon::Drained));
    let op = h.submit(probe(4, "b"));
    h.refused_at_once(
        op,
        &ProbeRefusal::Draining {
            host: w("b"),
            cordon: Cordon::Drained,
        },
    );
}

/// Catches: a probe admitted to a cordoned worker that is not serving (quarantined, or
/// switching its session): the cordon is let through, the phase is not.
#[test]
fn a_probe_for_a_cordoned_worker_that_is_not_serving_is_refused_at_once() {
    let mut h = Harness::new();
    h.worker("a");
    h.cordon("a");
    let phase = NodePhase::Quarantined("disk check failed".to_owned());
    h.feed(Event::Phase {
        worker: w("a"),
        phase: phase.clone(),
    });
    let op = h.submit(probe(1, "a"));
    h.refused_at_once(
        op,
        &ProbeRefusal::NotServing {
            host: w("a"),
            phase,
        },
    );
}

/// Catches: a probe larger than its worker left waiting for ever (no room will come), and
/// a probe whose size is checked against the worker's raw memory instead of what
/// memory_schedulable_percent leaves of it.
#[test]
fn a_probe_larger_than_its_worker_after_the_memory_cut_is_refused_at_once() {
    // Half of `a`'s 8 GiB is schedulable: a 6 GiB probe is too large for it.
    let percent = MemoryPercent::new(50).expect("in range");
    let mut h = Harness::with(Scheduler::new(1).with_memory_percent(percent));
    h.worker("a");
    h.cordon("a");
    let mut big = probe(1, "a");
    big.resources = Resources::new(1_000, 6 * GIB);
    let op = h.submit(big);
    let capacity = h.s.capacity(&w("a")).expect("registered");
    assert_eq!(capacity.memory_bytes, 4 * GIB);
    h.refused_at_once(
        op,
        &ProbeRefusal::TooLarge {
            host: w("a"),
            asked: Resources::new(1_000, 6 * GIB),
        },
    );

    // Within the cut it places.
    let mut fits = probe(2, "a");
    fits.resources = Resources::new(1_000, 4 * GIB);
    let op = h.submit(fits);
    let [grant] = h.place().try_into().expect("it fits");
    assert_eq!(grant.operation, op);
}

/// Catches: a probe placed on a worker that cannot run its platform or its lease kind.
#[test]
fn a_probe_its_worker_cannot_run_is_refused_at_once() {
    let mut h = Harness::new();
    h.worker("a");
    h.cordon("a");
    let mut mac = probe(1, "a");
    mac.needs = js_caps::Request::parse([("os", "macos")]).unwrap();
    let op = h.submit(mac);
    h.refused_at_once(
        op,
        &ProbeRefusal::PlatformMismatch {
            host: w("a"),
            lacks: vec!["os=macos".to_owned()],
        },
    );

    let mut vm = probe(2, "a");
    vm.kind = LeaseKind::Vm;
    let op = h.submit(vm);
    h.refused_at_once(
        op,
        &ProbeRefusal::NoDriver {
            host: w("a"),
            kind: LeaseKind::Vm,
        },
    );
}

/// Catches: a probe answered by a twin running elsewhere (its run would not prove its
/// worker), and an ordinary request joined to a probe (it would run on the cordoned
/// worker).
#[test]
fn a_probe_is_never_joined_to_a_twin_nor_a_twin_to_it() {
    let mut h = Harness::new();
    h.worker("a");
    h.worker("b");
    h.cordon("a");
    let ordinary = h.submit(request(1));
    let probing = h.submit(probe(1, "a"));
    let twin = h.submit(request(1));
    assert_eq!(twin, ordinary, "two ordinary requests still join");
    assert_ne!(probing, ordinary);
    assert!(!probe(1, "a").joinable());
    assert_eq!(h.s.twin(&probe(1, "a")), None);
    let mut grants = h.place();
    grants.sort_by_key(|g| g.operation);
    let placed: Vec<(OperationId, WorkerId)> = grants
        .into_iter()
        .map(|g| (g.operation, g.worker))
        .collect();
    assert_eq!(placed, [(ordinary, w("b")), (probing, w("a"))]);
}

/// Catches: a probe placed past the lease numbers the round has left, and a probe the
/// round had no number for that is refused or dropped instead of placed the next round.
#[test]
fn a_probe_with_no_lease_number_left_waits_for_the_next_round() {
    let mut h = Harness::new();
    h.s.use_seq_block(1..1);
    h.worker("a");
    h.cordon("a");
    let op = h.submit(probe(1, "a"));
    let (grants, effects) = h.tick();
    assert!(
        grants.is_empty() && effects.is_empty(),
        "{grants:?} {effects:?}"
    );
    assert_eq!(h.state(op), &OpState::Queued);
    h.s.use_seq_block(1..10);
    let [grant] = h.place().try_into().expect("placed once numbers are there");
    assert_eq!((grant.operation, &grant.worker), (op, &w("a")));
}

/// The rule's order: the first failing check is the refusal, and an admitted probe is
/// `Ok` whatever is booked (room is not part of the rule).
/// Catches: a check skipped or reordered, so that a worker gets the wrong reason.
#[test]
fn admit_probe_checks_in_order_and_admits_a_cordoned_serving_worker_that_fits() {
    let caps = linux();
    let capacity = Resources::new(4_000, 8 * GIB);
    let serving = NodePhase::Serving;
    let cordoned = Cordon::Cordoned;
    let host = |cordon, phase| ProbeHost {
        cordon,
        phase,
        capacity: &capacity,
        caps: &caps,
    };
    let a = w("a");
    let small = probe(1, "a");
    assert_eq!(
        admit_probe(&a, Some(host(Some(&cordoned), &serving)), &small),
        Ok(())
    );
    // Not serving and open: the phase is named first.
    let restoring = NodePhase::MemoryPressure;
    assert_eq!(
        admit_probe(&a, Some(host(None, &restoring)), &small),
        Err(ProbeRefusal::NotServing {
            host: a.clone(),
            phase: NodePhase::MemoryPressure,
        })
    );
    // Open and too large: not cordoned is named first.
    let mut huge = probe(2, "a");
    huge.resources = Resources::new(64_000, GIB);
    assert_eq!(
        admit_probe(&a, Some(host(None, &serving)), &huge),
        Err(ProbeRefusal::NotCordoned { host: a.clone() })
    );
    assert_eq!(
        admit_probe(&a, Some(host(Some(&cordoned), &serving)), &huge),
        Err(ProbeRefusal::TooLarge {
            host: a.clone(),
            asked: huge.resources,
        })
    );
    // A whole-worker probe books what the worker has: any size fits.
    huge.whole_worker = true;
    assert_eq!(
        admit_probe(&a, Some(host(Some(&cordoned), &serving)), &huge),
        Ok(())
    );
    assert_eq!(
        admit_probe(&a, None, &small),
        Err(ProbeRefusal::UnknownHost { host: a })
    );
}

/// Catches: a refusal that names the wrong worker, a code that is not the one the wire
/// answers, and a sentence that loses what the operator needs (the phase, the drain
/// state, what is lacking, what was asked and what the worker has).
#[test]
fn every_probe_refusal_names_its_worker_with_its_code_and_sentence() {
    let a = || w("a");
    let deadline = secs(10);
    let cases = [
        (
            ProbeRefusal::UnknownHost { host: a() },
            "probe-host-unknown",
            "probe host a is not a live worker",
        ),
        (
            ProbeRefusal::NotServing {
                host: a(),
                phase: NodePhase::Draining,
            },
            "probe-host-not-serving",
            "probe host a is not serving (draining)",
        ),
        (
            ProbeRefusal::NotCordoned { host: a() },
            "probe-host-not-cordoned",
            "probe host a is not cordoned: a probe runs only on a worker kept from ordinary \
             work",
        ),
        (
            ProbeRefusal::Draining {
                host: a(),
                cordon: Cordon::Draining { deadline },
            },
            "probe-host-draining",
            "probe host a is draining: a probe runs only on a worker cordoned without a drain",
        ),
        (
            ProbeRefusal::Draining {
                host: a(),
                cordon: Cordon::Paused { deadline },
            },
            "probe-host-draining",
            "probe host a is draining (paused): a probe runs only on a worker cordoned \
             without a drain",
        ),
        (
            ProbeRefusal::Draining {
                host: a(),
                cordon: Cordon::Drained,
            },
            "probe-host-draining",
            "probe host a is drained: a probe runs only on a worker cordoned without a drain",
        ),
        (
            ProbeRefusal::Draining {
                host: a(),
                cordon: Cordon::Cordoned,
            },
            "probe-host-draining",
            "probe host a is draining: a probe runs only on a worker cordoned without a drain",
        ),
        (
            ProbeRefusal::NoDriver {
                host: a(),
                kind: LeaseKind::Vm,
            },
            "probe-host-mismatch",
            "probe host a serves no lease of kind vm: that needs a driver among vm",
        ),
        (
            ProbeRefusal::PlatformMismatch {
                host: a(),
                lacks: vec!["os=macos".to_owned(), "arch=arm64".to_owned()],
            },
            "probe-host-mismatch",
            "probe host a does not satisfy the probe's platform: it lacks os=macos, arch=arm64",
        ),
        (
            ProbeRefusal::TooLarge {
                host: a(),
                asked: Resources {
                    disk_bytes: 7,
                    ..Resources::new(8_000, 16)
                },
            },
            "probe-host-too-small",
            "probe host a is smaller than the probe's request (8000 millicores, 16 bytes of \
             memory, 0 GPU(s), 0 VM slot(s), 7 bytes of disk)",
        ),
    ];
    for (refusal, code, sentence) in cases {
        assert_eq!(refusal.host(), &a());
        assert_eq!(refusal.code(), code);
        assert_eq!(refusal.to_string(), sentence);
        assert_eq!(Unservable::Probe(refusal).to_string(), sentence);
    }
}
