//! Admission by load where it meets placement: ranking by what a worker is charged, the
//! shadow and off settings, the hold a blocked head of the queue keeps, the workers a
//! whole-machine lease or a drain is emptying, and the cost a lease keeps across a
//! restart.

#[path = "load_rig/mod.rs"]
mod load_rig;

use std::sync::Arc;

use js_caps::NodeCaps;
use js_sched::{
    DaemonInstance, Event, Input, LoadConfig, PlacementLoad, Recovered, RecoveredLease, Scheduler,
};
use js_types::sched::usage::{Curve, CurveSource, LeaseCost, MinutePoint};
use js_types::sched::{
    ControlRecord, Effect, FarmTime, LeaseId, LeaseKind, OperationId, Resources, StartLease,
    StateMachine, WaiterId, WorkKey,
};

use load_rig::{GIB, Rig, caps, leases, w, warm};

fn node() -> Resources {
    Resources::new(8_000, 32 * GIB)
}

/// The worker each start went to, in order.
fn on(starts: &[StartLease]) -> Vec<&str> {
    starts.iter().map(|s| s.worker.as_str()).collect()
}

/// Places `count` actions of `cpu_millis` on `worker` alone, by cordoning `others` for the
/// round: the score packs onto the fitter worker, so a setup that wants a worker loaded
/// says which.
fn place_on(r: &mut Rig, worker: &str, others: &[&str], count: u64, cpu_millis: u64) {
    for other in others {
        r.feed(Event::Cordon { worker: w(other) });
    }
    for _ in 0..count {
        r.action(cpu_millis, 4 * GIB, Some(warm(cpu_millis, 0)));
    }
    let starts = r.tick();
    assert_eq!(on(&starts), vec![worker; count as usize]);
    for other in others {
        r.feed(Event::Uncordon { worker: w(other) });
    }
}

fn config(mode: PlacementLoad) -> LoadConfig {
    LoadConfig {
        mode,
        ..LoadConfig::default()
    }
}

/// Worker `b` holds four leases booked at a core each and measured at 1000 m; worker `a`
/// holds one booked at a quarter core and measured at 6000 m (its lease is not the only
/// thing on it). Returns where one more action goes under `mode`.
fn where_one_more_goes(mode: PlacementLoad) -> String {
    let mut r = Rig::new(config(mode));
    r.up("b", node());
    let b_held = leases(&{
        for _ in 0..4 {
            r.action(1_000, 8 * GIB, Some(warm(250, 0)));
        }
        r.tick()
    });
    r.up("a", node());
    r.action(250, 8 * GIB, Some(warm(250, 0)));
    let a_held = leases(&r.tick());
    assert_eq!((a_held.len(), b_held.len()), (1, 4));
    for held in [("a", 6_000, &a_held), ("b", 1_000, &b_held)] {
        r.window(held.0, 0, 0, held.2);
        r.window(held.0, held.1, 0, held.2);
    }
    r.action(250, 8 * GIB, Some(warm(250, 0)));
    r.tick()[0].worker.as_str().to_owned()
}

/// Catches: ranking by what is booked when the load is read (the worker holding one
/// quarter-core lease that is measured at 6 cores would be chosen over one holding
/// four booked cores that is measured at one), and the load rank leaking into `Off`.
#[test]
fn workers_are_ranked_by_what_they_are_charged() {
    assert_eq!(where_one_more_goes(PlacementLoad::On), "b");
    assert_eq!(where_one_more_goes(PlacementLoad::Off), "a");
    assert_eq!(where_one_more_goes(PlacementLoad::Shadow), "a");
}

/// What nine actions, each booking a quarter core and predicted at 4 GiB, do on one
/// 32 GiB worker: where they were started, and the counters.
fn nine_big(mode: PlacementLoad) -> (Vec<(OperationId, LeaseId, String)>, js_sched::LoadCounters) {
    let mut r = Rig::new(config(mode));
    r.up("a", node());
    for _ in 0..9 {
        r.action(250, 8 * GIB, Some(warm(250, 4 * GIB)));
    }
    let starts = r.tick();
    let placed = starts
        .iter()
        .map(|s| (s.operation, s.lease, s.worker.as_str().to_owned()))
        .collect();
    (placed, r.s.load_counters())
}

