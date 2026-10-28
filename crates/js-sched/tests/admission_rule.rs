//! The flat load predicate through the scheduler's public inputs, one rule each: how a
//! worker is charged with and without a reading, what a reading must be to be believed,
//! the memory term, the cold cap, and the cost a lease is charged.
//!
//! The rig feeds readings as a worker's heartbeats; a window is 5 s of farm time.

#[path = "load_rig/mod.rs"]
mod load_rig;

use js_sched::{LoadConfig, LoadView};
use js_types::sched::usage::{LeaseCost, key_hash};
use js_types::sched::{Failure, LeaseId, Outcome, Resources, StartLease};

use load_rig::{GIB, Rig, W_MS, cold, leases, w, warm};

/// An 8-core worker with 32 GiB.
fn node() -> Resources {
    Resources::new(8_000, 32 * GIB)
}

/// The cost `start`'s lease is charged, with its key hash checked against its request's
/// key and then zeroed, so a test compares the other figures.
fn cost_of(r: &Rig, start: &StartLease) -> Option<LeaseCost> {
    let cost = r.s.lease_cost(start.lease)?;
    let key = &r.s.request(start.operation).expect("held").key;
    assert_eq!(cost.key_hash, key_hash(key));
    Some(LeaseCost {
        key_hash: 0,
        ..cost
    })
}

fn view(r: &Rig, name: &str) -> LoadView {
    r.s.load_view(&w(name)).expect("registered")
}

/// `count` actions that book one core and are predicted at `hint`, placed in one round.
fn fill(r: &mut Rig, count: u64, hint: js_sched::Prediction) -> Vec<LeaseId> {
    for _ in 0..count {
        r.action(1_000, 8 * GIB, Some(hint));
    }
    leases(&r.tick())
}

/// A worker `a` holding eight leases predicted at 500 m, with two readings in: a baseline
/// and a window of `busy` millicores over them.
fn measured(busy: u64, working_set: u64) -> (Rig, Vec<LeaseId>) {
    let mut r = Rig::on();
    r.up("a", node());
    let held = fill(&mut r, 8, warm(500, 0));
    assert_eq!(held.len(), 8);
    r.window("a", 0, 0, &held);
    r.window("a", busy, working_set, &held);
    (r, held)
}

/// Catches: a worker with no window admitted by the booked rule (8 one-core leases) or by
/// no rule at all: it is admitted by the sum of its leases' predicted costs against
/// capacity plus the CPU slack, 17 leases of 500 m on 8 cores (8500 of 8800), and the
/// 18th waits (and holds the worker for the head of the queue, so the two behind it are
/// not counted as blocked). Its one reading, the baseline, makes it a worker that
/// measures: no floor on its leases.
#[test]
fn a_worker_without_a_window_is_admitted_by_the_sum_of_predicted_costs() {
    let mut r = Rig::on();
    r.up("a", node());
    r.baseline("a");
    let placed = fill(&mut r, 20, warm(500, 0));
    assert_eq!(placed.len(), 17);
    let v = view(&r, "a");
    assert!(v.fallback);
    assert_eq!(v.predicted_cpu_millis, 8_500);
    assert_eq!(v.left_cpu_millis, 8_500, "each lease is charged once");
    assert_eq!(v.measured_cpu_millis, None);
    assert_eq!(r.s.fallback_workers(), 1);
    let c = r.s.load_counters();
    assert_eq!(
        (c.admitted, c.admitted_fallback, c.admitted_cold),
        (17, 17, 0)
    );
    assert_eq!(
        c.blocked, 1,
        "the 18th is blocked and holds the worker: the two behind it are not tried there"
    );
    assert_eq!(c.cold_deferred, 0);
}

