//! Simulations of admission by memory, of the fallback and of the cost a lease keeps,
//! over the fluid model of `load_world`.

#[path = "load_world/mod.rs"]
mod load_world;

use js_sched::{LoadConfig, PlacementLoad, Prediction};

use load_world::{GIB, Reporting, Spec, World};

fn warm(cpu_millis: u64, memory_bytes: u64) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes,
        cold: false,
        life_minutes: 1,
    }
}

/// An action that holds 1 GiB and allocates up to `peak` at 60% of its 50 ticks, predicted
/// at 4 GiB.
fn late_ramp(class: &'static str, peak: u64) -> Spec {
    Spec {
        limit: 8 * GIB,
        mem: vec![(30, GIB), (20, peak)],
        ..Spec::flat(class, 500, 50, warm(250, 4 * GIB))
    }
}

/// Sim c. Catches: predicted memory decaying with the measurement (allocating late must
/// not let the node take more than it holds): on a 64 GiB worker, actions predicted at
/// 4 GiB that allocate their 4 GiB at 60% of their run, a few (3%) that really peak at
/// 6 GiB, never put more than `C max(1, k_m)` = 96 GiB of true memory on the node, and
/// run 16 at a time. The cold page cache (80% of RAM) is not in the working set the agent
/// reports (`memory.current` minus the inactive file pages), so it does not keep leases out.
#[test]
fn c_late_ramps_and_tail_runs_stay_within_the_memory_bound() {
    let mut w = World::new(LoadConfig::default());
    w.linux("n", 8, 64);
    for n in 0..120 {
        let spec = if n % 33 == 32 {
            late_ramp("tail", 6 * GIB)
        } else {
            late_ramp("ramp", 4 * GIB)
        };
        w.submit(&spec);
    }
    w.run(1_000);
    assert!(
        w.stats.values().all(|s| s.finished == s.submitted),
        "{:?}",
        w.stats
            .values()
            .map(|s| (s.submitted, s.finished))
            .collect::<Vec<_>>()
    );
    let node = &w.nodes["n"];
    assert!(
        node.peak_memory <= 96 * GIB,
        "{} GiB",
        node.peak_memory / GIB
    );
    assert_eq!(node.peak_running, 16, "full at 4 GiB each");
}

/// Sim c (idle node). Catches: the livelock of a node whose retained file pages fill its
/// memory: after the leases end, the working set the agent reports stays at 110% of the
/// node's RAM (hot file pages nothing reclaims), and a working set counted whole would refuse
/// every lease for good. All five new actions start.
#[test]
fn c_retained_page_cache_on_an_idle_node_still_admits() {
    let mut w = World::new(LoadConfig::default());
    w.linux("n", 8, 64);
    w.submit_many(3, &late_ramp("ramp", 4 * GIB));
    w.run(60);
    assert_eq!(w.stats["ramp"].finished, 3);
    w.nodes.get_mut("n").unwrap().cache = 64 * GIB + 64 * GIB / 10;
    w.run(2 * load_world::W);
    w.submit_many(5, &late_ramp("late", 4 * GIB));
    w.run(1);
    assert_eq!(w.running_on("n"), 5);
}

/// Sim c (busy node). Catches: the cap on the file pages missing while leases run: with hot
/// file pages at 110% of RAM in the working set, counted whole they leave no room for any
/// lease; capped at a quarter of the node's memory they cost 16 GiB of the 64, and the node
/// still takes sixteen leases of 4 GiB (each with the 1 GiB it holds before it allocates
/// the rest) while the working set stays under their predictions.
#[test]
fn c_retained_page_cache_beside_leases_costs_a_bounded_share() {
    let mut w = World::new(LoadConfig::default());
    w.linux("n", 8, 64);
    w.nodes.get_mut("n").unwrap().cache = 64 * GIB + 64 * GIB / 10;
    // Two readings in first, so the node is charged by measurement.
    w.run(12);
    w.submit_many(40, &late_ramp("ramp", 4 * GIB));
    w.run(30);
    assert_eq!(w.nodes["n"].peak_running, 16);
}

/// `(1 / f) (1 + s_cpu) C` for `cores` cores, in millicores: what constant-duty actions
/// predicted at their demand can put on a measured node, since a saturated node's
/// measurement stops at its cores and only the floor limits what is admitted after.
fn floor_bound(cores: u64) -> u64 {
    cores * 1_100 * 4 / 3
}