/// Catches: `shadow` changing a placement (it places as `off` does), `off` reading the
/// load, and shadow's verdicts not being the ones `on` gives in its first round: of nine
/// leases of 4 GiB on 32 GiB, `off` and `shadow` start all nine, `on` starts eight and
/// blocks the ninth, and shadow counts eight that would be admitted and one blocked.
#[test]
fn shadow_places_as_off_does_and_counts_what_on_would_do() {
    let (off, off_counts) = nine_big(PlacementLoad::Off);
    let (shadow, shadow_counts) = nine_big(PlacementLoad::Shadow);
    let (on, on_counts) = nine_big(PlacementLoad::On);
    assert_eq!(off.len(), 9);
    assert_eq!(shadow, off, "the same starts, leases and workers");
    assert_eq!(on.len(), 8);
    assert_eq!(on, off[..8]);

    assert_eq!(off_counts, js_sched::LoadCounters::default());
    assert_eq!(
        (shadow_counts.would_admit, shadow_counts.would_block),
        (8, 1)
    );
    assert_eq!(shadow_counts.admitted, 0);
    assert_eq!(shadow_counts.blocked, 0);
    assert_eq!((on_counts.admitted, on_counts.blocked), (8, 1));
    assert_eq!((on_counts.would_admit, on_counts.would_block), (0, 0));
}

/// Catches: a scheduler built with `new` not placing as before (the load predicate is
/// opted into with `with_load_config`): the eight one-core leases the booked rule allows
/// on eight cores, whatever the predictions say.
#[test]
fn a_scheduler_built_with_new_places_by_the_booked_rule() {
    let mut s = Scheduler::new(1);
    let caps = caps("container");
    s.apply(Input::new(
        FarmTime::from_millis(0),
        Event::WorkerUp {
            worker: w("a"),
            instance: DaemonInstance::new("a"),
            predecessor_stopped: false,
            capacity: node(),
            caps,
        },
    ));
    for n in 0..12 {
        let request = js_sched::Request {
            key: WorkKey::new("load", format!("{n}")),
            qos: js_types::sched::Qos::Ci,
            kind: LeaseKind::Action,
            resources: Resources::new(1_000, GIB),
            needs: js_caps::Request::default(),
            hermetic: true,
            do_not_cache: false,
        };
        s.apply(Input::new(
            FarmTime::from_millis(0),
            Event::Submit {
                waiter: WaiterId(n),
                request,
            },
        ));
        s.set_cost(OperationId(n), warm(250, 0));
    }
    let placed = js_sched::harness::feed(&mut s, FarmTime::from_millis(1), Event::Tick)
        .into_iter()
        .filter(|e| matches!(e, Effect::Start(_)))
        .count();
    assert_eq!(placed, 8);
    assert_eq!(s.load_counters(), js_sched::LoadCounters::default());
}

/// Two 8-core workers `a` and `b`, each holding four leases predicted at a core (charged
/// by prediction: no readings), and a head of 6 cores queued ahead of a stream of small
/// actions.
struct Blocked {
    r: Rig,
    held_a: Vec<LeaseId>,
    held_b: Vec<LeaseId>,
}

impl Blocked {
    fn new() -> Self {
        let mut r = Rig::on();
        r.up("a", node());
        r.up("b", node());
        place_on(&mut r, "a", &["b"], 4, 1_000);
        place_on(&mut r, "b", &["a"], 4, 1_000);
        let held_a = r.held("a");
        let held_b = r.held("b");
        Self { r, held_a, held_b }
    }

    fn head(&mut self) -> OperationId {
        self.r.action(6_000, 4 * GIB, Some(warm(6_000, 0)))
    }

    fn smalls(&mut self, count: u64) -> Vec<StartLease> {
        for _ in 0..count {
            self.r.action(500, 4 * GIB, Some(warm(500, 0)));
        }
        self.r.round(1_000)
    }

    /// Ends every lease `held` names. Operations were submitted in order and leases
    /// granted in order, so a lease's sequence is its operation's id.
    fn end(&mut self, held: &[LeaseId]) {
        for &lease in held {
            let operation = OperationId(lease.seq);
            self.r.end(
                &StartLease {
                    worker: w("a"),
                    lease,
                    operation,
                    key: WorkKey::new("x", "y"),
                    kind: LeaseKind::Action,
                    resources: Resources::default(),
                    fence: js_types::sched::FencePolicy::RunOn,
                    device: None,
                },
                js_types::sched::Outcome::Completed {
                    action_result: js_types::sched::ResultRef::new([1; 32], 1),
                },
            );
        }
    }
}

