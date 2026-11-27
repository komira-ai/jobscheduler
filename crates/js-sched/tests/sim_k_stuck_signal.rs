//! Sim k of the placement-by-load design: a node's CPU signal that cannot be believed.
//!
//! The plausibility check compares the CPU time the results of leases admitted and ended
//! inside a span of readings report with the growth of the node's counter over the span,
//! the span reaching back until those leases used 60 core-seconds, at most an hour; a node
//! with no lease that reads more than half its cores for three windows is marked too. Over
//! the fluid model of `load_world`, where each finished lease reports its CPU time and a
//! node's counter can be stuck or count CPU no lease used. Logical ticks only; the bound is
//! checked against the model's true demand.

#[path = "load_world/mod.rs"]
mod load_world;

use js_sched::{LoadConfig, LoadView, Prediction};
use js_types::sched::{LeaseKind, WorkerId};

use load_world::{GIB, Spec, W, World};

fn warm(cpu_millis: u64) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes: 0,
        cold: false,
        life_minutes: 1,
    }
}

/// A 16-core node, `a`, that has sent its first reading, in a world whose leases report
/// their CPU time.
fn world() -> World {
    let mut w = World::new(LoadConfig::default());
    w.lease_cpu = true;
    w.linux("a", 16, 64);
    w.baseline("a");
    w
}

fn view(w: &World) -> LoadView {
    w.s.load_view(&WorkerId::new("a")).expect("a is registered")
}

/// The reading's tick of window `n` (from 1): the agent reads at the last tick of each.
const fn window(n: u64) -> u64 {
    n * W - 1
}

/// The cores the predicate allows on `a`, `(1 + s_cpu) C`, in millicores.
const LIMIT: u64 = 17_600;

/// Sim k (stuck low). Catches: a counter stuck at 0 while leases run believed (no check, or
/// one that never counts a lease), and a node flagged for good once its reader is fixed.
///
/// Leases of 4 cores for two windows run on a node whose counter does not move. The first
/// leases are admitted with the baseline and never count (they may have run before it); the
/// next batch is admitted after the second reading (tick 10), ends at tick 19, and the
/// reading after that, the fifth window's, compares 160 core-seconds with no growth: flagged
/// there. From then the node is charged its leases' predicted sum, and the true demand stays
/// within `(1 + s_cpu) C`. Before it, the fresh readings were believed. Once the counter
/// moves again, the first batch admitted after a reading of the repaired counter agrees
/// within three windows, and the node is charged by measurement again.
#[test]
fn k_a_stuck_counter_is_flagged_within_five_windows_and_believed_again_once_it_moves() {
    let mut w = world();
    w.nodes.get_mut("a").expect("a").counter_stuck = true;
    w.submit_many(200, &Spec::flat("short", 4_000, 2 * W, warm(4_000)));
    let mut flagged = None;
    while w.tick < 60 {
        w.step();
        let (tick, v) = (w.tick - 1, view(&w));
        if v.implausible_lease_cpu && flagged.is_none() {
            flagged = Some(tick);
        }
        match flagged {
            Some(_) => {
                assert!(v.fallback, "charged by prediction at tick {tick}");
                assert_eq!(
                    v.left_cpu_millis,
                    v.predicted_cpu_millis + v.vm_cpu_millis,
                    "the predicted sum at tick {tick}"
                );
            }
            None if tick >= window(1) => assert!(!v.fallback, "believed at tick {tick}"),
            None => {}
        }
    }
    let at = flagged.expect("the stuck counter is flagged");
    assert!(at <= window(5), "flagged at tick {at}");
    assert!(w.nodes["a"].peak_demand <= LIMIT);
    assert_eq!(w.s.load_counters().signal_implausible_lease_cpu, 1);
    assert_eq!(w.s.load_counters().fallback_implausible, 1);

    w.nodes.get_mut("a").expect("a").counter_stuck = false;
    let repaired = w.tick;
    let mut agreed = None;
    while w.tick < repaired + 8 * W {
        w.step();
        if !view(&w).implausible_lease_cpu && agreed.is_none() {
            agreed = Some(w.tick - 1);
        }
    }
    let agreed = agreed.expect("the repaired counter agrees");
    assert!(agreed <= repaired + 3 * W, "agreed at tick {agreed}");
    let v = view(&w);
    assert!(!v.fallback && v.measured_cpu_millis > Some(0));
    assert_eq!(w.s.load_counters().fallback_implausible, 0);
    assert!(w.nodes["a"].peak_demand <= LIMIT);
}

