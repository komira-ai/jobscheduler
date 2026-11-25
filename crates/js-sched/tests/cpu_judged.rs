//! Leaving the CPU of an operation on some workers to the caller (`Scheduler::set_cpu_judged`):
//! a placer that packs CPU curves slot by slot makes the CPU check itself, so the scheduler
//! makes none of its own there, while memory and every other check stay. Logical time only.

#[path = "profile_rig/mod.rs"]
mod profile_rig;

use std::collections::BTreeMap;

use js_sched::{HostRefusal, LoadConfig, PlacementLoad, Refusal, RefusalKind, Refusals};
use js_types::sched::{OperationId, Resources, WorkerId};

use profile_rig::load_rig::{GIB, Rig, w, warm};
use profile_rig::{busy_at, holding_busy_at};

/// An 8-core worker with 32 GiB.
fn node() -> Resources {
    Resources::new(8_000, 32 * GIB)
}

/// A host with cores to spare and 32 GiB.
fn wide() -> Resources {
    Resources::new(100_000, 32 * GIB)
}

/// The workers `figures` names, each with its figure, as the hook takes them.
fn judged(figures: &[(&str, u64)]) -> BTreeMap<WorkerId, u64> {
    figures.iter().map(|(name, cpu)| (w(name), *cpu)).collect()
}

/// The one refusal in `refusals`, nothing dropped.
fn only(refusals: Refusals) -> HostRefusal {
    assert_eq!(refusals.dropped, 0);
    assert_eq!(refusals.kept.len(), 1, "{:?}", refusals.kept);
    refusals.kept.into_iter().next().expect("one")
}

/// A refusal of `kind` with no measurement.
fn refusal(kind: RefusalKind, counted: u64, new: u64, limit: u64) -> Refusal {
    Refusal {
        kind,
        counted,
        new,
        limit,
        measured_cpu_millis: None,
    }
}

/// Fills `worker` with `count` actions of `cpu` millicores, the other workers cordoned for
/// the rounds so the placement is forced there.
fn fill(r: &mut Rig, worker: &str, others: &[&str], count: u64, cpu: u64) {
    for other in others {
        r.feed(js_sched::Event::Cordon { worker: w(other) });
    }
    for _ in 0..count {
        r.action(cpu, GIB, Some(warm(cpu, 0)));
    }
    let placed: Vec<_> = r.tick();
    assert_eq!(placed.len() as u64, count);
    assert!(placed.iter().all(|s| s.worker.as_str() == worker));
    for other in others {
        r.feed(js_sched::Event::Uncordon { worker: w(other) });
    }
}

/// A host `a` whose four leases of two cores fill it to its CPU limit (8 800 m), and the
/// operation of a fifth, queued.
fn full_of_cpu() -> (Rig, OperationId) {
    let mut r = Rig::on();
    r.up("a", node());
    fill(&mut r, "a", &[], 4, 2_000);
    let op = r.action(2_000, GIB, Some(warm(2_000, 0)));
    (r, op)
}

/// Behaviour: a worker whose CPU is full refuses an action lease by CPU (8 000 m counted,
/// 2 000 m new, limit 8 800 m, exactly); the same operation, once the hook names the worker,
/// is placed there on the next round and no refusal is kept.
///
/// Catches: a hook that names a worker and is not read by the load predicate's flat CPU
/// inequality, and a refusal that carries on after the worker is named.
#[test]
fn a_worker_full_of_cpu_takes_the_lease_once_the_caller_judges_it() {
    let (mut r, op) = full_of_cpu();
    assert!(r.tick().is_empty());
    assert_eq!(
        only(r.s.take_refusals()).refusal,
        refusal(RefusalKind::Cpu, 8_000, 2_000, 8_800)
    );
    r.s.set_cpu_judged(op, judged(&[("a", 10_000)]));
    let started = r.tick();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].worker, w("a"));
    assert_eq!(r.s.take_refusals(), Refusals::default());
    assert_eq!(r.s.load_counters().refused_cpu, 1, "only the first round");
}