fn constant(class: &'static str, millis: u64, ticks: u64) -> Spec {
    Spec {
        request_cpu: 2_000,
        ..Spec::flat(class, millis, ticks, warm(1_000, 0))
    }
}

fn node16(config: LoadConfig) -> World {
    let mut w = World::new(config);
    w.linux("n", 16, 64);
    w
}

/// A queue of constant one-core actions that book two cores each, on a 16-core node, run
/// for 120 ticks; the most that was running, and the node's peak demand in millicores.
fn run_constant(w: &mut World, ticks: u64) -> (usize, u64) {
    w.submit_many(60, &constant("c", 1_000, 100));
    w.run(ticks);
    (w.nodes["n"].peak_running, w.nodes["n"].peak_demand)
}

/// Sim d (no measurement). Catches: a node that never reports (a Mac, a node without the
/// cgroup) admitted by the booked rule (eight leases at two booked cores each) or without
/// limit: it is admitted by the sum of the predictions, 17 leases of a core on 16 cores
/// (17 of 17.6), and the demand stays there.
#[test]
fn d_a_node_that_never_reports_is_admitted_by_the_sum_of_predictions() {
    let mut w = node16(LoadConfig::default());
    w.nodes.get_mut("n").unwrap().reporting = Reporting::Silent;
    let (running, demand) = run_constant(&mut w, 60);
    assert_eq!((running, demand), (17, 17_000));
    assert_eq!(w.s.fallback_workers(), 1);
}

/// Sim d (partial fields). Catches: a reading that lists no covered lease read as
/// "everything covered": the leases then carry their full predictions (nothing is
/// covered), so the node is admitted as by the sum of predictions.
#[test]
fn d_readings_without_covered_leases_charge_the_full_predictions() {
    let mut w = node16(LoadConfig::default());
    w.nodes.get_mut("n").unwrap().reporting = Reporting::NoCovered;
    let (running, demand) = run_constant(&mut w, 60);
    assert_eq!((running, demand), (17, 17_000));
    let view = w.s.load_view(&js_types::sched::WorkerId::new("n")).unwrap();
    assert!(!view.fallback);
    assert_eq!(view.uncovered_cpu_millis, view.predicted_cpu_millis);
}

/// Sim d (going stale and returning). Catches: a reading believed past three windows and a
/// worker that does not come back: the node is measured, silent for four windows (not
/// stale after three, stale after the fourth: charged by prediction, which at 22 leases
/// of a core on 16.0 admits no more), and measured again once two readings are in.
#[test]
fn d_a_node_going_stale_and_returning() {
    let mut w = node16(LoadConfig::default());
    w.submit_many(60, &constant("c", 1_000, 100));
    w.run(40);
    assert_eq!(w.s.fallback_workers(), 0);
    assert!(w.nodes["n"].peak_demand <= floor_bound(16));
    w.nodes.get_mut("n").unwrap().reporting = Reporting::Silent;
    w.run(3 * load_world::W);
    assert_eq!(
        w.s.fallback_workers(),
        0,
        "three windows old is not stale yet"
    );
    w.run(load_world::W);
    assert_eq!(w.s.fallback_workers(), 1);
    let started = w.stats["c"].started;
    w.run(load_world::W);
    assert_eq!(
        w.stats["c"].started, started,
        "the sum of predictions admits no more"
    );
    w.nodes.get_mut("n").unwrap().reporting = Reporting::Normal;
    w.run(2 * load_world::W);
    assert_eq!(w.s.fallback_workers(), 0);
}

/// Sim d (one stale among fresh). Catches: the staleness of one worker spreading to
/// another, or the stale one unbounded: with `b` silent, `a` is measured and `b` is
/// charged by prediction, and the measured one stays inside the bound of the floor and the other inside the slack.
#[test]
fn d_one_stale_node_among_fresh_ones() {
    let mut w = World::new(LoadConfig::default());
    w.linux("a", 16, 64);
    w.linux("b", 16, 64);
    w.nodes.get_mut("b").unwrap().reporting = Reporting::Silent;
    w.submit_many(80, &constant("c", 1_000, 100));
    w.run(60);
    assert_eq!(w.s.fallback_workers(), 1);
    let a = w.s.load_view(&js_types::sched::WorkerId::new("a")).unwrap();
    let b = w.s.load_view(&js_types::sched::WorkerId::new("b")).unwrap();
    assert!(!a.fallback && b.fallback);
    assert!(w.nodes["a"].peak_demand <= floor_bound(16));
    assert!(
        w.nodes["b"].peak_demand <= 17_600,
        "by the sum of predictions"
    );
}

