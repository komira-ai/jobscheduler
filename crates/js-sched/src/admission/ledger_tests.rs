use super::*;

const GIB: u64 = 1 << 30;

fn cost(cpu: u64, mem: u64) -> LeaseCost {
    LeaseCost {
        cpu_millis: cpu,
        memory_bytes: mem,
        limit_bytes: mem,
        cold: false,
        retry: false,
        key_hash: 0,
    }
}

fn lease(n: u64) -> LeaseId {
    LeaseId::new(1, n)
}

/// Catches: a lease booked twice charged twice (a recovered lease adopted again would
/// double the worker's sums for good), and removing a lease that is not charged
/// changing the sums.
#[test]
fn a_lease_is_charged_once_and_removing_a_stranger_changes_nothing() {
    let mut load = WorkerLoad::default();
    load.insert(lease(1), cost(500, GIB), false, None);
    load.insert(lease(1), cost(700, 2 * GIB), false, None);
    assert_eq!(load.totals().cpu_uncovered, 700);
    assert_eq!(load.totals().mem_uncovered, 2 * GIB);
    assert_eq!(load.totals().actions, 1);
    load.remove(lease(9));
    assert_eq!(load.totals().cpu_uncovered, 700);
    load.remove(lease(1));
    assert_eq!(*load.totals(), Totals::default());
}

fn curve(points: &[(u32, u64)]) -> js_types::sched::usage::Curve {
    let points = points
        .iter()
        .map(
            |&(cores_milli, working_set_bytes)| js_types::sched::usage::MinutePoint {
                cores_milli,
                working_set_bytes,
            },
        )
        .collect();
    js_types::sched::usage::Curve::new(points, 5).expect("a short curve")
}

/// A profile of a lease of `cost(20_000, 2 GiB)`: the curve of cores 10 then 60 and
/// working set 4 GiB then 1 GiB, by curve in CPU and in memory as asked, or none and
/// a life of three minutes.
fn profile(by_curve: bool, cpu: bool, mem: bool, life: Option<u32>) -> Arc<Profile> {
    use super::super::profile::Choice;
    Arc::new(Profile::new(
        Choice {
            curve: by_curve.then(|| curve(&[(10_000, 4 * GIB), (60_000, GIB)])),
            cpu,
            mem,
            demoted_cpu: false,
            demoted_mem: false,
            life,
            floor_permille: 750,
            guard_key: 1,
        },
        &cost(20_000, 2 * GIB),
    ))
}

/// Catches: the per-slot lanes missing a kind of lease, or counting a lease in the
/// wrong lane or twice, or not giving it back: six leases (charged by curve in both
/// resources, covered and not; flat to a life; by curve in CPU only; by curve in
/// memory only; covered with no clock, which is flat) are summed slot by slot and taken
/// off to nothing.
#[test]
fn the_lanes_sum_each_kind_of_lease_by_slot_and_empty_when_they_are_removed() {
    use js_types::sched::usage::LeaseClock;
    let mut load = WorkerLoad::default();
    let kinds = [
        (profile(true, true, true, None), false),
        (profile(true, true, true, None), true),
        (profile(false, true, true, Some(3)), true),
        (profile(true, true, false, None), false),
        (profile(true, true, true, None), true),
        (profile(true, false, true, None), false),
    ];
    for (i, (p, _)) in kinds.iter().enumerate() {
        load.insert(
            lease(i as u64),
            cost(20_000, 2 * GIB),
            false,
            Some(p.clone()),
        );
    }
    // A floor below 1, so that the sum of the flat covered leases shows the floor.
    let cfg = LoadConfig {
        cpu_floor_permille: 750,
        ..LoadConfig::default()
    };
    let capacity = Resources::new(100_000, 64 * GIB);
    let mut counters = LoadCounters::default();
    let read = |seq: u64, covered: Vec<LeaseId>, clocks: Vec<LeaseClock>| NodeLoad {
        seq,
        mono_usec: 3_600_000_000 + seq * 5_000_000,
        covered,
        lease_clocks: clocks,
        ..NodeLoad::default()
    };
    let now = FarmTime::from_millis(0);
    load.accept(
        read(1, Vec::new(), Vec::new()),
        now,
        (&capacity, capacity.memory_bytes, true),
        &cfg,
        &mut counters,
    );
    // Leases 1, 2 and 4 are covered; 1 is a minute old, 2 is new, 4 has no clock.
    let second = read(2, vec![lease(1), lease(2), lease(4)], Vec::new());
    let clock = |l: u64, age_minutes: u64| LeaseClock {
        lease: lease(l),
        started_mono_usec: second.mono_usec - age_minutes * 60_000_000,
    };
    let second = NodeLoad {
        lease_clocks: vec![clock(1, 1), clock(2, 0)],
        ..second
    };
    load.accept(
        second,
        now,
        (&capacity, capacity.memory_bytes, true),
        &cfg,
        &mut counters,
    );
    let t = load.totals();
    assert!(t.has_timed());
    // Covered by curve (lease 1: 60 cores from its minute 1); covered flat to a life
    // (lease 2: 20 cores for three minutes, a floor of 0.75 on the sum); the flat
    // lease 4 stays in the scalar sum (20 cores).
    assert_eq!(
        t.cpu_covered_at(0, &cfg),
        60_000 + (20_000 + 20_000) * 3 / 4
    );
    assert_eq!(t.cpu_covered_at(5, &cfg), 60_000 + 20_000 * 3 / 4);
    // Uncovered: leases 0 and 3 by curve (60 cores, the larger of minutes 0 and 1 and
    // then the tail), lease 5 flat in CPU (20).
    assert_eq!(t.cpu_uncovered_at(0), 60_000 + 60_000 + 20_000);
    assert_eq!(t.cpu_uncovered_at(31), 60_000 + 60_000 + 20_000);
    // Memory: lease 0 and 5 hold 4 GiB then 1; lease 1 holds 1 (from its minute 1);
    // lease 2 holds 2 for three minutes; leases 3 and 4 flat (2 each).
    assert_eq!(t.mem_all_at(0), (4 + 1 + 2 + 2 + 2 + 4) * GIB);
    assert_eq!(t.mem_all_at(1), (1 + 1 + 2 + 2 + 2 + 1) * GIB);
    // Lease 2 has ended its life by minute 3.
    assert_eq!(t.mem_all_at(3), (1 + 1 + 2 + 2 + 1) * GIB);
    assert_eq!(t.mem_uncovered_at(0), (4 + 2 + 4) * GIB);
    assert_eq!(t.mem_uncovered_at(1), (1 + 2 + 1) * GIB);
    for i in 0..6 {
        load.remove(lease(i));
    }
    assert_eq!(*load.totals(), Totals::default());
}

