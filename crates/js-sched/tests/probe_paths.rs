//! Probes (`Request::pin`) on the paths a request takes after it is admitted: the room and
//! load boundaries of its worker, the kinds and shapes it may ask for, memory floors and
//! the memory ladder, reruns and a store outage, its worker changing while it waits, and
//! twins. Through the scheduler's public inputs.

#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "probe_rig/mod.rs"]
mod probe_rig;

use crate::probe_rig::{GIB, Harness, linux, probe, request, secs, w};

use js_caps::NodeCaps;
use js_sched::fence::LEASE_GRACE;
use js_sched::{Cordon, Event, Floor, KeyFloor, OpState, ProbeRefusal, Request, Unservable};
use js_types::sched::{
    ControlRecord, Effect, Failure, FarmTime, LeaseGrant, LeaseKind, LostWhy, NodePhase, Outcome,
    Resources,
};

/// Four leases of one core fill a 4-core worker `name`; the grants.
fn fill(h: &mut Harness, name: &str, from: u8) -> Vec<LeaseGrant> {
    for n in from..from + 4 {
        h.submit(request(n));
    }
    let grants = h.place();
    assert_eq!(grants.len(), 4);
    assert!(grants.iter().all(|g| g.worker == w(name)), "{grants:?}");
    grants
}

/// Whether `effects` propose the refusal of `op` for `why`, and nothing is granted.
fn refuses(effects: &[Effect], op: js_types::sched::OperationId, why: &ProbeRefusal) -> bool {
    let text = why.to_string();
    let refused = effects.iter().any(|e| {
        matches!(e, Effect::Commit(ControlRecord::Refusal(r)) if r.operation == op && r.reason == text)
    });
    let granted = effects
        .iter()
        .any(|e| matches!(e, Effect::Commit(ControlRecord::Lease(_))));
    refused && !granted
}

/// Catches: a probe placed past its worker's CPU slack, and one kept short of it: on a
/// 10-core worker with 10 one-core leases the slack (1.1x) leaves exactly one core, so
/// the first of two one-core probes takes it and the second waits.
#[test]
fn a_probe_is_held_to_its_workers_cpu_slack_exactly() {
    let mut h = Harness::new();
    h.worker_of("a", Resources::new(10_000, 32 * GIB), linux());
    for n in 1..=10 {
        h.submit(request(n));
    }
    assert_eq!(h.place().len(), 10);
    h.cordon("a");
    let within = h.submit(probe(20, "a"));
    let over = h.submit(probe(21, "a"));
    let grants = h.place();
    let placed: Vec<_> = grants
        .iter()
        .map(|g| (g.operation, g.worker.clone()))
        .collect();
    assert_eq!(placed, [(within, w("a"))]);
    assert_eq!(h.state(over), &OpState::Queued);
}

/// Catches: a probe placed past its worker's cap on leases while CPU and memory are free,
/// and one that does not get the slot once the lease there ends.
#[test]
fn a_probe_is_held_to_its_workers_lease_cap() {
    let mut h = Harness::new();
    let capped = Resources {
        leases: Some(1),
        ..Resources::new(4_000, 8 * GIB)
    };
    h.worker_of("a", capped, linux());
    h.submit(request(1));
    let [held] = h.place().try_into().expect("a takes it");
    h.cordon("a");
    let op = h.submit(probe(2, "a"));
    assert!(h.place().is_empty(), "the one slot is taken");
    assert_eq!(h.state(op), &OpState::Queued);
    h.finish(&held);
    let [grant] = h.place().try_into().expect("the slot is free");
    assert_eq!((grant.operation, &grant.worker), (op, &w("a")));
}