/// Behaviour: the hook skips the predicate's CPU test and no other. On a host of 8 GiB with
/// 8 GiB predicted memory held, a judged lease is refused for predicted memory (8 GiB
/// counted, 4 GiB new, limit 8 GiB); with the memory slack at 50% on a host of 32 GiB a lease
/// of 16 GiB beside one of 20 GiB passes the predicate and is refused by the flat-memory
/// guard (20 GiB held, 16 GiB new, limit 32 GiB), judged too.
///
/// Catches: a hook that skips the whole predicate, memory included, or the flat-memory
/// guard.
#[test]
fn a_judged_worker_is_still_refused_by_memory() {
    let mut r = Rig::on();
    r.up("a", Resources::new(100_000, 8 * GIB));
    r.action(1_000, GIB, Some(warm(1_000, 8 * GIB)));
    assert_eq!(r.tick().len(), 1);
    let op = r.action(1_000, GIB, Some(warm(1_000, 4 * GIB)));
    r.s.set_cpu_judged(op, judged(&[("a", 2_000)]));
    assert!(r.tick().is_empty());
    assert_eq!(
        only(r.s.take_refusals()).refusal,
        refusal(RefusalKind::PredictedMemory, 8 * GIB, 4 * GIB, 8 * GIB)
    );

    let mut r = Rig::new(LoadConfig {
        s_mem_permille: 500,
        ..LoadConfig::default()
    });
    r.up("a", wide());
    r.action(1_000, GIB, Some(warm(1_000, 20 * GIB)));
    assert_eq!(r.tick().len(), 1);
    let op = r.action(1_000, GIB, Some(warm(1_000, 16 * GIB)));
    r.s.set_cpu_judged(op, judged(&[("a", 2_000)]));
    assert!(r.tick().is_empty());
    assert_eq!(
        only(r.s.take_refusals()).refusal,
        refusal(RefusalKind::BookedMemory, 20 * GIB, 16 * GIB, 32 * GIB)
    );
}

/// Behaviour: of two workers full of CPU, the one the hook names takes the lease and the
/// other is refused by CPU in the same round, with its exact figures.
///
/// Catches: a hook read as covering every worker of the operation, not those named.
#[test]
fn a_worker_the_map_does_not_name_is_still_refused_by_cpu() {
    for (named, other) in [("a", "b"), ("b", "a")] {
        let mut r = Rig::on();
        r.up("a", node());
        r.up("b", node());
        fill(&mut r, "a", &["b"], 4, 2_000);
        fill(&mut r, "b", &["a"], 4, 2_000);
        let op = r.action(2_000, GIB, Some(warm(2_000, 0)));
        r.s.set_cpu_judged(op, judged(&[(named, 10_000)]));
        let started = r.tick();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].worker, w(named));
        let refused = only(r.s.take_refusals());
        assert_eq!(refused.worker, w(other));
        assert_eq!(
            refused.refusal,
            refusal(RefusalKind::Cpu, 8_000, 2_000, 8_800)
        );
    }
}

/// Behaviour: a later call replaces the map, and an empty map restores the scheduler's CPU
/// test: after the call that named `a`, an empty one makes the full worker refuse again.
///
/// Catches: a second call that adds to the first, or an empty map read as every worker.
#[test]
fn an_empty_map_restores_the_cpu_test() {
    let (mut r, op) = full_of_cpu();
    r.s.set_cpu_judged(op, judged(&[("a", 10_000)]));
    r.s.set_cpu_judged(op, BTreeMap::new());
    assert!(r.tick().is_empty());
    assert_eq!(
        only(r.s.take_refusals()).refusal,
        refusal(RefusalKind::Cpu, 8_000, 2_000, 8_800)
    );
    r.s.set_cpu_judged(op, judged(&[("b", 1)]));
    assert!(r.tick().is_empty(), "a map that names another worker");
    r.s.set_cpu_judged(op, judged(&[("a", 1)]));
    assert_eq!(r.tick().len(), 1);
}

