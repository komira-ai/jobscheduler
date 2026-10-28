//! Admission by per-minute profiles through the scheduler's public inputs, one rule each:
//! how a lease with a learned curve is charged over the next minutes, what the predicate
//! checks at each of them and at the horizon, which leases stay flat, and what the shape
//! guard and the two owner-decision settings do.
//!
//! The rig feeds readings as a worker's heartbeats; a window is 5 s of farm time, and a
//! reading can carry the start of each lease and its closed minutes.

//! Admission by per-minute profiles through the scheduler's public inputs, one rule each:
//! how a lease with a learned curve is charged over the next minutes, what the predicate
//! checks at each of them and at the horizon, and which leases stay flat. The shape guard
//! and the settings left to the owner are in `admission_profile_guard.rs`.
//!
//! The rig feeds readings as a worker's heartbeats; a window is 5 s of farm time, and a
//! reading can carry the start of each lease and its closed minutes.

#[path = "profile_rig/mod.rs"]
mod profile_rig;

use js_sched::{HORIZON_MINUTES, LoadConfig};
use js_types::sched::{Failure, LeaseId, Outcome, Resources, WorkKey};

use profile_rig::load_curves::Curves;
use profile_rig::load_rig::{self, GIB, Rig, cold, minute, w, warm};
use profile_rig::{
    HOUR, ahead, busy_at, busy_pair, cluster, cluster_on, flat_ws, holding_busy_at, node, place,
};

/// Catches: a curve not read at the minute ahead, the floor `g * p_bar` missing, the
/// in-flight rule missing (a lease no reading covers is counted at its quiet start), the
/// memory taken as the point and not the remaining maximum, and the `H+` slot and the tail
/// not holding the last point.
#[test]
fn a_lease_with_a_curve_is_charged_per_minute() {
    let (mut r, curves) = cluster(LoadConfig::default());
    let ws = [GIB, 4 * GIB, 4 * GIB, GIB];
    let points: Vec<(u32, u64)> = [10_000, 10_000, 60_000, 10_000]
        .into_iter()
        .zip(ws)
        .collect();
    curves.set("p", &points, 5);
    let start = place(&mut r, "p", warm(20_000, 2 * GIB));
    // Uncovered: the first two minutes are the larger of minutes 0 and 1, floored at
    // 0.75 * 20 cores = 15 cores.
    assert_eq!(ahead(&r, 0), (15_000, 4 * GIB));
    assert_eq!(ahead(&r, 1), (15_000, 4 * GIB));
    assert_eq!(ahead(&r, 2), (60_000, 4 * GIB));
    assert_eq!(ahead(&r, 3), (15_000, GIB), "the peak is behind");
    assert_eq!(ahead(&r, 4), (15_000, GIB), "the tail is the last point");
    assert_eq!(ahead(&r, HORIZON_MINUTES), (15_000, GIB));
    assert_eq!(ahead(&r, HORIZON_MINUTES + 1), (15_000, GIB), "the H+ slot");
    assert_eq!(ahead(&r, 500), ahead(&r, HORIZON_MINUTES + 1));
    // Covered, its own clock says 1 minute: the curve is read from minute 1.
    r.window_clocked("a", 5_000, GIB, &[start.lease], &[(start.lease, 1)], &[]);
    assert_eq!(
        ahead(&r, 0),
        (15_000, 4 * GIB),
        "covered: minute 1, no in-flight rule"
    );
    assert_eq!(ahead(&r, 1), (60_000, 4 * GIB));
    assert_eq!(ahead(&r, 2), (15_000, GIB));
    let c = r.s.load_counters();
    assert_eq!((c.profile_cpu, c.profile_memory, c.curve_flat), (1, 1, 0));
}

/// Catches: a covered lease whose agent gave it no clock read at age 0 (its busy phase
/// would be charged at minute 0 for good) instead of charged flat, and a lease with a
/// clock charged flat.
#[test]
fn a_covered_lease_without_a_clock_is_charged_flat() {
    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("p", &flat_ws(&[10_000, 10_000, 60_000, 10_000], GIB), 5);
    let start = place(&mut r, "p", warm(20_000, 2 * GIB));
    r.window("a", 5_000, GIB, &[start.lease]);
    // Covered flat: f * 20 cores at every minute; memory its flat 2 GiB.
    for m in [0, 1, 2, 3, 40] {
        assert_eq!(ahead(&r, m), (15_000, 2 * GIB), "minute {m}");
    }
    r.window_clocked("a", 5_000, GIB, &[start.lease], &[(start.lease, 0)], &[]);
    assert_eq!(ahead(&r, 2), (60_000, GIB));
}

