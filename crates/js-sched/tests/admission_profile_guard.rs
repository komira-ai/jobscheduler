//! The shape guard and the settings left to the owner, through the scheduler's public
//! inputs: when a key's curve is charged flat for eight runs, which ends of a run count,
//! the node guard on flat memory, the end of a new lease's charge at its expected life, the
//! the hold for a blocked head.
//!
//! The rig feeds readings as a worker's heartbeats; a window is 5 s of farm time.

#[path = "profile_rig/mod.rs"]
mod profile_rig;

use js_sched::{LoadConfig, PlacementLoad};
use js_types::sched::usage::key_hash;
use js_types::sched::{Failure, LeaseId, Outcome, StartLease, WorkKey};

use profile_rig::load_curves::Curves;
use profile_rig::load_rig::{self, GIB, Rig, cold, minute, w, warm};
use profile_rig::{HOUR, ahead, busy_at, cluster, flat_ws, node, place};

/// One run of `key` (predicted at 20 cores, so the curve is floored at 15): placed, and
/// the agent sends its minute 0 as having used `mean` millicores, then it ends as `end`.
fn run(r: &mut Rig, key: &str, mean: u32, end: Option<Outcome>) -> StartLease {
    let start = place(r, key, warm(20_000, 2 * GIB));
    let seen = [minute(start.lease, 0, mean, GIB)];
    r.window_clocked("a", 5_000, GIB, &[start.lease], &[(start.lease, 1)], &seen);
    match end {
        None => r.finish(&start),
        Some(outcome) => r.end(&start, outcome),
    }
    start
}

fn shape_key(key: &str) -> u64 {
    key_hash(&WorkKey::new("load", key))
}

/// A cluster whose key `g` has a curve of quiet minutes, busy at minute 2.
fn guarded() -> Rig {
    let (r, curves) = cluster(LoadConfig::default());
    curves.set("g", &flat_ws(&[10_000, 10_000, 70_000, 10_000], GIB), 5);
    r
}

/// Catches: the trigger of 2 misfit runs in 3 missing or off by one run (a quiet run
/// between two misfits still counts, a third is not needed), the CPU curve staying in
/// use after the demotion, the eight flat runs not counted off, and the curve not coming
/// back after them. A minute misfits when its mean is above twice the charge (15 cores).
#[test]
fn two_cpu_misfit_runs_in_three_demote_the_cpu_curve_for_eight_runs() {
    let mut r = guarded();
    let g = shape_key("g");
    run(&mut r, "g", 30_000, None);
    assert_eq!(
        r.s.load_counters().misfit_cpu_minutes,
        0,
        "twice the charge is not above it"
    );
    run(&mut r, "g", 30_001, None);
    assert_eq!(r.s.load_counters().misfit_cpu_minutes, 1);
    assert_eq!(r.s.shape_demoted_runs(g), (0, 0));
    run(&mut r, "g", 5_000, None);
    run(&mut r, "g", 31_000, None);
    let c = r.s.load_counters();
    assert_eq!(
        (c.demotions_cpu, c.misfit_cpu_minutes),
        (1, 2),
        "[miss, ok, miss]"
    );
    assert_eq!(r.s.shape_demoted_runs(g), (8, 0));
    // Charged flat now: the 70 cores of minute 2 are not charged, the flat 20 are.
    let flat = place(&mut r, "g", warm(20_000, 2 * GIB));
    assert_eq!(ahead(&r, 2).0, 20_000);
    // The 1 GiB working set last read, and the curve's 1 GiB (its flat cost is 2 GiB).
    assert_eq!(ahead(&r, 2).1, 2 * GIB, "memory is still by curve");
    let c = r.s.load_counters();
    assert_eq!((c.profile_cpu, c.profile_memory), (4, 5));
    r.finish(&flat);
    assert_eq!(r.s.shape_demoted_runs(g), (7, 0), "one flat run done");
    for left in (0..7).rev() {
        run(&mut r, "g", 5_000, None);
        assert_eq!(r.s.shape_demoted_runs(g), (left, 0));
    }
    place(&mut r, "g", warm(20_000, 2 * GIB));
    assert_eq!(ahead(&r, 2).0, 70_000, "the curve is back");
}