/// Sim d (off and shadow). Catches: `off` or `shadow` placing differently from the booked
/// rule: the same workload gives the same starts, leases and workers under both. (The
/// recorded traces of the existing simulations are unchanged by this work: a scheduler
/// built with `new` places by the booked rule.)
#[test]
fn d_off_and_shadow_place_the_same() {
    let run = |mode| {
        let mut w = World::new(LoadConfig {
            mode,
            ..LoadConfig::default()
        });
        w.linux("a", 16, 64);
        w.linux("b", 16, 64);
        w.submit_many(80, &constant("c", 1_000, 30));
        w.run(300);
        w.starts
    };
    let off = run(PlacementLoad::Off);
    assert_eq!(off.len(), 80);
    assert_eq!(run(PlacementLoad::Shadow), off);
}

/// Sim j. Catches: costs recomputed after a restart from the current estimator, and
/// recovered leases not counted: a 16-core node holds leases predicted at a core when the
/// scheduler restarts; the estimator now says 100 m for the keys of those leases. The new scheduler holds
/// the same sums, and no lease is placed beyond the bound across the restart (fifteen more
/// would be placed at once if the old leases were charged 100 m).
#[test]
fn j_a_restart_keeps_what_was_admitted() {
    let mut w = node16(LoadConfig::default());
    w.submit_many(60, &constant("c", 1_000, 100));
    w.run(40);
    let node = js_types::sched::WorkerId::new("n");
    let before = w.s.load_view(&node).unwrap();
    assert!(before.predicted_cpu_millis >= 17_000);
    w.restart(warm(100, 0));
    let after = w.s.load_view(&node).unwrap();
    assert_eq!(
        after.predicted_cpu_millis, before.predicted_cpu_millis,
        "{after:?}"
    );
    assert!(
        after.fallback,
        "the first reading of the new session is a baseline"
    );
    w.run(30);
    assert!(
        w.nodes["n"].peak_demand <= floor_bound(16),
        "{}",
        w.nodes["n"].peak_demand
    );
    assert!(w.running_on("n") <= 22);
}

/// Sim l. Catches: the cost of a pool-sized key learned on a big node (60 cores) refusing
/// it on a small one for good: placed on a 16-thread and an 88-thread node, a lease
/// predicted at 60 cores is charged the node's cores on the small one, runs on both, and
/// the demand stays within the slack on the small one, and on the big one within the
/// bound for a key that demands 1.47 times its prediction.
#[test]
fn l_a_pool_sized_key_runs_on_nodes_of_different_sizes() {
    let mut w = World::new(LoadConfig::default());
    w.linux("small", 16, 64);
    w.linux("big", 88, 512);
    let pool = Spec {
        wide: true,
        request_cpu: 16_000,
        ..Spec::flat("pool", 0, 20, warm(60_000, 4 * GIB))
    };
    let light = Spec::flat("light", 300, 10, warm(500, GIB / 4));
    for _ in 0..8 {
        w.submit(&pool);
        w.submit_many(10, &light);
    }
    w.run(400);
    let on = |node: &str| {
        w.starts
            .iter()
            .filter(|s| s.1 == "pool" && s.2 == node)
            .count()
    };
    assert!(
        on("small") > 0 && on("big") > 0,
        "{} {}",
        on("small"),
        on("big")
    );
    assert!(w.nodes["small"].peak_demand <= 17_600);
    // On the big node the key is predicted at 60 cores and demands all 88: `k` = 1.47, and
    // the bound is `(k / f) (1 + s) C`.
    assert!(w.nodes["big"].peak_demand <= 88 * 1_100 * 147 / 75 + 1_000);
}