/// Catches: the measurement entering at later minutes (the covered lease idles by its
/// curve at 15 cores while the node measures 70): it raises the figure at minute 0 only,
/// and a lease that fits the curves at every later minute is refused at minute 0 by it
/// and by nothing else.
#[test]
fn the_measurement_enters_at_minute_zero_only() {
    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("q", &flat_ws(&[10_000], GIB), 5);
    let q = place(&mut r, "q", warm(20_000, 2 * GIB));
    r.window_clocked("a", 70_000, 3 * GIB, &[q.lease], &[(q.lease, 0)], &[]);
    assert_eq!(ahead(&r, 0).0, 70_000);
    assert_eq!(ahead(&r, 1).0, 15_000);
    assert_eq!(ahead(&r, 31).0, 15_000);
    // 70 + 45 = 115 > 110 at minute 0; 15 + 45 = 60 everywhere else.
    r.keyed("big", 1_000, 8 * GIB, warm(45_000, GIB));
    assert!(r.tick().is_empty(), "refused by the measurement");
    r.window_clocked("a", 40_000, 3 * GIB, &[q.lease], &[(q.lease, 0)], &[]);
    r.window_clocked("a", 40_000, 3 * GIB, &[q.lease], &[(q.lease, 0)], &[]);
    assert_eq!(
        r.tick().len(),
        1,
        "40 + 45 = 85 fits once the measurement falls"
    );
}

/// Catches: the predicate checking minute 0 only (two leases busy at the same later
/// minute admitted), and checking only the peak (two leases busy at different minutes
/// refused): the second lease is refused while it overlaps the first one's busy minute 5,
/// and admitted when its own busy minute is 10.
#[test]
fn the_predicate_is_checked_at_every_minute_of_the_horizon() {
    let (mut r, curves) = holding_busy_at(5, 14);
    curves.set("same", &busy_at(5, 14), 5);
    // Minute 5: 90 + 90. Everywhere else 15 (the floor) or 90 beside 15: within 110.
    r.keyed("same", 1_000, 8 * GIB, warm(20_000, GIB));
    assert!(r.tick().is_empty());
    assert_eq!(r.s.load_counters().blocked, 1);
    let (mut r, curves) = holding_busy_at(5, 14);
    curves.set("later", &busy_at(10, 14), 5);
    r.keyed("later", 1_000, 8 * GIB, warm(20_000, GIB));
    assert_eq!(
        r.tick().len(),
        1,
        "busy at minute 10: 15 + 90, then 90 + 15"
    );
}

/// Catches: a busy phase beyond the horizon not seen (the `H+` check missing): two leases
/// busy at minute 40 are not admitted together, while one busy at minute 10 is admitted
/// beside one busy at minute 40; and minute 30 of the charge read as the `H+` slot.
#[test]
fn the_h_plus_check_sees_a_busy_phase_beyond_the_horizon() {
    let (mut r, curves) = holding_busy_at(40, 60);
    curves.set("same", &busy_at(40, 60), 5);
    assert_eq!(ahead(&r, 10).0, 15_000, "nothing busy inside the horizon");
    assert_eq!(ahead(&r, 31).0, 90_000, "the H+ slot holds the later peak");
    assert_eq!(
        ahead(&r, HORIZON_MINUTES).0,
        15_000,
        "minute 30 is slot 30, not the H+ slot"
    );
    r.keyed("same", 1_000, 8 * GIB, warm(20_000, GIB));
    assert!(r.tick().is_empty(), "90 + 90 from minute 30 on");
    let (mut r, curves) = holding_busy_at(40, 60);
    curves.set("early", &busy_at(10, 60), 5);
    r.keyed("early", 1_000, 8 * GIB, warm(20_000, GIB));
    assert_eq!(r.tick().len(), 1, "its own tail is quiet: 90 + 15");
}