/// Catches: a worker nothing has measured (no reading ever: a Mac, a native or VM node)
/// charged the estimates of a light population as they are: it charges every lease at
/// least 1000 m, so 0.3-core keys (estimated at 500 m) fill it at 8 leases, 8000 of 8800,
/// where a worker that has measured takes 17. The lease is recorded at its estimate; the
/// floor is the worker's.
#[test]
fn a_worker_never_measured_charges_each_lease_at_least_a_core() {
    let mut r = Rig::on();
    r.up("a", node());
    let placed = fill(&mut r, 20, warm(500, 0));
    assert_eq!(placed.len(), 8);
    let v = view(&r, "a");
    assert!(v.fallback);
    assert_eq!(v.left_cpu_millis, 8_000);
    assert_eq!(v.predicted_cpu_millis, 4_000, "the sums hold the estimates");
    assert_eq!(r.s.lease_cost(placed[0]).unwrap().cpu_millis, 500);
    // A lease above the floor is charged what it is.
    let mut big = Rig::on();
    big.up("a", node());
    big.action(2_500, 8 * GIB, Some(warm(2_500, 0)));
    assert_eq!(leases(&big.tick()).len(), 1);
    assert_eq!(view(&big, "a").left_cpu_millis, 2_500);
}

/// Catches: the floor kept after the worker has measured, or applied to the sub-core
/// charge of a measured worker. A worker whose signal went stale was measured once, so
/// its leases keep their estimates (500 m: 9 more fit beside 4000); and at the least
/// estimate, 250 m, 35 leases fill 8750 of 8800.
#[test]
fn a_worker_that_has_measured_keeps_the_sub_core_charge_when_stale() {
    let (mut r, _) = measured(1_000, GIB);
    r.ms += 3 * W_MS + 1;
    r.tick();
    let v = view(&r, "a");
    assert!(v.fallback);
    assert_eq!(v.left_cpu_millis, 4_000);
    assert_eq!(
        fill(&mut r, 20, warm(500, 0)).len(),
        9,
        "4000 + 9 * 500 of 8800"
    );

    let mut light = Rig::on();
    light.up("a", node());
    light.baseline("a");
    assert_eq!(fill(&mut light, 40, warm(250, 0)).len(), 35);
    assert_eq!(view(&light, "a").left_cpu_millis, 8_750);
}

/// Catches: the CPU limit read as exclusive (`>=`): a lease landing exactly on
/// `C * (1 + s_cpu)` (8800 on 8 cores) is admitted, and one milli over waits.
#[test]
fn a_lease_landing_exactly_on_the_cpu_limit_is_admitted_and_one_milli_over_is_not() {
    for (last, admitted) in [(300, true), (301, false)] {
        let mut r = Rig::on();
        r.up("a", node());
        r.baseline("a");
        assert_eq!(fill(&mut r, 17, warm(500, 0)).len(), 17);
        r.action(1_000, GIB, Some(warm(last, 0)));
        assert_eq!(leases(&r.tick()).len(), usize::from(admitted), "{last} m");
    }
}

/// Catches: the working set added to the fallback's memory sum. A worker whose signal
/// went stale is charged the predicted memory of its leases alone: beside a stale
/// reading of 30 GiB in use and leases predicted at no memory, 4 GiB still fits 32 GiB.
#[test]
fn the_fallback_memory_sum_leaves_the_working_set_out() {
    let (mut r, _) = measured(1_000, 30 * GIB);
    assert!(
        view(&r, "a").left_memory_bytes >= 30 * GIB,
        "fresh, the working set counts"
    );
    r.ms += 3 * W_MS + 1;
    r.tick();
    let v = view(&r, "a");
    assert!(v.fallback);
    assert_eq!(v.left_memory_bytes, 0);
    r.action(500, 4 * GIB, Some(warm(500, 4 * GIB)));
    assert_eq!(leases(&r.tick()).len(), 1);
}

/// Catches: the measurement ignored, or believed below the floor. Eight leases predicted at
/// 500 m (4000) with 1000 m measured are charged `f` of 4000 = 3000, so 11 more of 500 m
/// fit under 8800, and the floor binds in that window.
#[test]
fn a_reading_replaces_the_sum_but_not_below_the_floor() {
    let (mut r, _) = measured(1_000, GIB);
    let v = view(&r, "a");
    assert!(!v.fallback);
    assert_eq!(v.measured_cpu_millis, Some(1_000));
    assert_eq!(v.left_cpu_millis, 3_000);
    assert_eq!(v.predicted_cpu_millis, 4_000);
    assert_eq!(v.uncovered_cpu_millis, 0);
    assert_eq!(r.s.fallback_workers(), 0);
    assert_eq!(fill(&mut r, 20, warm(500, 0)).len(), 11);
    let c = r.s.load_counters();
    assert_eq!((c.readings_accepted, c.readings_dropped), (2, 0));
    assert_eq!(
        (c.floor_windows, c.floor_binds),
        (1, 1),
        "the baseline gives no window; the second reading's window is under the floor"
    );
}