/// Catches: a stream of small actions refilling the node a large one waits for, for good
/// (the head would start only when the stream paused): the blocked head holds the worker
/// with the most headroom (the first by name on a tie), small work after it goes to the
/// other worker, and the head starts on the held worker as soon as its leases end.
#[test]
fn a_blocked_head_holds_a_worker_and_starts_when_it_empties() {
    let mut f = Blocked::new();
    let head = f.head();
    let starts = f.smalls(4);
    assert_eq!(
        on(&starts),
        ["b", "b", "b", "b"],
        "nothing is placed on the held worker"
    );
    assert_eq!(f.r.s.head_hold(head), Some(&w("a")));
    assert_eq!(f.r.s.load_counters().reservations, 1);
    // The next round keeps the hold and counts nothing new.
    let starts = f.smalls(2);
    assert_eq!(on(&starts), ["b", "b"]);
    assert_eq!(f.r.s.head_hold(head), Some(&w("a")));
    assert_eq!(f.r.s.load_counters().reservations, 1);
    // Its leases end: the head starts there, and the hold is gone.
    let held_a = f.held_a.clone();
    f.end(&held_a);
    let starts = f.r.round(1_000);
    assert_eq!(on(&starts), ["a"]);
    assert_eq!(starts[0].operation, head);
    assert_eq!(f.r.s.head_hold(head), None);
    assert!(f.held_b.len() == 4);
}

/// Catches: work ahead of the head held back (the hold is for work after it in queue
/// order only): a small action queued before the head is placed on the worker the head
/// then holds.
#[test]
fn work_ahead_of_the_head_is_never_held_back() {
    let mut f = Blocked::new();
    f.smalls(0);
    f.r.action(500, 4 * GIB, Some(warm(500, 0)));
    let head = f.head();
    let starts = f.r.round(1_000);
    assert_eq!(
        on(&starts),
        ["a"],
        "ahead of the head, on the worker it then holds"
    );
    assert_eq!(f.r.s.head_hold(head), Some(&w("b")));
}

/// Catches: one hold per blocked request instead of per pool (a second large request
/// would take the other worker too and nothing small could run), and a second head never
/// becoming the head: the second waits without a hold, and holds a worker once the first
/// has started.
#[test]
fn one_hold_per_pool_and_the_second_head_waits_its_turn() {
    let mut f = Blocked::new();
    let first = f.head();
    let second = f.head();
    let starts = f.smalls(1);
    assert_eq!(on(&starts), ["b"]);
    assert_eq!(f.r.s.head_hold(first), Some(&w("a")));
    assert_eq!(f.r.s.head_hold(second), None);
    assert_eq!(f.r.s.load_counters().reservations, 1);
    let held_a = f.held_a.clone();
    f.end(&held_a);
    let starts = f.r.round(1_000);
    assert_eq!(starts[0].operation, first);
    assert_eq!(f.r.s.head_hold(second), Some(&w("b")), "now it is the head");
    assert_eq!(f.r.s.load_counters().reservations, 2);
}

/// Catches: a head blocked in one pool holding a worker of another, or being held up by
/// another pool's hold: a request for a Mac blocked on the Mac holds the Mac, a request
/// for Linux blocked on Linux holds a Linux worker, and the two do not meet.
#[test]
fn a_blocked_head_does_not_delay_another_pool() {
    let mut f = Blocked::new();
    let mac_caps = NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
        .unwrap()
        .with_drivers(["container"]);
    f.r.up_with("m", node(), mac_caps);
    let mac = js_caps::Request::from_platform([("OSFamily", "macos")]).unwrap();
    for _ in 0..4 {
        let mut q = f.r.request(LeaseKind::Action, 1_000, 4 * GIB);
        q.needs = mac.clone();
        let op = f.r.submit(q);
        f.r.s.set_cost(op, warm(1_000, 0));
    }
    assert_eq!(on(&f.r.round(1_000)), ["m", "m", "m", "m"]);
    let linux_head = f.head();
    let mut q = f.r.request(LeaseKind::Action, 6_000, 4 * GIB);
    q.needs = mac;
    let mac_head = f.r.submit(q);
    f.r.s.set_cost(mac_head, warm(6_000, 0));
    f.r.round(1_000);
    assert_eq!(f.r.s.head_hold(linux_head), Some(&w("a")));
    assert_eq!(f.r.s.head_hold(mac_head), Some(&w("m")));
    assert_eq!(f.r.s.load_counters().reservations, 2);
}