/// Catches: a new lease's memory judged at its minute 1 and not its minute 0: a curve that
/// peaks at 10 GiB in minute 0 alone is refused beside 8 GiB held on a 15 GiB worker (at
/// minute 1 it would ask for 1 GiB and fit).
#[test]
fn the_new_leases_memory_is_judged_at_its_first_minute() {
    let (mut r, curves) = cluster_on(LoadConfig::default(), Resources::new(100_000, 15 * GIB));
    r.keyed("held", 1_000, GIB, warm(1_000, 8 * GIB));
    assert_eq!(r.tick().len(), 1);
    curves.set("peak", &[(1_000, 10 * GIB), (1_000, GIB), (1_000, GIB)], 5);
    r.keyed("peak", 1_000, GIB, warm(1_000, 10 * GIB));
    assert!(r.tick().is_empty(), "8 + 10 > 15 at minute 0");
    assert_eq!(r.s.load_counters().blocked, 1);
}

/// Catches: a lease older than the curve charged from minute 0 again, an age that is not
/// whole minutes, and the clock read from the wrong reading: the same curve at ages 0, 1
/// and 3 minutes (a clock of 59.99 s is 0 minutes).
#[test]
fn a_leases_age_is_whole_minutes_of_the_agents_clock() {
    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("p", &flat_ws(&[10_000, 90_000, 10_000, 10_000], GIB), 5);
    let p = place(&mut r, "p", warm(40_000, GIB));
    let show = |r: &mut Rig, age_usec: u64| {
        // A clock `age_usec` before the reading.
        let mut load = r.reading("a", 5_000, GIB, vec![p.lease]);
        load.lease_clocks = vec![js_types::sched::usage::LeaseClock {
            lease: p.lease,
            started_mono_usec: load.mono_usec - age_usec,
        }];
        r.ms += load_rig::W_MS;
        r.beat("a", Some(load));
        ahead(r, 0).0
    };
    assert_eq!(show(&mut r, 59_999_999), 30_000, "minute 0, floored");
    assert_eq!(show(&mut r, 60_000_000), 90_000, "minute 1");
    assert_eq!(show(&mut r, 119_999_999), 90_000, "still minute 1");
    assert_eq!(show(&mut r, 180_000_000), 30_000, "minute 3: the tail");
    assert_eq!(show(&mut r, 3_000_000_000), 30_000, "far past the curve");
}

/// Catches: a lease with a memory curve admitted beside another while the first one's
/// peak is ahead of it, and refused after it is behind it: two leases peaking at 8 GiB on
/// a 15 GiB worker do not fit together until the first has passed its peak (minute 3).
#[test]
fn memory_is_held_until_the_peak_is_behind_the_lease() {
    let (mut r, curves) = cluster_on(LoadConfig::default(), Resources::new(100_000, 15 * GIB));
    let points: Vec<(u32, u64)> = [2, 8, 8, 1].iter().map(|&g| (1_000, g * GIB)).collect();
    curves.set("m1", &points, 5);
    curves.set("m2", &points, 5);
    let first = place(&mut r, "m1", warm(1_000, 8 * GIB));
    r.window_clocked(
        "a",
        1_000,
        2 * GIB,
        &[first.lease],
        &[(first.lease, 0)],
        &[],
    );
    r.keyed("m2", 1_000, 8 * GIB, warm(1_000, 8 * GIB));
    assert!(r.tick().is_empty(), "8 + 8 > 15 while the peak is ahead");
    r.window_clocked(
        "a",
        1_000,
        8 * GIB,
        &[first.lease],
        &[(first.lease, 2)],
        &[],
    );
    assert!(r.tick().is_empty(), "minute 2 still has the peak ahead");
    r.window_clocked("a", 1_000, GIB, &[first.lease], &[(first.lease, 3)], &[]);
    assert_eq!(r.tick().len(), 1, "1 + 8 fits once the peak is behind");
}