/// Catches: the second trigger (3 misfits in 10 runs) missing: a misfit every third run
/// demotes at the third, and a misfit every fifth run is never demoted (the documented
/// gap).
#[test]
fn a_misfit_every_third_run_demotes_and_every_fifth_does_not() {
    let mut r = guarded();
    let g = shape_key("g");
    let pattern = [true, false, false, true, false, false, true];
    for (i, misfit) in pattern.into_iter().enumerate() {
        assert_eq!(r.s.shape_demoted_runs(g), (0, 0), "run {i}");
        run(&mut r, "g", if misfit { 40_000 } else { 5_000 }, None);
    }
    assert_eq!(r.s.shape_demoted_runs(g), (8, 0));
    let mut r = guarded();
    for i in 0..20 {
        run(&mut r, "g", if i % 5 == 0 { 40_000 } else { 5_000 }, None);
    }
    assert_eq!(r.s.shape_demoted_runs(g), (0, 0));
    assert_eq!(r.s.load_counters().misfit_cpu_minutes, 4);
    assert_eq!(r.s.load_counters().demotions_cpu, 0);
}

/// Catches: the guard judging runs that did not run to their end (cancelled, lost, killed
/// by the node): two misfit runs ended that way and one that ended on its own are one
/// run; the second that ended on its own demotes.
#[test]
fn only_runs_that_end_on_their_own_or_at_their_limit_are_judged() {
    let mut r = guarded();
    let g = shape_key("g");
    // Killed by the node under pressure; lost; timed out: not judged.
    for outcome in [
        Outcome::Failed(Failure::NodeMemoryPressure),
        Outcome::Failed(Failure::Lost(js_types::sched::LostWhy::DaemonRestarted)),
        Outcome::Failed(Failure::Timeout),
    ] {
        let start = run(&mut r, "g", 40_000, Some(outcome));
        // A run that is run again ends here, not judged either way.
        for again in r.tick() {
            r.finish(&again);
        }
        assert_eq!(r.s.shape_demoted_runs(g), (0, 0), "{start:?}");
    }
    run(&mut r, "g", 40_000, None);
    assert_eq!(r.s.shape_demoted_runs(g), (0, 0), "one misfit that counts");
    run(&mut r, "g", 40_000, None);
    assert_eq!(r.s.shape_demoted_runs(g), (8, 0), "two");
    assert_eq!(r.s.load_counters().demotions_cpu, 1);
}

/// Catches: a run of a demoted key counted off its eight flat runs though it did not run
/// to its end (it was lost, or the node killed it), and a kill at its limit not counted
/// off: both resources.
#[test]
fn a_run_of_a_demoted_key_counts_off_only_if_it_ran() {
    let mut r = guarded();
    let g = shape_key("g");
    run(&mut r, "g", 40_000, None);
    run(&mut r, "g", 40_000, None);
    assert_eq!(r.s.shape_demoted_runs(g), (8, 0));
    let lost = Outcome::Failed(Failure::Lost(js_types::sched::LostWhy::Fenced));
    run(&mut r, "g", 5_000, Some(lost));
    assert_eq!(r.s.shape_demoted_runs(g), (8, 0), "lost");
    for again in r.tick() {
        r.finish(&again);
    }
    assert_eq!(r.s.shape_demoted_runs(g), (7, 0), "its run again ran");

    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("k", &flat_ws(&[10_000, 10_000], 2 * GIB), 5);
    let k = shape_key("k");
    for _ in 0..2 {
        let start = place(&mut r, "k", warm(20_000, 2 * GIB));
        r.end(&start, Outcome::Failed(Failure::OutOfMemory));
        for retry in r.tick() {
            r.finish(&retry);
        }
    }
    assert_eq!(r.s.shape_demoted_runs(k), (0, 8));
    let start = place(&mut r, "k", warm(20_000, 2 * GIB));
    r.end(&start, Outcome::Failed(Failure::NodeMemoryPressure));
    for retry in r.tick() {
        r.finish(&retry);
    }
    assert_eq!(r.s.shape_demoted_runs(k), (0, 8), "killed by the node");
    let start = place(&mut r, "k", warm(20_000, 2 * GIB));
    r.end(&start, Outcome::Failed(Failure::OutOfMemory));
    assert_eq!(r.s.shape_demoted_runs(k), (0, 7), "killed at its own limit");
}