/// Catches: a hold that wanders (moving at every wobble of the loads) or one that never
/// moves: both workers measured at 4000 m, the head holds `a`; `b` falling to 3500 m
/// (within a tenth of the head's ratio) leaves the hold, and falling to 3000 m moves it
/// to `b`.
#[test]
fn the_hold_moves_only_when_the_kept_worker_is_a_tenth_worse() {
    let mut f = Blocked::new();
    let (held_a, held_b) = (f.held_a.clone(), f.held_b.clone());
    for (name, held) in [("a", &held_a), ("b", &held_b)] {
        f.r.window(name, 4_000, 0, held);
        f.r.window(name, 4_000, 0, held);
    }
    let head = f.head();
    f.r.round(0);
    assert_eq!(f.r.s.head_hold(head), Some(&w("a")));
    for _ in 0..2 {
        f.r.window("b", 3_500, 0, &held_b);
    }
    f.r.round(0);
    assert_eq!(f.r.s.head_hold(head), Some(&w("a")), "0.0625 worse is kept");
    for _ in 0..2 {
        f.r.window("b", 3_000, 0, &held_b);
    }
    f.r.round(0);
    assert_eq!(f.r.s.head_hold(head), Some(&w("b")), "0.125 worse moves");
    assert_eq!(f.r.s.load_counters().reservations, 2);
}

/// Catches: a head blocked only by the cold cap holding a worker (the cap lifts at the next
/// window, and the hold would keep small work off the worker for nothing).
#[test]
fn a_head_blocked_by_the_cold_cap_holds_nothing() {
    let mut r = Rig::on();
    r.up("a", node());
    for _ in 0..3 {
        r.action(1_000, 4 * GIB, Some(load_rig::cold(1_000, 0)));
    }
    let starts = r.tick();
    assert_eq!(starts.len(), 1, "one cold lease per window on 8 cores");
    let small = r.action(500, 4 * GIB, Some(warm(500, 0)));
    r.tick();
    assert_eq!(r.s.head_hold(small), None);
    assert_eq!(r.s.load_counters().reservations, 0);
}

/// Catches: a worker reserved for a whole-machine lease taking small work because its
/// charged load is the lowest (it is the one being emptied): the whole-machine head
/// reserves `a`, which holds one light lease while `b` holds four, and a stream of small
/// actions goes to `b` only, until `a` empties and the whole-machine lease starts there.
#[test]
fn a_worker_reserved_for_a_whole_machine_lease_takes_no_small_work() {
    let mut r = Rig::on();
    let whole = caps("container").with_drivers(["container", "native-whole-machine"]);
    r.up_with("b", node(), whole.clone());
    for _ in 0..4 {
        r.action(1_000, 4 * GIB, Some(warm(250, 0)));
    }
    r.tick();
    r.up_with("a", node(), whole);
    r.feed(Event::Cordon { worker: w("b") });
    r.action(250, 4 * GIB, Some(warm(250, 0)));
    let light = r.tick();
    assert_eq!(on(&light), ["a"]);
    r.feed(Event::Uncordon { worker: w("b") });
    let mut q = r.request(LeaseKind::WholeMachine, 8_000, 32 * GIB);
    q.resources = node();
    let head = r.submit(q);
    r.round(1_000);
    assert_eq!(r.s.reservation(head), Some(&w("a")));
    for _ in 0..6 {
        r.action(250, 4 * GIB, Some(warm(250, 0)));
    }
    let starts = r.round(1_000);
    assert_eq!(
        on(&starts),
        ["b"; 6],
        "the emptier worker is the reserved one"
    );
    r.finish(&light[0]);
    let starts = r.round(1_000);
    assert_eq!(starts[0].operation, head);
}