/// Behaviour: over the horizon, a lease that the predicate refuses at the minute a curve on
/// the worker peaks (90 cores held, 90 new, limit 110 cores) is placed when the hook names
/// the worker, and refused as before while it does not.
///
/// Catches: a hook read at minute 0 only, so that the slot-by-slot test still refuses.
#[test]
fn the_horizon_cpu_test_is_skipped_on_a_judged_worker() {
    let (mut r, curves) = holding_busy_at(5, 14);
    curves.set("same", &busy_at(5, 14), 5);
    let op = r.keyed("same", 1_000, 8 * GIB, warm(20_000, GIB));
    assert!(r.tick().is_empty());
    let refused = only(r.s.take_refusals());
    assert_eq!(refused.refusal.kind, RefusalKind::Cpu);
    assert_eq!(
        (
            refused.refusal.counted,
            refused.refusal.new,
            refused.refusal.limit
        ),
        (90_000, 90_000, 110_000)
    );
    r.s.set_cpu_judged(op, judged(&[("a", 100_000)]));
    assert_eq!(r.tick().len(), 1);
    assert_eq!(r.s.take_refusals(), Refusals::default());
}

/// Behaviour: with admission not by load, the booked-CPU sum of the room check is skipped
/// for a judged worker alone. Two 8-core workers each hold a 6-core action; a 4-core one
/// fits neither; once the hook names `a` it is placed there (10 cores booked), never on `b`.
///
/// Catches: a hook that skips the booked sum on every worker, or on none.
#[test]
fn without_load_admission_only_a_judged_worker_skips_the_booked_cpu_sum() {
    for (named, other) in [("a", "b"), ("b", "a")] {
        let mut r = Rig::new(LoadConfig {
            mode: PlacementLoad::Off,
            ..LoadConfig::default()
        });
        r.up("a", node());
        r.up("b", node());
        fill(&mut r, "a", &["b"], 1, 6_000);
        fill(&mut r, "b", &["a"], 1, 6_000);
        let op = r.action(4_000, GIB, None);
        assert!(r.tick().is_empty(), "no worker has the cores booked-free");
        r.s.set_cpu_judged(op, judged(&[(named, 10_000)]));
        let started = r.tick();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].worker, w(named));
        assert_eq!(r.s.booked(&w(named)).expect("known").cpu_millis, 10_000);
        assert_eq!(r.s.booked(&w(other)).expect("known").cpu_millis, 6_000);
    }
}

/// Behaviour: the pack limit reads the CPU figure the caller gave. Two workers of 20 cores
/// are booked at 17 and 6 cores; their own sums with a core are 18 000 (past the 10 000
/// limit) and 7 000 (within), so the lighter is chosen. The caller's figures put the first
/// within the limit (2 000) and the second past it (15 000): the first is chosen. Run both
/// ways round.
///
/// Catches: a pack limit still reading the bookings' sum, and a figure given for a worker
/// that is read at the wrong worker.
#[test]
fn the_pack_limit_follows_the_figures_given() {
    for (heavy, light) in [("a", "b"), ("b", "a")] {
        let mut r = Rig::on();
        for name in ["a", "b"] {
            r.up(name, Resources::new(20_000, 64 * GIB));
        }
        fill(&mut r, heavy, &[light], 17, 1_000);
        fill(&mut r, light, &[heavy], 6, 1_000);
        let op = r.action(1_000, GIB, Some(warm(1_000, 0)));
        r.s.set_cpu_judged(op, judged(&[(heavy, 2_000), (light, 15_000)]));
        let started = r.tick();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].worker, w(heavy), "heavy is {heavy}");
    }
    // Control: with no figure given, the sums choose the lighter worker.
    let mut r = Rig::on();
    for name in ["a", "b"] {
        r.up(name, Resources::new(20_000, 64 * GIB));
    }
    fill(&mut r, "a", &["b"], 17, 1_000);
    fill(&mut r, "b", &["a"], 6, 1_000);
    r.action(1_000, GIB, Some(warm(1_000, 0)));
    assert_eq!(r.tick()[0].worker, w("b"));
}

/// Behaviour: the kind the caller reports is named `cpu-curve` and is the fifth of `ALL`.
#[test]
fn the_cpu_curve_kind_has_its_name() {
    assert_eq!(RefusalKind::CpuCurve.as_str(), "cpu-curve");
    assert_eq!(RefusalKind::ALL.len(), 5);
    assert_eq!(RefusalKind::ALL[4], RefusalKind::CpuCurve);
}