/// Catches: a run that did not end on its own filling the window of the last runs with
/// clean ones: a misfit, two runs that timed out, a misfit are two misfits in three runs
/// that counted, for the CPU curve and for the memory curve.
#[test]
fn runs_that_did_not_end_are_left_out_of_the_window() {
    let timed_out = Outcome::Failed(Failure::Timeout);
    let mut r = guarded();
    let g = shape_key("g");
    run(&mut r, "g", 40_000, None);
    run(&mut r, "g", 40_000, Some(timed_out));
    run(&mut r, "g", 5_000, Some(timed_out));
    run(&mut r, "g", 5_000, Some(timed_out));
    assert_eq!(r.s.shape_demoted_runs(g), (0, 0));
    run(&mut r, "g", 40_000, None);
    assert_eq!(r.s.shape_demoted_runs(g), (8, 0), "CPU");

    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("w", &flat_ws(&[10_000, 10_000], 2 * GIB), 5);
    let w_key = shape_key("w");
    // A run whose minute 0 held `ws`, ended as `end`.
    let mut mem_run = |ws: u64, end: Option<Outcome>| {
        let start = place(&mut r, "w", warm(20_000, 2 * GIB));
        let seen = [minute(start.lease, 0, 1_000, ws)];
        r.window_clocked("a", 1_000, ws, &[start.lease], &[(start.lease, 1)], &seen);
        match end {
            None => r.finish(&start),
            Some(outcome) => r.end(&start, outcome),
        }
        r.s.shape_demoted_runs(w_key)
    };
    mem_run(3 * GIB, None);
    mem_run(3 * GIB, Some(timed_out));
    mem_run(GIB, Some(timed_out));
    assert_eq!(mem_run(GIB, Some(timed_out)), (0, 0));
    assert_eq!(mem_run(3 * GIB, None), (0, 8), "memory");
}

/// Catches: a run that was charged by curve before the key was demoted, and ends after it,
/// being judged (and so keeping the key's history for after its eight flat runs): three
/// leases of one size key run together, the first two end with a misfit and demote the key,
/// the third ends with a misfit too and counts for nothing; after eight flat runs one
/// misfit does not demote the key again.
#[test]
fn a_run_that_ends_after_the_demotion_is_not_judged() {
    let (mut r, curves) = cluster(LoadConfig::default());
    for key in ["s1", "s2", "s3"] {
        curves.set(key, &flat_ws(&[10_000, 10_000], GIB), 5);
    }
    let mut starts = Vec::new();
    for key in ["s1", "s2", "s3"] {
        let operation = r.s.next_operation();
        r.s.set_size_key(operation, 99);
        starts.push(place(&mut r, key, warm(20_000, 2 * GIB)));
    }
    let leases: Vec<LeaseId> = starts.iter().map(|s| s.lease).collect();
    let ages: Vec<(LeaseId, u32)> = leases.iter().map(|&l| (l, 1)).collect();
    let seen: Vec<_> = leases.iter().map(|&l| minute(l, 0, 40_000, GIB)).collect();
    r.window_clocked("a", 5_000, GIB, &leases, &ages, &seen);
    r.finish(&starts[0]);
    assert_eq!(r.s.shape_demoted_runs(99), (0, 0));
    r.finish(&starts[1]);
    assert_eq!(r.s.shape_demoted_runs(99), (8, 0));
    r.finish(&starts[2]);
    assert_eq!(
        r.s.shape_demoted_runs(99),
        (8, 0),
        "charged by curve before, says nothing"
    );
    for i in 0..8 {
        let operation = r.s.next_operation();
        r.s.set_size_key(operation, 99);
        let flat = place(&mut r, "s1", warm(20_000, 2 * GIB));
        r.finish(&flat);
        assert_eq!(r.s.shape_demoted_runs(99).0, 7 - i, "flat run {i}");
    }
    let operation = r.s.next_operation();
    r.s.set_size_key(operation, 99);
    let one = place(&mut r, "s1", warm(20_000, 2 * GIB));
    let seen = [minute(one.lease, 0, 40_000, GIB)];
    r.window_clocked("a", 5_000, GIB, &[one.lease], &[(one.lease, 1)], &seen);
    r.finish(&one);
    assert_eq!(
        r.s.shape_demoted_runs(99),
        (0, 0),
        "one misfit with a clean history"
    );
}