/// Catches: a floor rounded down (an under-charge by a millicore), and `f` applied to the
/// measured term instead of the predicted: three covered leases of 250 m, 750 m, are
/// charged 563 (`ceil(0.75 * 750)`) when nothing is measured.
#[test]
fn the_floor_is_rounded_up() {
    let mut r = Rig::on();
    r.up("a", node());
    let held = fill(&mut r, 3, warm(250, 0));
    r.window("a", 0, 0, &held);
    r.window("a", 0, 0, &held);
    assert_eq!(view(&r, "a").left_cpu_millis, 563);
}

/// Catches: a rise not believed at once, or a fall believed at once: with 3000 m as the
/// floor, a window of 6000 m is charged 6000; the next window of 1000 m still is (the
/// larger of the last two); the one after is back at the floor.
#[test]
fn a_rise_is_believed_at_once_and_a_fall_after_two_windows() {
    let (mut r, held) = measured(6_000, GIB);
    assert_eq!(view(&r, "a").left_cpu_millis, 6_000);
    r.window("a", 1_000, GIB, &held);
    let v = view(&r, "a");
    assert_eq!(
        (v.measured_cpu_millis, v.left_cpu_millis),
        (Some(6_000), 6_000)
    );
    r.window("a", 1_000, GIB, &held);
    let v = view(&r, "a");
    assert_eq!(
        (v.measured_cpu_millis, v.left_cpu_millis),
        (Some(1_000), 3_000)
    );
}

/// Catches: leases placed since the last reading not carrying their full cost: after the
/// reading, four new leases of 500 m are uncovered and add 2000 on top of the measured
/// 6000, and once a reading lists them as covered they are charged through the
/// measurement alone.
#[test]
fn leases_a_reading_does_not_cover_carry_their_full_cost() {
    let (mut r, mut held) = measured(6_000, GIB);
    let fresh = fill(&mut r, 4, warm(500, 0));
    assert_eq!(fresh.len(), 4);
    let v = view(&r, "a");
    assert_eq!((v.uncovered_cpu_millis, v.left_cpu_millis), (2_000, 8_000));
    held.extend(&fresh);
    r.window("a", 6_000, GIB, &held);
    let v = view(&r, "a");
    assert_eq!((v.uncovered_cpu_millis, v.left_cpu_millis), (0, 6_000));
}

/// Catches: predicted memory decaying with the measurement: eight leases predicted at
/// 4 GiB fill 32 GiB while the node reports a working set of 1 GiB, and a ninth waits;
/// the CPU side is nowhere near full.
#[test]
fn predicted_memory_is_held_to_the_end() {
    let mut r = Rig::on();
    r.up("a", node());
    let held = fill(&mut r, 12, warm(250, 4 * GIB));
    assert_eq!(held.len(), 8, "32 GiB of 4 GiB");
    r.window("a", 0, GIB, &held);
    r.window("a", 0, GIB, &held);
    let v = view(&r, "a");
    assert_eq!(v.left_memory_bytes, 32 * GIB);
    assert_eq!(v.working_set_bytes, Some(GIB));
    assert!(fill(&mut r, 1, warm(250, 4 * GIB)).is_empty());
    assert!(r.s.load_counters().blocked >= 1);
}