/// Catches: two probes for the last room on their worker both placed in one round (the
/// worker oversubscribed), and the one left out refused instead of placed once room comes.
#[test]
fn two_probes_for_the_last_core_of_their_worker_take_it_one_after_the_other() {
    let mut h = Harness::new();
    h.worker("a");
    h.worker("b");
    for n in 1..=3 {
        h.submit(request(n));
    }
    // Spread over two workers: put three on `a` alone by cordoning `b` first.
    h.cordon("b");
    let three = h.place();
    assert!(three.iter().all(|g| g.worker == w("a")), "{three:?}");
    h.cordon("a");
    let first = h.submit(probe(10, "a"));
    let second = h.submit(probe(11, "a"));
    let grants = h.place();
    let placed: Vec<_> = grants
        .iter()
        .map(|g| (g.operation, g.worker.clone()))
        .collect();
    // One core is left within the slack (4.4 cores), so only one of the two takes it.
    assert_eq!(placed, [(first, w("a"))]);
    assert_eq!(h.state(second), &OpState::Queued);
    h.finish(&grants[0]);
    let [grant] = h
        .place()
        .try_into()
        .expect("the second takes the freed core");
    assert_eq!((grant.operation, &grant.worker), (second, &w("a")));
    assert_eq!(h.s.leases_on(&w("b")), []);
}

/// Catches: a probe that asks for a whole worker placed beside a lease, or one whose
/// booking is not all of the worker, and ordinary work placed beside it after.
#[test]
fn a_whole_worker_probe_waits_for_its_worker_to_empty_and_books_all_of_it() {
    let mut h = Harness::new();
    h.worker("a");
    h.worker("b");
    h.cordon("b");
    h.submit(request(1));
    let [held] = h.place().try_into().expect("a takes it");
    h.feed(Event::Uncordon { worker: w("b") });
    h.cordon("a");
    let mut whole = probe(2, "a");
    whole.whole_worker = true;
    let op = h.submit(whole);
    assert!(h.place().is_empty(), "a holds a lease");
    h.finish(&held);
    let [grant] = h.place().try_into().expect("a is empty");
    assert_eq!((grant.operation, &grant.worker), (op, &w("a")));
    assert_eq!(h.s.booked(&w("a")), Some(Resources::new(4_000, 8 * GIB)));
    h.submit(request(3));
    let [after] = h.place().try_into().expect("b takes ordinary work");
    assert_eq!(after.worker, w("b"));
}

/// Catches: a GPU probe placed on a worker whose one GPU is held, and one that does not
/// get the GPU once it is free.
#[test]
fn a_gpu_probe_waits_for_the_gpu_of_its_worker() {
    let mut h = Harness::new();
    let gpu = Resources::new(4_000, 8 * GIB).with_gpus(1);
    h.worker_of("a", gpu, linux());
    let mut first = request(1);
    first.resources = Resources::new(1_000, GIB).with_gpus(1);
    h.submit(first);
    let [held] = h.place().try_into().expect("a takes the GPU lease");
    h.cordon("a");
    let mut wants = probe(2, "a");
    wants.resources = Resources::new(1_000, GIB).with_gpus(1);
    let op = h.submit(wants);
    assert!(h.place().is_empty(), "the GPU is held");
    h.finish(&held);
    let [grant] = h.place().try_into().expect("the GPU is free");
    assert_eq!(grant.operation, op);
}

/// Catches: a VM probe refused on a worker that serves VMs, or placed past its VM slots.
#[test]
fn a_vm_probe_runs_on_a_worker_that_serves_vms_and_waits_for_its_slot() {
    let mut h = Harness::new();
    let caps = NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
        .unwrap()
        .with_drivers(["vm"]);
    let one_slot = Resources {
        vms: 1,
        ..Resources::new(8_000, 16 * GIB)
    };
    h.worker_of("mac", one_slot, caps);
    h.cordon("mac");
    let vm = |n| Request {
        kind: LeaseKind::Vm,
        resources: Resources {
            vms: 1,
            ..Resources::new(2_000, 4 * GIB)
        },
        ..probe(n, "mac")
    };
    let first = h.submit(vm(1));
    let second = h.submit(vm(2));
    let grants = h.place();
    let placed: Vec<_> = grants.iter().map(|g| g.operation).collect();
    assert_eq!(placed, [first]);
    assert_eq!(h.state(second), &OpState::Queued);
    h.finish(&grants[0]);
    let [grant] = h.place().try_into().expect("the slot is free");
    assert_eq!(grant.operation, second);
}