/// Catches: a lease with an expected life and no curve, alone on its worker, not charged
/// to its life (the worker holds nothing charged over time before it), and a lease with a
/// profile that charges nothing over time (its key demoted in CPU, the memory switch off)
/// treated as one that does: it is charged flat at every minute.
#[test]
fn a_lone_lease_is_charged_to_its_life_or_flat_when_its_profile_charges_nothing() {
    let ending = LoadConfig {
        expected_life_delta_minutes: Some(2),
        ..LoadConfig::default()
    };
    let (mut r, curves) = cluster(ending);
    curves.set_life("short", 3);
    place(&mut r, "short", warm(30_000, GIB));
    assert_eq!(ahead(&r, 4).0, 30_000);
    assert_eq!(ahead(&r, 5).0, 0, "3 minutes and the 2 of delta");

    let no_memory = LoadConfig {
        profile_memory: false,
        ..LoadConfig::default()
    };
    let (mut r, curves) = cluster(no_memory);
    curves.set("g", &flat_ws(&[10_000, 10_000, 70_000], GIB), 5);
    let g = shape_key("g");
    run(&mut r, "g", 40_000, None);
    run(&mut r, "g", 40_000, None);
    assert_eq!(r.s.shape_demoted_runs(g), (8, 0));
    place(&mut r, "g", warm(20_000, 2 * GIB));
    // Flat in both: 20 cores, and 2 GiB beside the 1 GiB working set the last reading saw.
    assert_eq!(ahead(&r, 2), (20_000, 3 * GIB));
    assert_eq!(ahead(&r, 31), (20_000, 3 * GIB));
}

/// Catches: a run that was lost for good (the farm's reruns used up) judged as a run of the
/// curve: after one misfit that counted, three lost runs of one operation with misfit
/// minutes do not demote the key, and the next one that ends on its own does.
#[test]
fn a_run_lost_for_good_is_not_judged() {
    let mut r = guarded();
    let g = shape_key("g");
    run(&mut r, "g", 40_000, None);
    let lost = Outcome::Failed(Failure::Lost(js_types::sched::LostWhy::Fenced));
    let mut start = place(&mut r, "g", warm(20_000, 2 * GIB));
    for round in 0..3 {
        let seen = [minute(start.lease, 0, 40_000, GIB)];
        r.window_clocked("a", 5_000, GIB, &[start.lease], &[(start.lease, 1)], &seen);
        r.end(&start, lost);
        if round < 2 {
            start = r.tick().remove(0);
        }
    }
    assert!(r.tick().is_empty(), "the operation failed with the third");
    assert_eq!(r.s.shape_demoted_runs(g), (0, 0));
    run(&mut r, "g", 40_000, None);
    assert_eq!(r.s.shape_demoted_runs(g), (8, 0));
}

/// Catches: a lease given back before its run started judged as a run: one misfit, two
/// leases given back, a misfit are two misfits in three runs.
#[test]
fn a_lease_given_back_before_it_started_is_not_a_run() {
    let mut r = guarded();
    let g = shape_key("g");
    run(&mut r, "g", 40_000, None);
    let first = place(&mut r, "g", warm(20_000, 2 * GIB));
    assert!(r.s.unplace(first.lease).is_some());
    let second = r.tick().remove(0);
    assert!(r.s.unplace(second.lease).is_some());
    let third = r.tick().remove(0);
    let seen = [minute(third.lease, 0, 40_000, GIB)];
    r.window_clocked("a", 5_000, GIB, &[third.lease], &[(third.lease, 1)], &seen);
    r.finish(&third);
    assert_eq!(r.s.shape_demoted_runs(g), (8, 0));
}

/// Catches: the flat-memory guard held on a lease that is not charged by a memory curve:
/// with the memory switch off and the guard's slack (25%) below the predicate's (50%), the
/// flat rule alone decides, three leases of 30 GiB (90 of 96) are admitted and the fourth
/// is not.
#[test]
fn the_flat_memory_guard_leaves_a_flat_charged_lease_to_the_flat_rule() {
    let config = LoadConfig {
        s_mem_permille: 500,
        profile_memory: false,
        flat_memory_slack_permille: Some(250),
        ..LoadConfig::default()
    };
    let (mut r, curves) = cluster(config);
    curves.set_all(&flat_ws(&[1_000], GIB), 5);
    for (i, want) in [1, 1, 1, 0].into_iter().enumerate() {
        r.keyed(&format!("p{i}"), 1_000, 8 * GIB, warm(1_000, 30 * GIB));
        assert_eq!(r.tick().len(), want, "lease {i}");
    }
}