/// Catches: a draining worker taking small work because its charged load falls as its
/// leases end: placement skips it before the predicate, so the drain completes.
#[test]
fn a_draining_worker_takes_no_small_work() {
    let mut r = Rig::on();
    r.up("a", node());
    r.up("b", node());
    r.action(250, 4 * GIB, Some(warm(250, 0)));
    let first = r.tick();
    assert_eq!(on(&first), ["a"]);
    r.feed(Event::Drain {
        worker: w("a"),
        deadline: FarmTime::from_millis(3_600_000),
    });
    for _ in 0..5 {
        r.action(250, 4 * GIB, Some(warm(250, 0)));
    }
    assert_eq!(on(&r.round(1_000)), ["b"; 5]);
}

/// The key of the action `grant` is for: `placed_with_grants` names them by operation.
fn grant_key(
    _grants: &[js_types::sched::LeaseGrant],
    grant: &js_types::sched::LeaseGrant,
) -> WorkKey {
    WorkKey::new("load", format!("{}", grant.operation.0))
}

/// A scheduler with `n` actions placed on worker `a` and the grants it committed.
fn placed_with_grants(n: u64) -> (Scheduler, Vec<js_types::sched::LeaseGrant>) {
    let mut s = Scheduler::new(1).with_load_config(LoadConfig::default());
    let t = FarmTime::from_millis(0);
    s.apply(Input::new(
        t,
        Event::WorkerUp {
            worker: w("a"),
            instance: DaemonInstance::new("a"),
            predecessor_stopped: false,
            capacity: node(),
            caps: caps("container"),
        },
    ));
    for i in 0..n {
        let request = js_sched::Request {
            key: WorkKey::new("load", format!("{i}")),
            qos: js_types::sched::Qos::Ci,
            kind: LeaseKind::Action,
            resources: Resources::new(1_000, 8 * GIB),
            needs: js_caps::Request::default(),
            hermetic: true,
            do_not_cache: false,
        };
        s.apply(Input::new(
            t,
            Event::Submit {
                waiter: WaiterId(i),
                request,
            },
        ));
        s.set_cost(OperationId(i), warm(500, 2 * GIB));
    }
    let grants = s
        .apply(Input::new(t, Event::Tick))
        .into_iter()
        .filter_map(|e| match e {
            Effect::Commit(ControlRecord::Lease(grant)) => Some(grant),
            _ => None,
        })
        .collect();
    (s, grants)
}

/// Catches: a grant record that does not carry the cost the lease was charged (a
/// scheduler rebuilt from the log could not know it), and a cost that is not the
/// prediction, the request's limit and the cold flag.
#[test]
fn the_grant_record_carries_the_cost() {
    let (s, grants) = placed_with_grants(3);
    assert_eq!(grants.len(), 3);
    for grant in &grants {
        let want = LeaseCost {
            cpu_millis: 500,
            memory_bytes: 2 * GIB,
            limit_bytes: 8 * GIB,
            cold: false,
            key_digest: js_sched::key_digest(&grant_key(&grants, grant)),
        };
        assert_ne!(want.key_digest, 0);
        assert_eq!(grant.cost, want);
        assert_eq!(s.lease_cost(grant.lease), Some(want));
    }
}

/// Rebuilds a scheduler from `grants` as the state store would hold them, whose
/// estimator now predicts something else, and registers worker `a`.
fn restarted(s: &Scheduler, grants: &[js_types::sched::LeaseGrant], keep_cost: bool) -> Scheduler {
    let mut b = Scheduler::new(2).with_load_config(LoadConfig::default());
    let operations = grants
        .iter()
        .map(|g| Recovered {
            operation: g.operation,
            request: s.request(g.operation).expect("held").clone(),
            waiter: WaiterId(g.operation.0),
            lease: Some(RecoveredLease {
                lease: g.lease,
                worker: g.worker.clone(),
                cost: keep_cost.then_some(g.cost),
            }),
            losses: 0,
            fate: None,
        })
        .collect();
    b.recover(2, OperationId(grants.len() as u64), 0, operations);
    for g in grants {
        // The estimator has moved on.
        b.set_cost(g.operation, warm(9_999, 99 * GIB));
    }
    b.apply(Input::new(
        FarmTime::from_millis(1_000),
        Event::WorkerUp {
            worker: w("a"),
            instance: DaemonInstance::new("a"),
            predecessor_stopped: false,
            capacity: node(),
            caps: caps("container"),
        },
    ));
    b
}