/// Catches: a lease past its curve charged the curve (its working set is above what the
/// curve asks for from its age on, and the next lease is admitted against the curve): the
/// lease is charged what it uses, for every minute ahead, and the minute that misfit is
/// counted once however often the agent sends it.
#[test]
fn a_lease_above_its_curve_is_charged_its_working_set() {
    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("over", &flat_ws(&[1_000], 4 * GIB), 5);
    curves.set("p", &flat_ws(&[1_000], 8 * GIB), 5);
    curves.set("n", &flat_ws(&[1_000], 48 * GIB), 5);
    let over = place(&mut r, "over", warm(1_000, 4 * GIB));
    let p = place(&mut r, "p", warm(1_000, 8 * GIB));
    let ages = [(over.lease, 1), (p.lease, 1)];
    let covered = [over.lease, p.lease];
    r.window_clocked("a", 2_000, 5 * GIB, &covered, &ages, &[]);
    assert_eq!(ahead(&r, 5).1, 12 * GIB, "4 + 8 by the curves");
    // The minute that has just closed: `over` used 9 GiB against a curve of 4.
    let seen = [
        minute(over.lease, 0, 1_000, 9 * GIB),
        minute(p.lease, 0, 1_000, GIB),
    ];
    r.window_clocked("a", 2_000, 10 * GIB, &covered, &ages, &seen);
    assert_eq!(ahead(&r, 5).1, 17 * GIB, "9 held + 8");
    assert_eq!(ahead(&r, 0).1, 17 * GIB);
    // The next minute is back within the curve: the lease is charged the curve again.
    let calm = [
        minute(over.lease, 1, 1_000, 4 * GIB),
        minute(p.lease, 1, 1_000, GIB),
    ];
    r.window_clocked(
        "a",
        2_000,
        5 * GIB,
        &covered,
        &[(over.lease, 2), (p.lease, 2)],
        &calm,
    );
    assert_eq!(ahead(&r, 5).1, 12 * GIB, "4 + 8 again");
    let again = [
        minute(over.lease, 2, 1_000, 9 * GIB),
        minute(p.lease, 2, 1_000, GIB),
    ];
    r.window_clocked(
        "a",
        2_000,
        10 * GIB,
        &covered,
        &[(over.lease, 3), (p.lease, 3)],
        &again,
    );
    assert_eq!(ahead(&r, 5).1, 17 * GIB, "above it again");
    // The same minute again, and an older one, change nothing and are not counted again.
    let old = [
        minute(over.lease, 0, 1_000, 9 * GIB),
        minute(over.lease, 2, 1_000, 9 * GIB),
    ];
    r.window_clocked(
        "a",
        2_000,
        10 * GIB,
        &covered,
        &[(over.lease, 3), (p.lease, 3)],
        &old,
    );
    assert_eq!(r.s.load_counters().misfit_memory_minutes, 2);
    r.keyed("n", 1_000, 8 * GIB, warm(1_000, 48 * GIB));
    assert!(r.tick().is_empty(), "17 + 48 > 64, where 12 + 48 would fit");
}

/// Catches: a lease whose curve asks for more than the node has refused for good (an action
/// of the whole node must run on an otherwise idle one): a curve of 150 cores at minute 3
/// and 80 GiB on a 100-core worker with 64 GiB is charged what the worker has and placed.
#[test]
fn a_curve_larger_than_the_node_is_clamped_to_it() {
    let (mut r, curves) = cluster(LoadConfig::default());
    let points = [
        (10_000, 80 * GIB),
        (10_000, 80 * GIB),
        (10_000, 80 * GIB),
        (150_000, 80 * GIB),
    ];
    curves.set("huge", &points, 5);
    place(&mut r, "huge", warm(20_000, 8 * GIB));
    r.keyed("other", 1_000, 8 * GIB, warm(20_000, GIB));
    assert!(r.tick().is_empty(), "nothing fits beside it");
}

/// Catches: the working set a lease held in the last session still charged in the next
/// (a re-registered worker whose lease shrank): after a new session the held floor is gone
/// until a minute says it again.
#[test]
fn a_new_session_forgets_what_the_old_one_said_of_a_working_set() {
    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("over", &flat_ws(&[1_000], 4 * GIB), 5);
    let over = place(&mut r, "over", warm(1_000, 4 * GIB));
    let seen = [minute(over.lease, 0, 1_000, 9 * GIB)];
    r.window_clocked(
        "a",
        2_000,
        9 * GIB,
        &[over.lease],
        &[(over.lease, 1)],
        &seen,
    );
    assert_eq!(ahead(&r, 5).1, 9 * GIB);
    r.feed(js_sched::Event::WorkerUp {
        worker: w("a"),
        instance: js_sched::DaemonInstance::new("a"),
        predecessor_stopped: false,
        capacity: node(),
        caps: load_rig::caps("container"),
    });
    r.baseline("a");
    r.window_clocked("a", 2_000, GIB, &[over.lease], &[(over.lease, 1)], &[]);
    assert_eq!(ahead(&r, 5).1, 4 * GIB, "the curve again");
}