/// Catches: a working set that cannot raise the memory figure above the predictions (a
/// key whose real peak is higher than predicted): four leases predicted at 2 GiB with a
/// working set of 12 GiB are charged 12; and hot file pages in the working set count for at
/// most a quarter of the node's memory (8 GiB), the rest being cache: a working set of
/// 30 GiB of which 20 are active file pages is charged 10 + 8 = 18, and one of which 6 are
/// (under the share) is charged whole.
#[test]
fn a_working_set_above_the_predictions_raises_memory_with_its_file_pages_capped() {
    let at = |working_set: u64, file: u64| {
        let mut r = Rig::on();
        r.up("a", node());
        let held = fill(&mut r, 4, warm(250, 2 * GIB));
        r.window_file("a", 0, working_set, file, &held);
        r.window_file("a", 0, working_set, file, &held);
        view(&r, "a").left_memory_bytes
    };
    assert_eq!(at(12 * GIB, 0), 12 * GIB);
    assert_eq!(at(30 * GIB, 20 * GIB), 18 * GIB);
    assert_eq!(at(30 * GIB, 6 * GIB), 30 * GIB);
    assert_eq!(at(30 * GIB, 8 * GIB), 30 * GIB, "exactly the share");
    assert_eq!(at(30 * GIB, 8 * GIB + 1), 30 * GIB - 1);
}

/// Catches: the working set not added to what no reading covers yet: with 12 GiB measured
/// and a new 4 GiB lease, memory is `max(8 + 4, 12 + 4)`, 16 GiB.
#[test]
fn an_uncovered_lease_adds_to_the_working_set() {
    let mut r = Rig::on();
    r.up("a", node());
    let held = fill(&mut r, 4, warm(250, 2 * GIB));
    r.window("a", 0, 12 * GIB, &held);
    r.window("a", 0, 12 * GIB, &held);
    assert_eq!(fill(&mut r, 1, warm(250, 4 * GIB)).len(), 1);
    let v = view(&r, "a");
    assert_eq!(v.uncovered_memory_bytes, 4 * GIB);
    assert_eq!(v.left_memory_bytes, 16 * GIB);
}

/// Catches: the livelock of an idle node whose page cache fills its memory: after leases
/// end, the hot file pages stay in the working set (here 110% of the node's memory) with
/// nothing to reclaim them, and a working set counted whole would refuse every lease for
/// good. A node with no action lease counts none of it and admits a lease as large as the
/// node; a node with one small lease counts it for at most a quarter of the node.
#[test]
fn retained_page_cache_does_not_stop_an_idle_node_from_admitting() {
    let mut r = Rig::on();
    r.up("a", node());
    r.window_file("a", 0, 35 * GIB, 35 * GIB, &[]);
    r.window_file("a", 0, 35 * GIB, 35 * GIB, &[]);
    let v = view(&r, "a");
    assert!(!v.fallback);
    assert_eq!(v.working_set_bytes, Some(35 * GIB));
    assert_eq!(v.left_memory_bytes, 0);
    assert_eq!(
        fill(&mut r, 1, warm(250, 32 * GIB)).len(),
        1,
        "as large as the node"
    );
}

/// Catches: the cap on the file pages missing when a lease runs: one 1 GiB lease beside
/// hot file pages of 110% of the node's memory is charged 8 GiB (the share of the node's
/// memory the file pages may count for), so a 24 GiB lease fits beside it and a 25 GiB one
/// does not.
#[test]
fn file_pages_beside_a_small_lease_count_for_a_bounded_share() {
    let beside = |memory: u64| {
        let mut r = Rig::on();
        r.up("a", node());
        let held = fill(&mut r, 1, warm(250, GIB));
        r.window_file("a", 0, 35 * GIB, 35 * GIB, &held);
        r.window_file("a", 0, 35 * GIB, 35 * GIB, &held);
        assert_eq!(view(&r, "a").left_memory_bytes, 8 * GIB);
        r.action(1_000, 8 * GIB, Some(warm(250, memory)));
        r.tick().len()
    };
    assert_eq!(beside(24 * GIB), 1);
    assert_eq!(beside(25 * GIB), 0);
}

/// Catches: a configurable memory slack ignored (`s_mem`): with a quarter more allowed,
/// ten leases of 4 GiB fit 32 GiB and the eleventh does not.
#[test]
fn the_memory_slack_is_the_configured_one() {
    let mut r = Rig::new(LoadConfig {
        s_mem_permille: 250,
        ..LoadConfig::default()
    });
    r.up("a", node());
    r.baseline("a");
    assert_eq!(fill(&mut r, 12, warm(250, 4 * GIB)).len(), 10);
}