/// Catches: costs recomputed after a restart from the current estimator (the sums before
/// and after would differ, and a node could be admitted past its bound across the
/// restart), and a recovered lease not counted: the rebuilt scheduler holds the same
/// costs and sums to the byte, and admits exactly as many more leases.
#[test]
fn a_restart_keeps_the_admitted_costs_and_sums() {
    let (mut a, grants) = placed_with_grants(5);
    let mut b = restarted(&a, &grants, true);
    for g in &grants {
        assert_eq!(b.lease_cost(g.lease), a.lease_cost(g.lease));
    }
    let (before, after) = (a.load_view(&w("a")).unwrap(), b.load_view(&w("a")).unwrap());
    assert_eq!(before, after);
    assert_eq!(after.predicted_cpu_millis, 2_500);
    assert_eq!(after.predicted_memory_bytes, 10 * GIB);
    assert!(after.fallback);
    // The same room: 32 GiB of 2 GiB leases is sixteen, eleven more beside five.
    let more = |s: &mut Scheduler, seq: u64| {
        let t = FarmTime::from_millis(2_000);
        for i in 0..20 {
            let request = js_sched::Request {
                key: WorkKey::new("more", format!("{i}")),
                qos: js_types::sched::Qos::Ci,
                kind: LeaseKind::Action,
                resources: Resources::new(1_000, 8 * GIB),
                needs: js_caps::Request::default(),
                hermetic: true,
                do_not_cache: false,
            };
            s.apply(Input::new(
                t,
                Event::Submit {
                    waiter: WaiterId(100 + i),
                    request,
                },
            ));
            s.set_cost(OperationId(seq + i), warm(500, 2 * GIB));
        }
        js_sched::harness::feed(s, t, Event::Tick)
            .into_iter()
            .filter(|e| matches!(e, Effect::Start(_)))
            .count()
    };
    assert_eq!(more(&mut a, 5), 11);
    assert_eq!(more(&mut b, 5), 11);
}

/// Catches: a row written before costs were kept charged nothing (it would not count) or
/// by the current estimator: it is charged the cores its request books and no memory.
#[test]
fn a_recovered_lease_with_no_cost_is_charged_its_request() {
    let (a, grants) = placed_with_grants(2);
    let b = restarted(&a, &grants, false);
    let view = b.load_view(&w("a")).unwrap();
    assert_eq!(view.predicted_cpu_millis, 2_000);
    assert_eq!(view.predicted_memory_bytes, 0);
    assert_eq!(
        b.lease_cost(grants[0].lease),
        Some(LeaseCost {
            cpu_millis: 1_000,
            memory_bytes: 0,
            limit_bytes: 8 * GIB,
            cold: false,
            key_digest: js_sched::key_digest(&WorkKey::new("load", "0")),
        })
    );
}

/// Catches: a lease recovered onto a worker that had registered already not being charged
/// (the worker took the grant before the scheduler learned of the lease): booking
/// happens at once, and the sums include it.
#[test]
fn a_lease_recovered_onto_a_registered_worker_is_charged() {
    let (a, grants) = placed_with_grants(2);
    let mut b = Scheduler::new(2).with_load_config(LoadConfig::default());
    b.apply(Input::new(
        FarmTime::from_millis(0),
        Event::WorkerUp {
            worker: w("a"),
            instance: DaemonInstance::new("a"),
            predecessor_stopped: false,
            capacity: node(),
            caps: caps("container"),
        },
    ));
    let operations = grants
        .iter()
        .map(|g| Recovered {
            operation: g.operation,
            request: a.request(g.operation).unwrap().clone(),
            waiter: WaiterId(0),
            lease: Some(RecoveredLease {
                lease: g.lease,
                worker: g.worker.clone(),
                cost: Some(g.cost),
            }),
            losses: 0,
            fate: None,
        })
        .collect();
    b.recover(2, OperationId(2), 0, operations);
    assert_eq!(b.load_view(&w("a")).unwrap().predicted_cpu_millis, 1_000);
}

/// A curve source that knows a curve for every key.
#[derive(Debug)]
struct EveryKey;

impl CurveSource for EveryKey {
    fn curve(&self, _key: &WorkKey) -> Option<Curve> {
        Some(Curve::new(vec![MinutePoint::default()], 5).unwrap())
    }
}