/// Catches: a minute record for a lease the worker does not hold, or for a lease charged
/// flat, being counted or changing a charge.
#[test]
fn minutes_of_strangers_and_of_flat_leases_change_nothing() {
    let (mut r, _curves) = cluster(LoadConfig::default());
    let flat = place(&mut r, "flat", warm(1_000, 2 * GIB));
    let seen = [
        minute(flat.lease, 0, 9_000, 30 * GIB),
        minute(LeaseId::new(9, 9), 0, 9_000, 30 * GIB),
    ];
    r.window_clocked(
        "a",
        2_000,
        3 * GIB,
        &[flat.lease],
        &[(flat.lease, 1)],
        &seen,
    );
    let c = r.s.load_counters();
    assert_eq!((c.misfit_cpu_minutes, c.misfit_memory_minutes), (0, 0));
    assert_eq!(
        ahead(&r, 3).1,
        3 * GIB,
        "the working set, not a held floor of 30"
    );
}

/// Catches: the profile layer not what lets two leases whose busy minutes differ share a
/// worker that their flat costs would fill (the point of it), and the operator's off
/// switches not restoring the flat rule: 56 cores each, 112 > 110 flat, 60 + 42 by curve.
#[test]
fn profiles_admit_what_the_flat_rule_refuses_and_the_switch_turns_them_off() {
    let (r, placed) = busy_pair(LoadConfig::default(), 56_000);
    assert_eq!(placed, 2);
    let c = r.s.load_counters();
    assert_eq!((c.profile_cpu, c.profile_memory), (2, 2));
    let off = LoadConfig {
        profile: false,
        profile_memory: false,
        ..LoadConfig::default()
    };
    let (r, placed) = busy_pair(off, 56_000);
    assert_eq!(placed, 1, "the flat rule: 56 + 56 > 110");
    let c = r.s.load_counters();
    assert_eq!((c.profile_cpu, c.profile_memory), (0, 0));
    assert_eq!(
        c.curve_flat, 1,
        "a key with a curve charged flat is counted"
    );
}

/// Catches: the memory switch turning the CPU profile off with it, or not turning the
/// memory profile off: with memory off a lease past its peak is still charged its peak.
#[test]
fn the_memory_switch_is_apart_from_the_cpu_one() {
    let mem = LoadConfig {
        profile_memory: false,
        ..LoadConfig::default()
    };
    let (r, placed) = busy_pair(mem, 56_000);
    assert_eq!(placed, 2, "CPU is still by curve");
    let c = r.s.load_counters();
    assert_eq!((c.profile_cpu, c.profile_memory), (2, 0));
    let (mut r, curves) = cluster_on(mem, Resources::new(100_000, 15 * GIB));
    let points: Vec<(u32, u64)> = [2, 8, 8, 1].iter().map(|&g| (1_000, g * GIB)).collect();
    curves.set("m1", &points, 5);
    curves.set("m2", &points, 5);
    let first = place(&mut r, "m1", warm(1_000, 8 * GIB));
    r.window_clocked("a", 1_000, GIB, &[first.lease], &[(first.lease, 3)], &[]);
    r.keyed("m2", 1_000, 8 * GIB, warm(1_000, 8 * GIB));
    assert!(r.tick().is_empty(), "memory held flat: 8 + 8 > 15 for good");
    assert_eq!(ahead(&r, 9).1, 8 * GIB);
}