/// Catches: a kill at the lease's own limit not being a misfit of the memory curve, the
/// retry up the ladder judged as a run of the curve, and the memory demotion not leaving
/// the CPU curve alone: two kills in a row demote the memory curve, and each retry is
/// charged flat at its rung.
#[test]
fn a_kill_at_the_leases_own_limit_is_a_memory_misfit() {
    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("k", &flat_ws(&[10_000, 10_000], 2 * GIB), 5);
    let k = shape_key("k");
    for kill in 0..2 {
        let start = place(&mut r, "k", warm(20_000, 2 * GIB));
        r.end(&start, Outcome::Failed(Failure::OutOfMemory));
        let retry = r.tick();
        assert_eq!(retry.len(), 1, "run again one rung up");
        let cost = r.s.lease_cost(retry[0].lease).expect("held");
        // The key floor of the first kill starts the second run at 16 GiB.
        let rung = if kill == 0 { 16 * GIB } else { 32 * GIB };
        assert_eq!(cost.memory_bytes, rung, "charged flat at the rung");
        assert_eq!(ahead(&r, 5).1, rung);
        r.finish(&retry[0]);
        let want = if kill == 0 { (0, 0) } else { (0, 8) };
        assert_eq!(r.s.shape_demoted_runs(k), want, "kill {kill}");
    }
    let c = r.s.load_counters();
    assert_eq!((c.demotions_memory, c.demotions_cpu), (1, 0));
    assert_eq!(
        (c.profile_cpu, c.profile_memory),
        (2, 2),
        "the retries were flat"
    );
    place(&mut r, "k", warm(20_000, 2 * GIB));
    assert_eq!(
        ahead(&r, 5).1,
        2 * GIB,
        "charged flat, which is its flat cost"
    );
    let c = r.s.load_counters();
    assert_eq!((c.profile_cpu, c.profile_memory), (3, 2));
}

/// Catches: a memory minute above the curve not marking the run, and the run's end on its
/// own not recording it: two clean runs with a working set above the remaining maximum
/// demote the memory curve.
#[test]
fn a_working_set_above_the_curve_marks_the_run() {
    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("w", &flat_ws(&[10_000, 10_000], 2 * GIB), 5);
    for _ in 0..2 {
        let start = place(&mut r, "w", warm(20_000, 2 * GIB));
        let seen = [minute(start.lease, 0, 1_000, 3 * GIB)];
        r.window_clocked(
            "a",
            5_000,
            3 * GIB,
            &[start.lease],
            &[(start.lease, 1)],
            &seen,
        );
        r.finish(&start);
    }
    assert_eq!(r.s.shape_demoted_runs(shape_key("w")), (0, 8));
    let c = r.s.load_counters();
    assert_eq!((c.misfit_memory_minutes, c.demotions_memory), (2, 1));
}

/// Catches: runs of one learned key judged apart because their requests differ (the
/// request key carries the action digest, the size key names the learned key): two keys
/// under one size key demote together, and the demotion is under the size key.
#[test]
fn the_size_key_is_the_key_the_guard_judges_under() {
    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("s1", &flat_ws(&[10_000, 10_000, 70_000], GIB), 5);
    curves.set("s2", &flat_ws(&[10_000, 10_000, 70_000], GIB), 5);
    for key in ["s1", "s2"] {
        let operation = r.s.next_operation();
        r.s.set_size_key(operation, 99);
        run(&mut r, key, 40_000, None);
    }
    assert_eq!(r.s.shape_demoted_runs(99), (8, 0));
    assert_eq!(r.s.shape_demoted_runs(shape_key("s1")), (0, 0));
    assert_eq!(r.s.shape_demoted_runs(shape_key("s2")), (0, 0));
}