/// Catches: a lease placed before a reading counted again beside the measurement, which
/// holds some of what it used; such a lease hidden in the measurement, which may be other
/// work, instead of added to the window before it was placed; and a lease placed after
/// the reading taken into the measurement's maximum. A covered lease (30 cores) and two
/// uncovered ones (one by curve, one flat) are on the ledger when a reading is taken in.
/// Two more are placed after it, and a VM, which no reading sees. At minute 0 the seen
/// leases are added to the larger of the window before and the covered charge, that is
/// raised to the measurement, and the unseen leases and the VM are added on top; later
/// minutes have no measurement. A new session leaves no lease seen, and the sums are
/// empty when the leases go.
#[test]
fn leases_placed_before_a_reading_are_added_to_the_window_before_it() {
    let mut load = WorkerLoad::default();
    let cfg = LoadConfig::default();
    let capacity = Resources::new(400_000, 64 * GIB);
    let mut counters = LoadCounters::default();
    let read = |seq: u64, covered: Vec<LeaseId>| NodeLoad {
        seq,
        mono_usec: seq * 5_000_000,
        covered,
        ..NodeLoad::default()
    };
    let now = FarmTime::from_millis(0);
    let by_curve = || Some(profile(true, true, false, None));
    load.insert(lease(4), cost(30_000, GIB), false, None);
    load.insert(lease(0), cost(20_000, GIB), false, by_curve());
    load.insert(lease(1), cost(20_000, GIB), false, None);
    for (seq, covered) in [(1, Vec::new()), (2, vec![lease(4)])] {
        let reading = read(seq, covered);
        load.accept(
            reading,
            now,
            (&capacity, capacity.memory_bytes, true),
            &cfg,
            &mut counters,
        );
    }
    load.insert(lease(2), cost(20_000, GIB), false, by_curve());
    load.insert(lease(3), cost(20_000, GIB), false, None);
    load.insert(lease(5), cost(10_000, GIB), true, None);
    let t = *load.totals();
    // Seen: lease 0 by curve (60 cores, the larger of minutes 0 and 1), lease 1 flat
    // (20). Unseen: the same again. The VM is in neither.
    assert_eq!(t.cpu_uncovered_at(0), 2 * (60_000 + 20_000));
    assert_eq!(
        (t.cpu_unseen_at(0), t.cpu_unseen),
        (60_000 + 20_000, 40_000)
    );
    let (seen, unseen, vm) = (80_000, 80_000, 10_000);
    let at = |measured, before| horizon::cpu_left(&t, 0, (measured, before), &cfg);
    // The covered charge above the window before; then the window before above it.
    assert_eq!(at(30_000, 0), 30_000 + seen + unseen + vm);
    assert_eq!(at(60_000, 50_000), 50_000 + seen + unseen + vm);
    // The measurement above both sums: it holds the seen leases' use.
    assert_eq!(at(150_000, 10_000), 150_000 + unseen + vm);
    assert_eq!(
        horizon::cpu_left(&t, 5, (150_000, 10_000), &cfg),
        30_000 + seen + unseen + vm
    );
    load.new_session();
    let t = *load.totals();
    assert_eq!(t.cpu_unseen, t.cpu_uncovered);
    assert_eq!(t.cpu_unseen_at(0), t.cpu_uncovered_at(0));
    for i in 0..6 {
        load.remove(lease(i));
    }
    assert_eq!(*load.totals(), Totals::default());
}