/// Catches: the CPU switch turning the memory profile off with it (a lease past its peak
/// charged the peak again) or leaving CPU by curve: with CPU off and memory on, CPU is flat
/// (56 + 56 > 110) and memory is by the curve (1 + 8 <= 15).
#[test]
fn the_cpu_switch_is_apart_from_the_memory_one() {
    let cpu_off = LoadConfig {
        profile: false,
        ..LoadConfig::default()
    };
    let (r, placed) = busy_pair(cpu_off, 56_000);
    assert_eq!(placed, 1, "CPU is flat: 56 + 56 > 110");
    let c = r.s.load_counters();
    assert_eq!((c.profile_cpu, c.profile_memory), (0, 1));
    let (mut r, curves) = cluster_on(cpu_off, Resources::new(100_000, 15 * GIB));
    let points: Vec<(u32, u64)> = [2, 8, 8, 1].iter().map(|&g| (1_000, g * GIB)).collect();
    curves.set("m1", &points, 5);
    curves.set("m2", &points, 5);
    let first = place(&mut r, "m1", warm(1_000, 8 * GIB));
    r.window_clocked("a", 1_000, GIB, &[first.lease], &[(first.lease, 3)], &[]);
    assert_eq!(ahead(&r, 9).1, GIB, "past its peak: the curve's tail");
    r.keyed("m2", 1_000, 8 * GIB, warm(1_000, 8 * GIB));
    assert_eq!(r.tick().len(), 1, "memory by curve: 1 + 8 <= 15");
}

/// Catches: both switches off not being the flat rule exactly: the same events with a
/// curve source that knows every key and with none give the same placements, the same
/// charges at every minute and the same counters (but for the count of curves unused).
#[test]
fn with_both_switches_off_the_curves_change_nothing() {
    let off = LoadConfig {
        profile: false,
        profile_memory: false,
        ..LoadConfig::default()
    };
    let run = |with_curves: bool| {
        let (mut r, curves) = cluster(off);
        if with_curves {
            curves.set_all(&flat_ws(&[10_000, 90_000, 10_000], 4 * GIB), 5);
        }
        let mut placed = Vec::new();
        for i in 0..6 {
            r.keyed(&format!("k{i}"), 1_000, 8 * GIB, warm(30_000, 6 * GIB));
            placed.push(r.tick().len());
        }
        let charges: Vec<(u64, u64)> = (0..40).map(|m| ahead(&r, m)).collect();
        let mut counters = r.s.load_counters();
        counters.curve_flat = 0;
        (placed, charges, counters)
    };
    assert_eq!(run(true), run(false));
}

/// Catches: a cold key, a key with too few runs, an empty curve, a retry up the ladder, and
/// a lease with no prediction charged by a curve: each is charged flat.
#[test]
fn cold_young_empty_retried_and_unpredicted_leases_are_charged_flat() {
    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("cold", &flat_ws(&[10_000, 90_000], GIB), 5);
    curves.set("young", &flat_ws(&[10_000, 90_000], GIB), 2);
    curves.set("empty", &[], 5);
    curves.set("none", &flat_ws(&[10_000, 90_000], GIB), 5);
    let cold_start = {
        r.keyed("cold", 1_000, 8 * GIB, cold(20_000, 4 * GIB));
        r.tick().remove(0)
    };
    place(&mut r, "young", warm(20_000, 2 * GIB));
    place(&mut r, "empty", warm(20_000, 2 * GIB));
    let request = {
        let mut q = r.request(js_types::sched::LeaseKind::Action, 20_000, 8 * GIB);
        q.key = WorkKey::new("load", "none");
        q
    };
    r.submit(request);
    assert_eq!(r.tick().len(), 1);
    let c = r.s.load_counters();
    assert_eq!((c.admitted, c.profile_cpu, c.profile_memory), (4, 0, 0));
    assert_eq!(c.curve_flat, 4);
    // Flat at every minute: 20 + 20 + 20 + 20 (the unpredicted one is its request's cores).
    for m in [0, 1, 2, 31] {
        assert_eq!(ahead(&r, m).0, 80_000, "minute {m}");
    }
    // A retry: the cold lease is killed at its limit and runs again one rung up, flat,
    // though its key now has a curve.
    curves.set("cold", &flat_ws(&[10_000, 90_000], GIB), 5);
    r.end(&cold_start, Outcome::Failed(Failure::OutOfMemory));
    r.s.set_cost(cold_start.operation, warm(20_000, 2 * GIB));
    let again = r.tick();
    assert_eq!(again.len(), 1);
    assert_eq!(
        r.s.load_counters().profile_cpu,
        0,
        "a retry is charged flat"
    );
    let cost = r.s.lease_cost(again[0].lease).expect("held");
    assert_eq!(cost.memory_bytes, 16 * GIB, "at the rung");
}