/// Catches: a curve changing what the flat rule charges (the seam is for the rule that
/// reads it), and the keys that have one going uncounted: three leases are charged
/// exactly as without a source and counted.
#[test]
fn the_flat_rule_charges_a_key_with_a_curve_flat_and_counts_it() {
    let mut s = Scheduler::new(1)
        .with_load_config(LoadConfig::default())
        .with_curves(Arc::new(EveryKey));
    assert!(format!("{s:?}").contains("Curves"));
    let plain = placed_with_grants(3).0.load_view(&w("a")).unwrap();
    let t = FarmTime::from_millis(0);
    s.apply(Input::new(
        t,
        Event::WorkerUp {
            worker: w("a"),
            instance: DaemonInstance::new("a"),
            predecessor_stopped: false,
            capacity: node(),
            caps: caps("container"),
        },
    ));
    for i in 0..3 {
        let request = js_sched::Request {
            key: WorkKey::new("load", format!("{i}")),
            qos: js_types::sched::Qos::Ci,
            kind: LeaseKind::Action,
            resources: Resources::new(1_000, 8 * GIB),
            needs: js_caps::Request::default(),
            hermetic: true,
            do_not_cache: false,
        };
        s.apply(Input::new(
            t,
            Event::Submit {
                waiter: WaiterId(i),
                request,
            },
        ));
        s.set_cost(OperationId(i), warm(500, 2 * GIB));
    }
    js_sched::harness::feed(&mut s, t, Event::Tick);
    assert_eq!(s.load_view(&w("a")).unwrap(), plain);
    assert_eq!(s.load_counters().curve_flat, 3);
}

/// Catches: an action at the whole host's memory holding a worker: it runs alone, on a worker
/// with no lease, and nothing is emptying for it, so it holds none. Here the worker has no
/// lease and the measurement (something else uses its cores) keeps the action out.
#[test]
fn an_action_at_the_whole_hosts_memory_holds_no_worker() {
    let mut r = Rig::on();
    r.up("a", node());
    r.window("a", 8_000, 0, &[]);
    r.window("a", 8_000, 0, &[]);
    let big = r.action(8_000, 32 * GIB, Some(warm(8_000, 0)));
    assert!(r.tick().is_empty());
    assert_eq!(r.s.head_hold(big), None);
    assert_eq!(r.s.load_counters().blocked, 1);
}

/// Catches: a head holding the worker a whole-machine lease is emptying (the worker would be
/// refilled for the head as soon as the lease ends, or the two holds would fight over it):
/// the whole-machine lease holds `a` (the first with the fewest leases), and the large
/// action behind it holds `b`.
#[test]
fn a_head_does_not_hold_the_worker_a_whole_machine_lease_holds() {
    let mut r = Rig::on();
    let whole = caps("container").with_drivers(["container", "native-whole-machine"]);
    r.up_with("a", node(), whole.clone());
    r.up_with("b", node(), whole);
    place_on(&mut r, "a", &["b"], 4, 1_000);
    place_on(&mut r, "b", &["a"], 4, 1_000);
    let mut q = r.request(LeaseKind::WholeMachine, 8_000, 32 * GIB);
    q.resources = node();
    let whole_op = r.submit(q);
    let head = r.action(6_000, 4 * GIB, Some(warm(6_000, 0)));
    r.round(1_000);
    assert_eq!(r.s.reservation(whole_op), Some(&w("a")));
    assert_eq!(r.s.head_hold(head), Some(&w("b")));
}

/// Catches: a head holding a worker its own earlier runs ended on as the farm's fault (it
/// would not be placed there, so the hold would keep small work off a worker for nobody):
/// the operation was killed on `a` by memory pressure, and holds `b`.
#[test]
fn a_head_does_not_hold_a_worker_it_avoids() {
    let mut r = Rig::on();
    r.up("a", node());
    let head = r.action(6_000, 4 * GIB, Some(warm(6_000, 0)));
    let first = r.round(1_000);
    assert_eq!(on(&first), ["a"]);
    r.up("b", node());
    for _ in 0..8 {
        r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    }
    let smalls = r.round(1_000);
    assert_eq!(smalls.len(), 8);
    assert!(smalls.iter().filter(|s| s.worker == w("b")).count() >= 5);
    r.end(
        &first[0],
        js_types::sched::Outcome::Failed(js_types::sched::Failure::NodeMemoryPressure),
    );
    let starts = r.round(1_000);
    assert!(starts.iter().all(|s| s.operation != head), "it waits for b");
    assert_eq!(
        r.s.head_hold(head),
        Some(&w("b")),
        "not the worker it avoids"
    );
}