/// Catches: the flat-memory guard missing, applying to a lease that is not charged by a
/// memory curve, leaving out the cold or demoted leases or the VMs on the worker, or
/// applying with no setting: with `d` = 0.25 on 64 GiB the flat costs of the leases and
/// the new one stay within 80 GiB, though the curves ask for 1 GiB each.
#[test]
fn the_flat_memory_guard_bounds_the_sum_of_flat_costs() {
    let on = LoadConfig {
        flat_memory_slack_permille: Some(250),
        ..LoadConfig::default()
    };
    let flat_30 = |r: &mut Rig, key: &str| {
        r.keyed(key, 1_000, 8 * GIB, warm(1_000, 30 * GIB));
        r.tick().len()
    };
    let (mut r, curves) = cluster(on);
    curves.set_all(&flat_ws(&[1_000], GIB), 5);
    assert_eq!(flat_30(&mut r, "p1"), 1);
    assert_eq!(flat_30(&mut r, "p2"), 1);
    assert_eq!(flat_30(&mut r, "p3"), 0, "30 + 30 + 30 > 80");
    // Off: the curves alone decide.
    let off = LoadConfig {
        flat_memory_slack_permille: None,
        ..LoadConfig::default()
    };
    let (mut r, curves) = cluster(off);
    curves.set_all(&flat_ws(&[1_000], GIB), 5);
    assert_eq!(
        [
            flat_30(&mut r, "p1"),
            flat_30(&mut r, "p2"),
            flat_30(&mut r, "p3")
        ],
        [1; 3]
    );
    // A cold lease of 30 is charged flat and is not held to the guard (60 + 30 > 80 is
    // fine for it)...
    let (mut r, curves) = cluster(on);
    curves.set_all(&flat_ws(&[1_000], GIB), 5);
    assert_eq!(flat_30(&mut r, "p1"), 1);
    r.keyed("c1", 1_000, 8 * GIB, cold(1_000, 30 * GIB));
    assert_eq!(r.tick().len(), 1);
    // ...but it counts in the sum a lease charged by curve is held to: 30 + 30 + 30 > 80.
    assert_eq!(flat_30(&mut r, "p2"), 0);
    // The same for a VM that books 40 GiB: 40 + 30 fits, 40 + 30 + 30 does not.
    let curves = Curves::new("load");
    let mut r = Rig::with_curves(on, curves.clone());
    r.up_with(
        "a",
        node().with_vms(2),
        load_rig::caps("container").with_drivers(["container", "vm"]),
    );
    r.set_clock("a", HOUR);
    r.baseline("a");
    r.window("a", 0, 0, &[]);
    curves.set_all(&flat_ws(&[1_000], GIB), 5);
    let mut vm = r.request(js_types::sched::LeaseKind::Vm, 4_000, 40 * GIB);
    vm.resources = vm.resources.with_vms(1);
    r.submit(vm);
    assert_eq!(r.tick().len(), 1);
    assert_eq!(flat_30(&mut r, "p1"), 1);
    assert_eq!(flat_30(&mut r, "p2"), 0);
}

/// Catches: the flat-memory guard off by default, or reading its limit as exclusive or
/// with a slack: with `d` 0 on 64 GiB, a lease charged by a memory curve of 1 GiB is
/// admitted beside a flat 30 GiB when its own flat cost is 34 GiB (64 GiB of flat costs
/// in all) and refused when it is one byte more; with the guard off the same lease runs.
#[test]
fn the_flat_memory_guard_is_on_at_zero_slack_by_default() {
    let second = |config: LoadConfig, flat: u64| {
        let (mut r, curves) = cluster(config);
        curves.set_all(&flat_ws(&[1_000], GIB), 5);
        r.keyed("p1", 1_000, 8 * GIB, warm(1_000, 30 * GIB));
        assert_eq!(r.tick().len(), 1);
        r.keyed("p2", 1_000, 8 * GIB, warm(1_000, flat));
        r.tick().len()
    };
    assert_eq!(second(LoadConfig::default(), 34 * GIB), 1, "exactly C_mem");
    assert_eq!(
        second(LoadConfig::default(), 34 * GIB + 1),
        0,
        "a byte over"
    );
    let off = LoadConfig {
        flat_memory_slack_permille: None,
        ..LoadConfig::default()
    };
    assert_eq!(second(off, 34 * GIB + 1), 1, "the guard alone refused it");
}