/// Catches: a reading used past its age: three windows after it arrived it is still
/// believed, one millisecond more and the worker is charged by prediction. Age is on the
/// scheduler's farm time, which only the inputs it is fed move.
#[test]
fn a_reading_is_stale_after_three_windows() {
    let (mut r, _) = measured(1_000, GIB);
    r.ms += 3 * W_MS;
    r.tick();
    assert!(!view(&r, "a").fallback);
    r.ms += 1;
    r.tick();
    let v = view(&r, "a");
    assert!(v.fallback);
    assert_eq!(v.left_cpu_millis, 4_000, "the sum of predicted costs");
    assert_eq!(r.s.fallback_workers(), 1);
}

/// Catches: a worker whose actions cgroup has a CPU limit measured anyway (the limit hides
/// overload, so its measurement reads low): it is charged by prediction until a reading
/// says the limit is gone.
#[test]
fn a_cpu_limited_cgroup_is_the_fallback() {
    let (mut r, held) = measured(1_000, GIB);
    let mut limited = r.reading("a", 1_000, GIB, held.clone());
    limited.actions_cpu_limited = true;
    r.ms += W_MS;
    r.beat("a", Some(limited));
    assert!(view(&r, "a").fallback);
    r.window("a", 1_000, GIB, &held);
    assert!(!view(&r, "a").fallback);
}

/// Catches: the CPU-limited fallback not told apart from the others (a node whose actions
/// cgroup has a limit looks like any stale or unmeasured worker), or counted for a worker
/// that is not in the fallback for that reason: `limited` reports a CPU limit and is
/// counted, `stale` went silent and `fresh` is measured and neither is, `new` never sent
/// a reading and is not, `gone` reported a limit and was then lost and is not. In shadow
/// mode no worker is counted, and the limit's end takes the count back to zero.
#[test]
fn a_cpu_limited_worker_is_counted_apart_from_the_other_fallbacks() {
    let (mut r, held) = measured(1_000, GIB);
    assert_eq!(r.s.load_counters().fallback_cpu_limited, 0);
    let mut limited = r.reading("a", 1_000, GIB, held.clone());
    limited.actions_cpu_limited = true;
    r.ms += W_MS;
    r.beat("a", Some(limited));
    assert_eq!(r.s.fallback_workers(), 1);
    assert_eq!(r.s.load_counters().fallback_cpu_limited, 1);
    r.window("a", 1_000, GIB, &held);
    assert_eq!(r.s.load_counters().fallback_cpu_limited, 0);

    // A second node, never measured, and a third gone silent: fallbacks, not counted.
    r.up("new", node());
    r.up("stale", node());
    r.baseline("stale");
    r.window("stale", 0, 0, &[]);
    r.ms += 4 * W_MS;
    r.window("a", 1_000, GIB, &held);
    r.tick();
    assert_eq!(r.s.fallback_workers(), 2);
    assert_eq!(r.s.load_counters().fallback_cpu_limited, 0);

    // A limited node that is lost is not counted.
    r.up("gone", node());
    r.baseline("gone");
    let mut limited = r.reading("gone", 0, 0, Vec::new());
    limited.actions_cpu_limited = true;
    r.beat("gone", Some(limited));
    assert_eq!(r.s.load_counters().fallback_cpu_limited, 1);
    r.ms += 10 * 60 * 1_000;
    r.window("a", 1_000, GIB, &held);
    assert_eq!(r.s.load_counters().fallback_cpu_limited, 0);

    // Shadow mode charges nobody by measurement, so there is no fallback to count.
    let mut shadow = Rig::new(LoadConfig {
        mode: js_sched::PlacementLoad::Shadow,
        ..LoadConfig::default()
    });
    shadow.up("a", node());
    shadow.baseline("a");
    let mut limited = shadow.reading("a", 0, 0, Vec::new());
    limited.actions_cpu_limited = true;
    shadow.ms += W_MS;
    shadow.beat("a", Some(limited));
    assert_eq!(shadow.s.load_counters().fallback_cpu_limited, 0);
}