fn keyed(cpu: u64, hash: u64) -> LeaseCost {
    LeaseCost {
        key_hash: hash,
        ..cost(cpu, 0)
    }
}

/// Catches: the same-key sum counting a lease of another key, a VM, or leases with no
/// key known (hash 0 matches nothing, not each other), or not counting covered and
/// uncovered leases alike.
#[test]
fn the_same_key_sum_counts_only_that_key_covered_or_not() {
    let mut load = WorkerLoad::default();
    load.insert(lease(1), keyed(1_000, 7), false, None);
    load.insert(lease(2), keyed(2_000, 7), false, None);
    load.insert(lease(3), keyed(4_000, 8), false, None);
    load.insert(lease(4), keyed(8_000, 7), true, None);
    load.insert(lease(5), keyed(16_000, 0), false, None);
    load.cover(&BTreeSet::from([lease(1)]), true);
    assert_eq!(load.same_key_millis(7), 3_000);
    assert_eq!(load.same_key_millis(8), 4_000);
    assert_eq!(load.same_key_millis(9), 0);
    assert_eq!(load.same_key_millis(0), 0);
}

/// Catches: the uncovered count taking covered leases or VMs in, or not following a
/// reading that covers a lease.
#[test]
fn uncovered_leases_are_the_actions_no_reading_lists() {
    let mut load = WorkerLoad::default();
    load.insert(lease(1), cost(1_000, 0), false, None);
    load.insert(lease(2), cost(1_000, 0), false, None);
    load.insert(lease(3), cost(1_000, 0), true, None);
    assert_eq!(load.uncovered_leases(), 2);
    load.cover(&BTreeSet::from([lease(2), lease(3)]), true);
    assert_eq!(load.uncovered_leases(), 1);
}

/// Catches: `M > P_c` compared with the floor or with all leases instead of the covered
/// ones, and true before any window: the measurement must be strictly above the sum of
/// the covered leases' predictions.
#[test]
fn measured_above_predicted_compares_with_the_covered_sum_only() {
    let readings = |windows: [Option<u32>; 2]| Readings {
        last: NodeLoad::default(),
        arrived: FarmTime::from_millis(0),
        windows,
    };
    let mut load = WorkerLoad::default();
    load.insert(lease(1), cost(1_000, 0), false, None);
    load.insert(lease(2), cost(5_000, 0), false, None);
    load.cover(&BTreeSet::from([lease(1)]), true);
    assert!(!load.measured_above_covered(), "no window yet");
    load.readings = Some(readings([Some(1_000), None]));
    assert!(!load.measured_above_covered(), "equal is not above");
    load.readings = Some(readings([Some(1_001), None]));
    assert!(load.measured_above_covered());
    load.readings = Some(readings([Some(900), Some(1_001)]));
    assert!(load.measured_above_covered(), "the larger of two windows");
}

/// Catches: a VM lease's CPU time counted by the plausibility check (a VM runs outside the
/// actions group, and no lease result measures it), a recovered lease counted (its run may
/// have begun before any reading the check keeps), CPU time reported before the lease's
/// end lost, and a marked signal still charged by measurement. An action lease admitted
/// after the first reading reports 60 core-seconds while the counter does not move: the
/// worker is charged by prediction from the reading after its end.
#[test]
fn the_plausibility_check_counts_admitted_action_leases_and_marks_the_worker() {
    let mut load = WorkerLoad::default();
    let cfg = LoadConfig::default();
    let capacity = Resources::new(16_000, 64 * GIB);
    let mut counters = LoadCounters::default();
    let read = |seq: u64| NodeLoad {
        seq,
        mono_usec: seq * 5_000_000,
        ..NodeLoad::default()
    };
    let t = FarmTime::from_millis;
    let held = |held: bool| (&capacity, capacity.memory_bytes, held);
    load.accept(read(1), t(5_000), held(false), &cfg, &mut counters);
    load.admit(lease(1), cost(4_000, 0), None, t(6_000));
    load.insert(lease(2), cost(4_000, 0), true, None);
    load.insert(lease(3), cost(4_000, 0), false, None);
    for l in 1..=3 {
        assert!(load.lease_cpu(lease(l), 60_000_000, t(7_000)), "lease {l}");
    }
    assert!(!load.lease_cpu(lease(9), 60_000_000, t(7_000)));
    assert!(load.end(lease(9), t(8_000)).is_none());
    for l in 2..=3 {
        assert!(load.end(lease(l), t(8_000)).is_some());
    }
    load.accept(read(2), t(10_000), held(true), &cfg, &mut counters);
    assert!(
        !load.check().implausible(),
        "neither the VM nor the recovered lease counts"
    );
    assert!(!load.left(t(10_000), &capacity, &cfg).fallback);
    assert!(load.end(lease(1), t(11_000)).is_some());
    load.accept(read(3), t(15_000), held(false), &cfg, &mut counters);
    assert!(load.check().lease_cpu_implausible());
    assert!(!load.fresh(t(15_000), &cfg));
    assert!(load.left(t(15_000), &capacity, &cfg).fallback);
    assert_eq!(counters.signal_implausible_lease_cpu, 1);
}