/// Sim k (long beside short). Catches: a lease counted in a span it was not admitted in (a
/// check that took every lease ending inside the span would land a long lease's 400
/// core-seconds in a window whose counter grew 20, and fail a healthy node). Two leases of
/// 40 windows run beside a stream of short ones on a healthy counter: the check compares
/// (the short leases fill its spans), and the node is never flagged, before or after the
/// long leases end.
#[test]
fn k_long_leases_beside_short_ones_on_a_healthy_counter_are_never_flagged() {
    let mut w = world();
    // Past the first reading, so the long leases are admitted inside a span.
    w.run(window(1) + 3);
    w.submit_many(2, &Spec::flat("long", 2_000, 40 * W, warm(2_000)));
    w.submit_many(100, &Spec::flat("short", 2_000, 2 * W, warm(2_000)));
    while w.tick < 80 * W {
        w.step();
        let v = view(&w);
        let tick = w.tick - 1;
        assert!(!v.implausible_lease_cpu, "flagged at tick {tick}");
        assert!(!v.implausible_idle, "flagged idle at tick {tick}");
    }
    assert_eq!(w.stats["long"].finished, 2);
    assert_eq!(w.stats["short"].finished, 100);
    let counters = w.s.load_counters();
    assert!(
        counters.signal_checked_windows > 0,
        "the check never compared"
    );
    assert_eq!(counters.signal_implausible_lease_cpu, 0);
}

/// Sim k (only long leases). Catches: a node whose leases all outlive the span reported
/// failed instead of unchecked, the unchecked time not reported, and a span longer than an
/// hour (these leases run 3700 ticks, admitted after the first reading: a span from it that
/// held them would be 3705 seconds long). Every window is unchecked, the time is counted to
/// the window, and the node is never flagged.
#[test]
fn k_a_node_with_only_leases_longer_than_the_span_is_unchecked_and_reported() {
    let mut w = world();
    w.run(window(1) + 3);
    w.submit_many(4, &Spec::flat("long", 2_000, 3_700, warm(2_000)));
    while w.tick < 760 * W {
        w.step();
        let v = view(&w);
        assert!(!v.implausible_lease_cpu && !v.implausible_idle);
    }
    assert_eq!(w.stats["long"].finished, 4);
    let counters = w.s.load_counters();
    let windows = 760;
    let unchecked = windows * W * 1_000_000;
    assert_eq!(counters.signal_checked_windows, 0);
    assert_eq!(counters.signal_unchecked_windows, windows);
    assert_eq!(counters.signal_unchecked_usec, unchecked);
    assert_eq!(view(&w).unchecked_usec, unchecked);
}

/// Sim k (idle). Catches: an idle node whose counter reads more than half its cores (a
/// reader of the wrong group) believed, flagged before the third window, flagged at exactly
/// half, and kept flagged once an idle window reads half. Its counter adds 8.2 cores a tick
/// with no lease running: flagged at the third window and charged by prediction; at 8 cores
/// (half of 16) the next window clears it.
#[test]
fn k_an_idle_node_reading_over_half_its_cores_for_three_windows_is_flagged() {
    let mut w = world();
    w.nodes.get_mut("a").expect("a").counter_extra_millis = 8_200;
    w.run(window(2) + 1);
    assert!(!view(&w).implausible_idle, "two windows are not three");
    w.run(W);
    let v = view(&w);
    assert!(v.implausible_idle && v.fallback);
    let counters = w.s.load_counters();
    assert_eq!(counters.signal_implausible_idle, 1);
    assert_eq!(counters.fallback_implausible, 1);
    w.nodes.get_mut("a").expect("a").counter_extra_millis = 8_000;
    w.run(W);
    let v = view(&w);
    assert!(!v.implausible_idle && !v.fallback);
}

/// Sim k (whole machine). Catches: the idle rule reading the ledger alone, which charges no
/// whole-machine lease: a node running one on all its cores would read as idle and busy, and
/// be flagged at the third window.
#[test]
fn k_a_node_running_a_whole_machine_lease_is_not_idle() {
    let mut w = world();
    let whole = Spec {
        kind: LeaseKind::WholeMachine,
        request_cpu: 16_000,
        limit: 64 * GIB,
        hint: None,
        ..Spec::flat("whole", 16_000, 8 * W, warm(16_000))
    };
    w.submit(&whole);
    w.run(8 * W);
    assert_eq!(w.stats["whole"].started, 1);
    assert!(w.nodes["a"].peak_demand >= 16_000);
    assert!(!view(&w).implausible_idle);
    assert_eq!(w.s.load_counters().signal_implausible_idle, 0);
}