/// Catches: a reading out of order accepted (a counter going back would read as a huge
/// rise or a negative window), a dropped reading leaving the old baseline to compare the
/// next with, and a worker believed before two readings in a row: the dropped one sends
/// the worker to prediction, the next is a baseline only, the one after gives a window.
#[test]
fn a_reading_out_of_order_is_dropped_and_two_in_a_row_are_needed_again() {
    let (mut r, held) = measured(1_000, GIB);
    let again = r.reading("a", 1_000, GIB, held.clone());
    let mut old = again.clone();
    old.seq = 1;
    r.beat("a", Some(old));
    assert!(view(&r, "a").fallback);
    assert_eq!(r.s.load_counters().readings_dropped, 1);
    // The next is a baseline: no window yet.
    r.ms += W_MS;
    r.beat("a", Some(again));
    let v = view(&r, "a");
    assert!(v.fallback);
    assert_eq!(v.measured_cpu_millis, None);
    r.window("a", 1_000, GIB, &held);
    assert!(!view(&r, "a").fallback);
}

/// Catches: figures no worker could produce believed: more than four times the node's
/// cores in a window, or four times its memory in the working set, drop the reading
/// (exactly four times is taken). A bogus memory figure on the first reading is dropped
/// too.
#[test]
fn implausible_readings_are_dropped() {
    let (mut r, held) = measured(32_000, GIB);
    assert!(!view(&r, "a").fallback, "exactly four times the cores");
    r.window("a", 32_001, GIB, &held);
    assert!(view(&r, "a").fallback);
    assert_eq!(r.s.load_counters().readings_dropped, 1);

    let (mut r, held) = measured(1_000, 128 * GIB);
    assert!(!view(&r, "a").fallback, "exactly four times the memory");
    r.window("a", 1_000, 128 * GIB + 1, &held);
    assert!(view(&r, "a").fallback);

    let mut r = Rig::on();
    r.up("a", node());
    r.window("a", 0, 128 * GIB + 1, &[]);
    assert_eq!(
        r.s.load_counters(),
        js_sched::LoadCounters {
            readings_dropped: 1,
            ..js_sched::LoadCounters::default()
        }
    );
}

/// Catches: the readings of an earlier session carried into a new one (its `seq` starts
/// again, so every reading would read as out of order, or worse as a window against a
/// counter of another process): a new registration puts the worker back on prediction,
/// and its leases stay charged.
#[test]
fn a_new_session_forgets_the_readings_and_keeps_the_leases() {
    let (mut r, _) = measured(1_000, GIB);
    r.up("a", node());
    let v = view(&r, "a");
    assert!(v.fallback);
    assert_eq!(v.predicted_cpu_millis, 4_000);
    r.window("a", 1_000, GIB, &[]);
    r.window("a", 1_000, GIB, &[]);
    assert!(!view(&r, "a").fallback);
}

/// Catches: no cap on cold leases (a mass of unlearned keys admitted at once), a cap that
/// counts warm leases, and a window that never rolls over: an 88-core node admits
/// three cold leases per window (one per 32 cores, rounded up), none more in the same
/// window, three more in the next, and a warm lease is not counted.
#[test]
fn cold_leases_are_capped_per_worker_per_window() {
    let mut r = Rig::on();
    r.up("big", Resources::new(88_000, 512 * GIB));
    for _ in 0..10 {
        r.action(2_000, 4 * GIB, Some(cold(2_000, 4 * GIB)));
    }
    assert_eq!(r.tick().len(), 3);
    assert_eq!(view(&r, "big").cold_this_window, 3);
    let c = r.s.load_counters();
    assert_eq!((c.admitted_cold, c.cold_deferred), (3, 7));
    assert_eq!(c.blocked, 7);
    r.ms = W_MS - 1;
    assert!(r.tick().is_empty(), "still the first window");
    r.action(500, 4 * GIB, Some(warm(500, GIB)));
    assert_eq!(r.tick().len(), 1, "a warm lease is not capped");
    r.ms = W_MS;
    assert_eq!(r.tick().len(), 3, "the next window");
    assert_eq!(view(&r, "big").cold_this_window, 3);
    assert_eq!(r.s.load_counters().admitted_cold, 6);
}