/// Catches: the ending of a new lease's charge at its expected life missing or applied
/// when off or when the key's life is not known: a 30-core lease of life 3 + 2 minutes is
/// admitted beside a lease that is busy at 90 cores at minute 10, and refused with the
/// setting off or with no life known. The present minute stays charged, and a lease
/// already on the worker stops being charged at its own life.
#[test]
fn a_charge_can_end_at_the_expected_life() {
    // The default: 2 minutes past the key's P90 life.
    let ending = LoadConfig::default();
    let off = LoadConfig {
        expected_life_delta_minutes: None,
        ..LoadConfig::default()
    };
    let try_it = |config: LoadConfig, life: Option<u32>| {
        let (mut r, curves) = cluster(config);
        curves.set("first", &busy_at(10, 20), 5);
        let first = place(&mut r, "first", warm(20_000, GIB));
        r.window_clocked("a", 5_000, GIB, &[first.lease], &[(first.lease, 0)], &[]);
        if let Some(life) = life {
            curves.set_life("short", life);
        }
        r.keyed("short", 1_000, 8 * GIB, warm(30_000, GIB));
        (r.tick().len(), r)
    };
    let (placed, r) = try_it(ending, Some(3));
    assert_eq!(placed, 1, "gone by minute 5: 90 at minute 10 is alone");
    assert_eq!(
        ahead(&r, 4).0,
        15_000 + 30_000,
        "charged for its life, 3 + 2 minutes"
    );
    assert_eq!(ahead(&r, 5).0, 15_000);
    assert_eq!(ahead(&r, 10).0, 90_000);
    assert_eq!(ahead(&r, 31).0, 15_000, "the H+ slot");
    assert_eq!(ahead(&r, 4).1, 2 * GIB);
    assert_eq!(ahead(&r, 5).1, GIB, "its memory ends with it");
    assert_eq!(try_it(off, Some(3)).0, 0, "off: 90 + 30 > 110");
    assert_eq!(
        try_it(ending, None).0,
        0,
        "no life known: charged as before"
    );
    // Covered and older than its life: the present minute is still charged, the next not.
    let (_, mut r) = try_it(ending, Some(3));
    let leases = r.held("a");
    let ages: Vec<(LeaseId, u32)> = leases.iter().map(|&l| (l, 6)).collect();
    r.window_clocked("a", 5_000, GIB, &leases, &ages, &[]);
    // The first lease is at minute 6 of its curve (quiet, floored); the short one is
    // over its life: 30 cores (the covered floor of 30) now, none after.
    assert_eq!(ahead(&r, 0).0, 15_000 + 30_000);
    assert_eq!(ahead(&r, 1).0, 15_000);
    assert_eq!(ahead(&r, 4).0, 90_000, "minute 10 of the first lease");
}

/// Catches: a charge ended at the expected life for a lease that is charged by a curve
/// not working with the curve (both lanes), and a retry or a lease with no prediction
/// ended at a life: a curve lease busy at minute 10 and of life 3 + 2 minutes is admitted
/// beside one busy at the same minute.
#[test]
fn an_expected_life_ends_a_curve_too() {
    let ending = LoadConfig {
        expected_life_delta_minutes: Some(2),
        ..LoadConfig::default()
    };
    let (mut r, curves) = cluster(ending);
    curves.set("first", &busy_at(10, 20), 5);
    curves.set("early", &busy_at(10, 20), 5);
    curves.set_life("early", 3);
    let first = place(&mut r, "first", warm(20_000, GIB));
    r.window_clocked("a", 5_000, GIB, &[first.lease], &[(first.lease, 0)], &[]);
    r.keyed("early", 1_000, 8 * GIB, warm(20_000, GIB));
    assert_eq!(r.tick().len(), 1, "90 + 90 at minute 10 but one has ended");
    assert_eq!(ahead(&r, 10).0, 90_000);
}

/// Catches: a scheduler on the booked rule reading curves: with the load rule off the
/// leases are charged flat, whatever the source knows.
#[test]
fn the_booked_rule_reads_no_curve() {
    let off = LoadConfig {
        mode: PlacementLoad::Off,
        ..LoadConfig::default()
    };
    let (mut r, curves) = cluster(off);
    curves.set("p", &flat_ws(&[10_000, 90_000], GIB), 5);
    place(&mut r, "p", warm(20_000, 2 * GIB));
    assert_eq!(ahead(&r, 1), (20_000, 2 * GIB));
}