/// Catches: a probe raised to a memory floor learned on a larger worker, which would make
/// it too large for its own (refused) though what it asks fits there.
#[test]
fn a_probe_takes_no_memory_floor_of_its_key() {
    let mut h = Harness::new();
    h.worker_of("big", Resources::new(4_000, 32 * GIB), linux());
    h.worker("a");
    h.cordon("a");
    h.s.restore_floors(vec![Floor {
        key: request(1).key,
        ask: 16 * GIB,
        stamp: 1,
    }]);
    let ordinary = h.submit(request(1));
    let probing = h.submit(probe(1, "a"));
    let mut grants = h.place();
    grants.sort_by_key(|g| g.operation);
    let placed: Vec<_> = grants
        .iter()
        .map(|g| (g.operation, g.worker.clone()))
        .collect();
    assert_eq!(placed, [(ordinary, w("big")), (probing, w("a"))]);
    let memory = |op| h.s.request(op).unwrap().resources.memory_bytes;
    assert_eq!((memory(ordinary), memory(probing)), (16 * GIB, GIB));
}

/// Catches: a probe raised to a size key's floor bounded by a larger worker, which would
/// make it own its worker whole and wait for ever where what it asks fits now.
#[test]
fn a_probe_takes_no_key_floor() {
    const KEY: u64 = 7;
    let mut h = Harness::new();
    h.worker_of("big", Resources::new(4_000, 32 * GIB), linux());
    h.worker("a");
    h.cordon("a");
    h.s.restore_key_floors(vec![KeyFloor {
        key: KEY,
        ask: 8 * GIB,
        stamp: 1,
    }]);
    let op = h.s.next_operation();
    h.s.set_size_key(op, KEY);
    h.submit(probe(1, "a"));
    let [grant] = h.place().try_into().expect("placed at what it asks");
    assert_eq!((grant.operation, &grant.worker), (op, &w("a")));
    assert_eq!(h.s.request(op).unwrap().resources.memory_bytes, GIB);
}

/// Catches: a probe killed at its own memory limit raised past its worker by a larger
/// worker it may not run on (then refused as too large for its own), instead of climbing
/// to its worker's whole memory at most and running there again.
#[test]
fn a_probe_killed_for_memory_climbs_only_to_its_workers_memory() {
    let mut h = Harness::new();
    h.worker_of("big", Resources::new(4_000, 64 * GIB), linux());
    h.worker("a");
    h.cordon("a");
    let op = h.submit(probe(1, "a"));
    let oom = Outcome::Failed(Failure::OutOfMemory);
    let mut booked = Vec::new();
    loop {
        let grants = h.place();
        let [grant] = grants.as_slice() else {
            panic!("one grant expected: {grants:?}");
        };
        assert_eq!(grant.worker, w("a"));
        booked.push(h.s.request(op).unwrap().resources.memory_bytes / GIB);
        h.report(grant, oom);
        if matches!(h.state(op), OpState::Failed { .. }) {
            break;
        }
    }
    // 1 GiB asked, then the ladder's 4 GiB, then `a`'s whole 8 GiB; killed there it ends.
    assert_eq!(booked, [1, 4, 8]);
}

/// Catches: a probe whose rerun after a lost run is placed on another worker (a rerun
/// prefers a worker the lost runs did not end on), and a probe placed while the state
/// store is down.
#[test]
fn a_probe_reruns_on_its_worker_and_is_not_placed_while_the_store_is_down() {
    let mut h = Harness::new();
    h.worker("a");
    h.worker("b");
    h.cordon("a");
    let op = h.submit(probe(1, "a"));
    let [grant] = h.place().try_into().expect("placed");
    h.report(
        &grant,
        Outcome::Failed(Failure::Lost(LostWhy::DaemonRestarted)),
    );
    assert_eq!(h.state(op), &OpState::Queued);
    h.s.store_unavailable();
    assert!(
        h.place().is_empty(),
        "nothing is placed while the store is down"
    );
    h.s.store_available(h.now);
    let [rerun] = h.place().try_into().expect("placed again");
    assert_eq!((rerun.operation, &rerun.worker), (op, &w("a")));
    assert_eq!(h.s.leases_on(&w("b")), []);
}