/// Catches: a cold lease refused for the cap when the CPU is what refuses it, counted as
/// deferred (a head blocked by the cap would hold a worker for nothing): on a node full of
/// cold leases the next one is blocked on load, not deferred.
#[test]
fn a_cold_lease_refused_for_cpu_is_blocked_not_deferred() {
    let mut r = Rig::new(LoadConfig {
        cold_per_window: Some(100),
        ..LoadConfig::default()
    });
    r.up("a", node());
    for _ in 0..6 {
        r.action(2_000, 4 * GIB, Some(cold(2_000, 0)));
    }
    assert_eq!(r.tick().len(), 4, "8800 of 2000");
    let c = r.s.load_counters();
    assert_eq!((c.blocked, c.cold_deferred), (1, 0));
}

/// Catches: a cost not clamped to the worker, so that a key that wants the whole node
/// (predicted above the node, as a spiky key can be) never runs; the clamp must not lift
/// the limit for the lease beside it either.
#[test]
fn a_cost_above_the_node_is_clamped_to_it() {
    let mut r = Rig::on();
    r.up("a", node());
    r.action(8_000, 8 * GIB, Some(warm(128_000, 100 * GIB)));
    let starts = r.tick();
    assert_eq!(starts.len(), 1);
    assert_eq!(
        cost_of(&r, &starts[0]),
        Some(LeaseCost {
            cpu_millis: 8_000,
            memory_bytes: 32 * GIB,
            limit_bytes: 8 * GIB,
            cold: false,
            key_hash: 0,
        })
    );
    r.action(250, 8 * GIB, Some(warm(250, GIB)));
    assert!(r.tick().is_empty(), "no memory left beside it");
}

/// Catches: the three memory figures overloaded: the lease's cost keeps the prediction
/// (2 GiB), apart from the limit (4 GiB) the lease runs under; and a rerun up the memory
/// ladder is charged at the rung, 16 GiB for both, not at the learned 2.
#[test]
fn a_retry_up_the_ladder_is_charged_at_the_rung() {
    let mut r = Rig::on();
    r.up("a", Resources::new(8_000, 64 * GIB));
    r.action(1_000, 4 * GIB, Some(warm(1_000, 2 * GIB)));
    let first = r.tick().remove(0);
    assert_eq!(
        cost_of(&r, &first),
        Some(LeaseCost {
            cpu_millis: 1_000,
            memory_bytes: 2 * GIB,
            limit_bytes: 4 * GIB,
            cold: false,
            key_hash: 0,
        })
    );
    r.end(&first, Outcome::Failed(Failure::OutOfMemory));
    r.ms += 1_000;
    let second = r.tick().remove(0);
    assert_eq!(
        cost_of(&r, &second),
        Some(LeaseCost {
            cpu_millis: 1_000,
            memory_bytes: 16 * GIB,
            limit_bytes: 16 * GIB,
            cold: false,
            key_hash: 0,
        })
    );
    assert_eq!(r.s.lease_cost(first.lease), None, "the first lease is gone");
}

/// Catches: a VM's booked cost left out of the sums (it runs outside the measured
/// cgroup, so neither a reading nor a prediction would show it): beside a VM of 4 cores
/// and 16 GiB, 16 actions of 1 GiB fit and the 17th does not, on memory.
#[test]
fn a_vm_is_added_to_every_sum() {
    use js_types::sched::LeaseKind;
    let mut r = Rig::on();
    r.up_with(
        "a",
        node().with_vms(2),
        load_rig::caps("container").with_drivers(["container", "vm"]),
    );
    r.baseline("a");
    let mut vm = r.request(LeaseKind::Vm, 4_000, 16 * GIB);
    vm.resources = vm.resources.with_vms(1);
    r.submit(vm);
    assert_eq!(r.tick().len(), 1);
    let v = view(&r, "a");
    assert_eq!((v.vm_cpu_millis, v.vm_memory_bytes), (4_000, 16 * GIB));
    assert_eq!(v.left_cpu_millis, 4_000);
    assert_eq!(v.left_memory_bytes, 16 * GIB);
    assert_eq!(fill(&mut r, 20, warm(250, GIB)).len(), 16);
}