/// Catches: a lease of a worker charged by prediction alone charged by curve (a worker
/// with no window): every minute reads the same, and the lease is counted as a fallback.
#[test]
fn a_worker_charged_by_prediction_alone_charges_flat() {
    let curves = Curves::new("load");
    let mut r = Rig::with_curves(LoadConfig::default(), curves.clone());
    r.up("a", node());
    curves.set("p", &flat_ws(&[10_000, 90_000], GIB), 5);
    place(&mut r, "p", warm(20_000, 2 * GIB));
    for m in [0, 1, 2, 31] {
        assert_eq!(ahead(&r, m), (20_000, 2 * GIB));
    }
    let c = r.s.load_counters();
    assert_eq!(
        (c.admitted_fallback, c.profile_cpu, c.curve_flat),
        (1, 0, 1)
    );
    assert_eq!(r.s.charged_ahead(&w("nobody"), 0), None);
}

/// Catches: a VM charged by a curve (it books its cores and memory whole and runs outside
/// the measured cgroup).
#[test]
fn a_vm_is_charged_what_it_books_whatever_curve_its_key_has() {
    use js_types::sched::LeaseKind;
    let curves = Curves::new("load");
    let mut r = Rig::with_curves(LoadConfig::default(), curves.clone());
    r.up_with(
        "a",
        node().with_vms(2),
        load_rig::caps("container").with_drivers(["container", "vm"]),
    );
    r.set_clock("a", HOUR);
    r.baseline("a");
    r.window("a", 0, 0, &[]);
    curves.set_all(&flat_ws(&[1_000, 1_000], GIB), 5);
    let mut vm = r.request(LeaseKind::Vm, 4_000, 16 * GIB);
    vm.resources = vm.resources.with_vms(1);
    r.submit(vm);
    assert_eq!(r.tick().len(), 1);
    assert_eq!(ahead(&r, 0), (4_000, 16 * GIB));
    assert_eq!(ahead(&r, 31), (4_000, 16 * GIB));
}

/// Catches: an action at the whole host's memory, which runs alone, charged by a curve
/// (or held to the flat-memory guard): it is charged flat.
#[test]
fn an_action_at_the_whole_hosts_memory_is_charged_flat() {
    let flat_guard = LoadConfig {
        flat_memory_slack_permille: Some(250),
        ..LoadConfig::default()
    };
    let (mut r, curves) = cluster(flat_guard);
    curves.set("whole", &flat_ws(&[10_000, 90_000], GIB), 5);
    r.keyed("whole", 1_000, 64 * GIB, warm(20_000, 64 * GIB));
    assert_eq!(r.tick().len(), 1);
    assert_eq!(ahead(&r, 1).0, 20_000, "flat, not the curve's 90 cores");
    assert_eq!(r.s.load_counters().profile_cpu, 0);
}

/// Catches: a lease recovered from the store charged by a curve it was not recorded with:
/// the recovered scheduler charges it flat, as its persisted cost says, at every minute.
#[test]
fn a_recovered_lease_is_charged_flat() {
    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("p", &flat_ws(&[10_000, 90_000], GIB), 5);
    let start = place(&mut r, "p", warm(20_000, 2 * GIB));
    let operation = start.operation;
    let request = r.s.request(operation).expect("held").clone();
    let cost = r.s.lease_cost(start.lease);
    let mut again = Rig::with_curves(LoadConfig::default(), curves.clone());
    again.s = js_sched::Scheduler::new(2)
        .with_load_config(LoadConfig::default())
        .with_curves(curves.clone());
    again.s.recover(
        2,
        js_types::sched::OperationId(operation.0 + 1),
        0,
        vec![js_sched::Recovered {
            operation,
            request,
            waiter: js_types::sched::WaiterId(0),
            lease: Some(js_sched::RecoveredLease {
                lease: start.lease,
                worker: w("a"),
                cost,
            }),
            losses: 0,
            fate: None,
        }],
    );
    again.up("a", node());
    again.set_clock("a", HOUR);
    again.baseline("a");
    again.window_clocked("a", 0, 0, &[start.lease], &[(start.lease, 0)], &[]);
    // Covered, a clock, a curve for its key: flat all the same.
    for m in [0, 1, 2, 31] {
        assert_eq!(
            again.s.charged_ahead(&w("a"), m),
            Some((15_000, 2 * GIB)),
            "minute {m}"
        );
    }
}