/// Catches: the hold for a blocked head ranking workers by the flat cost of the head when
/// it is charged by a curve: the binding ratio uses the head's charge at the present
/// minute. Worker `a` holds 60 cores and 40 GiB, `b` 20 cores and 55 GiB; the head (a
/// curve of 100 cores at minute 3, so no worker has room) is predicted at 56 cores and
/// 20 GiB. By its flat cost `b` is the fuller (memory 1.17 against 1.16 of cores), by its
/// curve (42 cores, 1 GiB now) `a` is.
#[test]
fn a_blocked_head_holds_the_worker_by_its_charge_now() {
    let hold = |profile: bool| {
        let config = LoadConfig {
            profile,
            profile_memory: profile,
            ..LoadConfig::default()
        };
        let curves = Curves::new("load");
        let mut r = Rig::with_curves(config, curves.clone());
        for name in ["a", "b"] {
            r.up(name, node());
            r.set_clock(name, HOUR);
            r.baseline(name);
            r.window(name, 0, 0, &[]);
        }
        r.keyed("ha", 1_000, 8 * GIB, warm(60_000, 40 * GIB));
        r.keyed("hb", 1_000, 8 * GIB, warm(20_000, 55 * GIB));
        assert_eq!(r.tick().len(), 2);
        assert_eq!(r.held("a").len(), 1);
        curves.set("head", &flat_ws(&[10_000, 10_000, 10_000, 100_000], GIB), 5);
        let head = r.keyed("head", 1_000, 8 * GIB, warm(56_000, 20 * GIB));
        assert!(r.tick().is_empty());
        r.s.head_hold(head).cloned()
    };
    assert_eq!(hold(false), Some(w("a")), "flat: a 1.16 against b 1.17");
    assert_eq!(
        hold(true),
        Some(w("b")),
        "by the charge now: a 1.02 against b 0.88"
    );
}

/// Catches: the hold for a blocked head ranking workers by the memory its curve charges at
/// minute 1 and not at the present minute. Worker `a` holds 60 cores and 40 GiB, `b` 20
/// cores and 55 GiB; the head (a curve of 30 cores and 38 GiB in minute 0, 1 GiB after, and
/// 100 cores at minute 3, so `a` is refused on CPU and `b` on memory) is predicted at 30
/// cores and 38 GiB. By its charge now `a` is the emptier (1.22 against 1.45); by its
/// memory a minute on (1 GiB) `b` would be (0.90 against 0.88).
#[test]
fn a_blocked_head_holds_the_worker_by_its_memory_now() {
    let curves = Curves::new("load");
    let mut r = Rig::with_curves(LoadConfig::default(), curves.clone());
    for name in ["a", "b"] {
        r.up(name, node());
        r.set_clock(name, HOUR);
        r.baseline(name);
        r.window(name, 0, 0, &[]);
    }
    r.keyed("ha", 1_000, GIB, warm(60_000, 40 * GIB));
    r.keyed("hb", 1_000, GIB, warm(20_000, 55 * GIB));
    assert_eq!(r.tick().len(), 2);
    assert_eq!(r.held("a").len(), 1);
    let points = [
        (30_000, 38 * GIB),
        (30_000, GIB),
        (30_000, GIB),
        (100_000, GIB),
    ];
    curves.set("head", &points, 5);
    let head = r.keyed("head", 1_000, GIB, warm(30_000, 38 * GIB));
    assert!(r.tick().is_empty());
    assert_eq!(r.s.head_hold(head).cloned(), Some(w("a")));
}

/// Catches: the hold for a blocked head ranking workers by the cores its charge asks for a
/// minute on and not now. The head is charged for one minute only (its key's wall minutes
/// plus nothing is 1), so at minute 1 it asks for nothing. Worker `a` holds 45 cores and
/// 29 GiB, `b` 70 cores and 10 GiB (the two do not fit together); the head (60 cores,
/// 38 GiB) is refused by `a` on memory and by `b` on CPU. Charged now `a` is the emptier
/// (1.05 against 1.30); charged no cores a minute on, `b` would be (0.75 against 1.05).
#[test]
fn a_blocked_head_holds_the_worker_by_its_cores_now() {
    let config = LoadConfig {
        expected_life_delta_minutes: Some(0),
        ..LoadConfig::default()
    };
    let curves = Curves::new("load");
    let mut r = Rig::with_curves(config, curves.clone());
    for name in ["a", "b"] {
        r.up(name, node());
        r.set_clock(name, HOUR);
        r.baseline(name);
        r.window(name, 0, 0, &[]);
    }
    r.keyed("ha", 1_000, GIB, warm(45_000, 29 * GIB));
    r.keyed("hb", 1_000, GIB, warm(70_000, 10 * GIB));
    assert_eq!(r.tick().len(), 2);
    assert_eq!(r.held("a").len(), 1, "45 + 70 cores is over the limit");
    curves.set_life("head", 1);
    let head = r.keyed("head", 1_000, GIB, warm(60_000, 38 * GIB));
    assert!(r.tick().is_empty());
    assert_eq!(r.s.head_hold(head).cloned(), Some(w("a")));
}