/// Catches: the lease cap dropped under the predicate: a worker capped at two leases takes
/// two however small their cost.
#[test]
fn the_lease_cap_still_holds() {
    let mut r = Rig::on();
    r.up("a", node().with_lease_cap(2));
    assert_eq!(fill(&mut r, 5, warm(250, 0)).len(), 2);
}

/// Catches: the CPU a request books no longer bounding what a worker could ever run: a
/// 64-core request is not placed on an 8-core worker because its prediction is small.
#[test]
fn a_request_larger_than_the_worker_still_waits() {
    let mut r = Rig::on();
    r.up("a", node());
    r.action(64_000, GIB, Some(warm(250, 0)));
    assert!(r.tick().is_empty());
    assert_eq!(r.s.queued_len(), 1);
}

/// Catches: an action at the whole host's memory sharing the node under the predicate: it
/// waits for an empty worker and then keeps the node alone.
#[test]
fn an_action_at_the_whole_hosts_memory_still_runs_alone() {
    let mut r = Rig::on();
    r.up("a", node());
    fill(&mut r, 1, warm(250, 0));
    r.action(1_000, 32 * GIB, Some(warm(250, 0)));
    assert!(r.tick().is_empty(), "a lease is on the worker");
}

/// Catches: an operation with no prediction charged by memory (a booked limit summed as if
/// it were a prediction) or charged nothing for cores: it is charged the cores it
/// books and no memory, which is what the booked rule did.
#[test]
fn a_request_without_a_prediction_is_charged_its_cores_and_no_memory() {
    let mut r = Rig::on();
    r.up("a", node());
    for _ in 0..4 {
        r.action(2_000, 16 * GIB, None);
    }
    let starts = r.tick();
    assert_eq!(starts.len(), 4, "16 GiB limits four times over 32 GiB");
    assert_eq!(
        cost_of(&r, &starts[0]),
        Some(LeaseCost {
            cpu_millis: 2_000,
            memory_bytes: 0,
            limit_bytes: 16 * GIB,
            cold: false,
            key_hash: 0,
        })
    );
    r.action(2_000, GIB, None);
    assert!(r.tick().is_empty(), "8000 and 2000 of 8800");
}

/// Catches: a VM lease counted as covered because a reading lists it (it runs outside the
/// measured cgroup, so no reading covers it; its booked cost stays in every sum and never
/// in the covered or uncovered ones).
#[test]
fn a_reading_never_covers_a_vm() {
    use js_types::sched::LeaseKind;
    let mut r = Rig::on();
    r.up_with(
        "a",
        node().with_vms(2),
        load_rig::caps("container").with_drivers(["container", "vm"]),
    );
    let mut vm = r.request(LeaseKind::Vm, 4_000, 16 * GIB);
    vm.resources = vm.resources.with_vms(1);
    r.submit(vm);
    let held = leases(&r.tick());
    assert_eq!(held.len(), 1);
    r.window("a", 0, 0, &held);
    r.window("a", 0, 0, &held);
    let v = view(&r, "a");
    assert!(!v.fallback);
    assert_eq!((v.vm_cpu_millis, v.vm_memory_bytes), (4_000, 16 * GIB));
    assert_eq!((v.predicted_cpu_millis, v.uncovered_cpu_millis), (0, 0));
    assert_eq!(v.left_cpu_millis, 4_000);
}

/// Catches: a plausibility check that compares against, or divides by, a capacity of 0: a
/// worker that reports no cores and no memory is taken at its word (there is nothing to
/// be implausible against), and a lease it holds is still charged by its prediction.
#[test]
fn a_worker_without_capacity_has_nothing_to_be_implausible_against() {
    let mut r = Rig::on();
    r.up("z", Resources::new(0, 0));
    r.window("z", 50_000, u64::MAX / 2, &[]);
    r.window("z", 50_000, u64::MAX / 2, &[]);
    let v = view(&r, "z");
    assert!(!v.fallback);
    assert_eq!(v.measured_cpu_millis, Some(50_000));
    let c = r.s.load_counters();
    assert_eq!((c.readings_accepted, c.readings_dropped), (2, 0));
}