/// Sim m. Catches: shadow changing a placement, and verdicts that are not the ones `on`
/// gives: on one 88-thread node, `shadow` starts what `off` starts (the same leases on the
/// same ticks), and its verdicts of the first round are those of an `on` run: the leases
/// `on` admits it would admit, and the rest of what `off` placed it would refuse.
#[test]
fn m_shadow_changes_nothing_and_its_verdicts_are_ons() {
    let run = |mode| {
        let mut w = World::new(LoadConfig {
            mode,
            ..LoadConfig::default()
        });
        w.linux("n", 88, 512);
        w.submit_many(300, &Spec::flat("e", 2_000, 50, warm(2_000, 4 * GIB)));
        w.step();
        let first = (w.s.load_counters(), w.stats["e"].started);
        w.run(99);
        (first, w.starts)
    };
    let (on_first, _) = run(PlacementLoad::On);
    let (off_first, off_starts) = run(PlacementLoad::Off);
    let (shadow_first, shadow_starts) = run(PlacementLoad::Shadow);
    assert_eq!(shadow_starts, off_starts);
    assert_eq!(on_first.1, 48, "96800 of 2000 m: 48 leases");
    assert_eq!(off_first.1, 88, "88 booked cores");
    let (counters, started) = shadow_first;
    assert_eq!(started, 88);
    assert_eq!(counters.would_admit, on_first.0.admitted);
    assert_eq!(counters.would_block, off_first.1 - on_first.1);
    assert_eq!(counters.admitted, 0);
}

/// Sim n. Catches: a retry up the memory ladder counted at what was learned (2 GiB)
/// instead of the rung it runs at, and a key that keeps being killed: an action that is
/// killed at its 4 GiB limit is rerun at 16 GiB and completes; while it runs again it is
/// charged 16 GiB; with the booking the kill taught, the next action of the key runs
/// once and is not killed.
#[test]
fn n_a_retry_is_counted_at_the_rung() {
    let mut w = World::new(LoadConfig::default());
    w.linux("n", 16, 64);
    let spiky = Spec {
        limit: 4 * GIB,
        mem: vec![(8, GIB), (20, 10 * GIB)],
        ..Spec::flat("spiky", 500, 28, warm(500, 2 * GIB))
    };
    w.submit(&spiky);
    let node = js_types::sched::WorkerId::new("n");
    let mut charged = Vec::new();
    for _ in 0..80 {
        w.step();
        charged.push(w.s.load_view(&node).unwrap().predicted_memory_bytes);
    }
    let stats = &w.stats["spiky"];
    assert_eq!((stats.killed, stats.finished), (1, 1));
    assert_eq!(charged[0], 2 * GIB, "the first run at what was learned");
    assert!(
        charged.contains(&(16 * GIB)),
        "the retry at the rung: {charged:?}"
    );
    assert!(
        charged
            .iter()
            .all(|&c| c == 0 || c == 2 * GIB || c == 16 * GIB)
    );
    // The booking the kill taught: the next action of the key is predicted at the rung and
    // limited to it, and runs once.
    let learned = Spec {
        limit: 16 * GIB,
        hint: Some(warm(500, 16 * GIB)),
        ..spiky
    };
    w.submit(&learned);
    w.run(60);
    let stats = &w.stats["spiky"];
    assert_eq!((stats.killed, stats.finished), (1, 2));
}

/// Sim i. Catches: a key understated four times (1 core predicted, 4 used) admitted without
/// the bound the prediction gives: at first the node holds 4.4 C of demand (`k` = 4 on the
/// slack), and with the cost corrected to within 1.25 times the truth the same key puts at
/// most 1.1 C on the node. (The estimator's rule that raises the cost under load is
/// `js-sizing`'s; this checks what the scheduler does with each figure.)
#[test]
fn i_an_understated_key_is_bounded_by_its_prediction() {
    let demand = |predicted: u64| {
        let mut w = World::new(LoadConfig::default());
        w.linux("n", 88, 512);
        w.submit_many(400, &Spec::flat("k", 4_000, 60, warm(predicted, GIB)));
        w.run(30);
        w.nodes["n"].peak_demand
    };
    let (understated, corrected) = (demand(1_000), demand(5_000));
    assert!(
        understated > 4_400 * 88 / 1000 * 1000,
        "past 4.4 C: {understated}"
    );
    assert!(
        understated <= 4 * 88 * 1_100 * 4 / 3,
        "within (k / f) (1 + s) C"
    );
    assert!(corrected <= 96_800 + 4_000, "{corrected}");
}