/// Catches: a probe that keeps waiting for room on a worker that has since been opened,
/// drained, taken out of service, gone silent or made smaller than it: each is judged
/// again every round and refused by name, never placed elsewhere.
#[test]
fn a_waiting_probe_is_refused_when_its_worker_changes() {
    // Opened: the uncordon's own round refuses it.
    let mut h = Harness::new();
    h.worker("a");
    h.worker("b");
    h.cordon("b");
    fill(&mut h, "a", 1);
    h.cordon("a");
    let op = h.submit(probe(9, "a"));
    assert!(h.place().is_empty());
    let effects = h.feed(Event::Uncordon { worker: w("a") });
    assert!(
        refuses(&effects, op, &ProbeRefusal::NotCordoned { host: w("a") }),
        "{effects:?}"
    );

    // Drained, not serving, made smaller: the next round refuses it.
    let deadline = secs(500);
    let changes: [(Event, ProbeRefusal); 3] = [
        (
            Event::Drain {
                worker: w("a"),
                deadline,
            },
            ProbeRefusal::Draining {
                host: w("a"),
                cordon: Cordon::Draining { deadline },
            },
        ),
        (
            Event::Phase {
                worker: w("a"),
                phase: NodePhase::MemoryPressure,
            },
            ProbeRefusal::NotServing {
                host: w("a"),
                phase: NodePhase::MemoryPressure,
            },
        ),
        (
            Event::Capacity {
                worker: w("a"),
                capacity: Resources::new(500, GIB),
                caps: linux(),
            },
            ProbeRefusal::TooLarge {
                host: w("a"),
                asked: Resources::new(1_000, GIB),
            },
        ),
    ];
    for (n, (change, why)) in (10..).zip(changes) {
        let mut h = Harness::new();
        h.worker("a");
        h.worker("b");
        h.cordon("b");
        fill(&mut h, "a", 1);
        h.cordon("a");
        let op = h.submit(probe(n, "a"));
        assert!(h.place().is_empty());
        h.feed(change);
        let (grants, effects) = h.tick();
        assert!(grants.is_empty(), "{grants:?}");
        assert!(refuses(&effects, op, &why), "{why:?}: {effects:?}");
        assert_eq!(h.s.unservable(op), Some(&Unservable::Probe(why)));
    }
}

/// Catches: a probe refused while its worker is still within the lease grace, or kept
/// waiting once the grace has passed: the worker is live until the grace ends, exactly.
#[test]
fn a_waiting_probe_is_refused_exactly_when_its_worker_goes_silent() {
    let mut h = Harness::new();
    h.worker("a");
    fill(&mut h, "a", 1);
    h.cordon("a");
    let op = h.submit(probe(9, "a"));
    let grace = u64::try_from(LEASE_GRACE.as_millis()).unwrap();
    h.now = FarmTime::from_millis(grace - 1);
    let (grants, effects) = h.tick();
    assert!(grants.is_empty());
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Commit(ControlRecord::Refusal(_)))),
        "still live: {effects:?}"
    );
    h.now = FarmTime::from_millis(grace);
    let (_, effects) = h.tick();
    let why = ProbeRefusal::UnknownHost { host: w("a") };
    assert!(refuses(&effects, op, &why), "{effects:?}");
}

/// Catches: an ordinary request joined to a probe submitted before it (it would run on the
/// cordoned worker), and two probes of one action for two workers made one operation (one
/// worker left unproved).
#[test]
fn a_probe_first_and_probes_for_two_workers_are_operations_of_their_own() {
    let mut h = Harness::new();
    h.worker("a");
    h.worker("b");
    h.worker("c");
    h.cordon("a");
    h.cordon("b");
    let on_a = h.submit(probe(1, "a"));
    let ordinary = h.submit(request(1));
    let on_b = h.submit(probe(1, "b"));
    assert_eq!([on_a.0, ordinary.0, on_b.0], [0, 1, 2]);
    let mut grants = h.place();
    grants.sort_by_key(|g| g.operation);
    let placed: Vec<_> = grants
        .iter()
        .map(|g| (g.operation, g.worker.clone()))
        .collect();
    assert_eq!(placed, [(on_a, w("a")), (ordinary, w("c")), (on_b, w("b"))]);
    h.finish(&grants[0]);
    assert!(matches!(h.state(on_a), OpState::Completed { .. }));
    assert!(matches!(h.state(on_b), OpState::Running { .. }));
}
