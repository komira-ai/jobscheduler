//! Tests of the scorer adapter and ranker 5: what the model answers through the seam, the
//! slot-0 left side with the measurement, the evaluation identity, the minting of lease
//! ids, and the scorer's own cases (`tests/load_score.rs`) replayed through the adapter.
//!
//! Each test names the defect it would catch. None reads a clock or asserts a timing.

use std::collections::BTreeSet;

use js_sched::{
    Charge, EVALUATIONS_PER_WORKER, Event, H_PLUS, PredictedLoad, SLOTS, ScoreConfig, Scorer,
};
use js_types::sched::{LeaseId, Resources, WorkerId};

use crate::engine::adapter::{
    FLOOR_PERMILLE, HostState, LEASE_TERM, LeaseBelief, ScoreModel, ScoreRanker, Slot0, lease_id,
    slot0_left,
};
use crate::load_rig::{GIB, Rig, w, warm};

const CPU_CAP: u64 = 88_000;
const MEM_CAP: u64 = 256 * GIB;

fn id(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn cpu(millis: u64) -> Charge {
    Charge {
        cpu_millis: millis,
        memory_bytes: 0,
    }
}

/// A lease charged `curve` (millicores) at each slot, memory 0.
fn lease_with(ix: u64, curve: [u64; SLOTS]) -> LeaseBelief {
    LeaseBelief {
        lease_ix: ix,
        cold: false,
        fit_minutes: 1,
        p_bar_cpu_millis: curve[0],
        charge: curve.map(cpu),
    }
}

/// A lease charged `millis` at every slot.
fn flat_lease(ix: u64, millis: u64) -> LeaseBelief {
    lease_with(ix, [millis; SLOTS])
}

/// A host whose predicted load is `curve` (millicores) at each slot, measured at the
/// curve's slot 0 and fully covered.
fn curve_host(cap: u64, curve: [u64; SLOTS]) -> HostState {
    let mut ahead = [Charge::default(); SLOTS - 1];
    for (slot, v) in ahead.iter_mut().zip(&curve[1..]) {
        *slot = cpu(*v);
    }
    HostState {
        cpu_capacity_millis: cap,
        memory_capacity_bytes: MEM_CAP,
        slot0: Slot0 {
            measured_cpu_millis: curve[0],
            covered_cpu_millis: curve[0],
            ..Slot0::default()
        },
        ahead,
        same_key_millis: 0,
        uncovered_leases: 0,
    }
}

fn flat_host(cap: u64, millis: u64) -> HostState {
    curve_host(cap, [millis; SLOTS])
}

fn model(lease: LeaseBelief, hosts: &[(&str, HostState)]) -> ScoreModel {
    ScoreModel::new(
        lease,
        FLOOR_PERMILLE,
        hosts.iter().map(|(n, h)| (id(n), *h)),
    )
}

fn names(list: &[&str]) -> Vec<WorkerId> {
    list.iter().map(|n| id(n)).collect()
}

/// Shows: every answer of the seam comes from the field it names, with figures distinct
/// enough that crossing two fields, or a slot off by one, changes an assertion. Catches a
/// load read at the wrong slot (the `H+` slot included), a charge taken from the host, a
/// capacity swapped, a lost `new_key_millis` and a fallback flag set for a measured host.
#[test]
fn the_model_answers_the_seam_from_the_figures_it_was_given() {
    let mut ahead = [Charge::default(); SLOTS - 1];
    for (i, slot) in ahead.iter_mut().enumerate() {
        *slot = Charge {
            cpu_millis: 1_000 + i as u64,
            memory_bytes: 5_000_000 + i as u64,
        };
    }
    let host = HostState {
        cpu_capacity_millis: 16_000,
        memory_capacity_bytes: 45 * GIB,
        slot0: Slot0 {
            measured_cpu_millis: 900,
            covered_cpu_millis: 800,
            uncovered_cpu_millis: 70,
            memory_all_bytes: 3_000_000,
            working_set_bytes: 2_000_000,
            uncovered_memory_bytes: 400_000,
        },
        ahead,
        same_key_millis: 1_234,
        uncovered_leases: 3,
    };
    let mut charge = [Charge::default(); SLOTS];
    for (i, slot) in charge.iter_mut().enumerate() {
        *slot = Charge {
            cpu_millis: 200 + i as u64,
            memory_bytes: 9_000_000 + i as u64,
        };
    }
    let lease = LeaseBelief {
        lease_ix: 41,
        cold: true,
        fit_minutes: 7,
        p_bar_cpu_millis: 555,
        charge,
    };
    let m = model(lease, &[("a", host)]);
    let a = id("a");

    assert_eq!(
        m.lease().lease,
        LeaseId {
            term: LEASE_TERM,
            seq: 41
        }
    );
    assert!(m.lease().cold);
    // Slot 0 is the rule with the measurement: max(900, ceil(0.75 * 800)) + 70 and
    // max(3_000_000, 2_000_000 + 400_000).
    assert_eq!(
        m.load(&a, 0),
        Charge {
            cpu_millis: 970,
            memory_bytes: 3_000_000
        }
    );
    for slot in 1..SLOTS {
        let i = (slot - 1) as u64;
        assert_eq!(
            m.load(&a, slot),
            Charge {
                cpu_millis: 1_000 + i,
                memory_bytes: 5_000_000 + i
            },
            "slot {slot}"
        );
    }
    assert_eq!(m.load(&a, H_PLUS).cpu_millis, 1_030, "the last entry is H+");
    for slot in 0..SLOTS {
        assert_eq!(m.charge(&a, slot), lease.charge[slot], "slot {slot}");
    }
    let facts = m.host(&a);
    assert_eq!(facts.cpu_capacity_millis, 16_000);
    assert_eq!(facts.memory_capacity_bytes, 45 * GIB);
    assert_eq!(facts.fit_minutes, 7);
    assert_eq!(facts.new_key_millis, 555);
    assert_eq!(facts.same_key_millis, 1_234);
    assert_eq!(facts.uncovered_leases, 3);
    assert!(!facts.fallback);
    assert!(facts.measured_above_predicted, "900 above the covered 800");
}

/// Shows: `measured_above_predicted` is the measured CPU strictly above the predicted sum
/// of the covered leases, not above the floor and not at equality. Catches `>=`, and a
/// comparison against `f * covered`.
#[test]
fn measured_above_predicted_is_strictly_above_the_covered_sum() {
    for (measured, want) in [(3_999, false), (4_000, false), (4_001, true)] {
        let mut h = flat_host(CPU_CAP, 0);
        h.slot0 = Slot0 {
            measured_cpu_millis: measured,
            covered_cpu_millis: 4_000,
            ..Slot0::default()
        };
        let m = model(flat_lease(0, 1_000), &[("a", h)]);
        assert_eq!(
            m.host(&id("a")).measured_above_predicted,
            want,
            "{measured}"
        );
    }
    // Uncovered leases do not raise the bar: a measurement between the covered sum and
    // covered plus uncovered is still above the predictions. Catches a comparison against
    // the sum that includes the uncovered side.
    for (measured, want) in [(4_000, false), (4_001, true), (4_500, true), (4_700, true)] {
        let mut h = flat_host(CPU_CAP, 0);
        h.slot0 = Slot0 {
            measured_cpu_millis: measured,
            covered_cpu_millis: 4_000,
            uncovered_cpu_millis: 700,
            ..Slot0::default()
        };
        let m = model(flat_lease(0, 1_000), &[("a", h)]);
        assert_eq!(
            m.host(&id("a")).measured_above_predicted,
            want,
            "uncovered, measured {measured}"
        );
    }
}

/// Shows: key 4 reads the lease's own `p_bar` through `new_key_millis`: a host whose
/// same-key residents are 2,000 millicores is under a quarter of 10,000 with a new lease
/// of 400 and over it with 600. Catches `new_key_millis` left at 0 (the share would never
/// be exceeded by the new lease) and `same_key_millis` dropped.
#[test]
fn key_four_adds_the_leases_own_p_bar() {
    for (p_bar, want) in [(400, false), (500, false), (501, true), (600, true)] {
        let mut h = flat_host(10_000, 0);
        h.same_key_millis = 2_000;
        let mut lease = flat_lease(0, 100);
        lease.p_bar_cpu_millis = p_bar;
        let m = model(lease, &[("a", h)]);
        let key = Scorer::default().key(&m, &id("a"));
        assert_eq!(key.concentrated(), want, "p_bar {p_bar}");
    }
    // The residents alone do not set it either.
    let mut h = flat_host(10_000, 0);
    h.same_key_millis = 0;
    let mut lease = flat_lease(0, 100);
    lease.p_bar_cpu_millis = 2_600;
    let m = model(lease, &[("a", h)]);
    assert!(Scorer::default().key(&m, &id("a")).concentrated());
}

/// Shows: the slot-0 left side is the predicate's `max(measured, f * covered) + uncovered`
/// and `max(all, working_set + uncovered)`, and reports when the measurement is the larger
/// term. Cases in the style of `load_score.rs`: the measurement above the floor, the floor
/// above the measurement, a tie, the floor rounded up, nothing covered, `f = 1`. Catches a
/// predicted sum used in place of the measurement, a floor rounded down, a measurement that
/// replaces the floor instead of competing with it, and uncovered leases left out.
#[test]
fn slot_zero_is_the_larger_of_measured_and_floored_covered_plus_uncovered() {
    // (measured, covered, uncovered, f, cpu left, measurement wins)
    let table: [(u64, u64, u64, u64, u64, bool); 9] = [
        (5_000, 4_000, 1_000, 750, 6_000, true),
        (1_000, 4_000, 1_000, 750, 4_000, false),
        (3_000, 4_000, 0, 750, 3_000, false),
        (3_001, 4_000, 0, 750, 3_001, true),
        (0, 1, 0, 750, 1, false),
        (0, 3, 0, 750, 3, false),
        (0, 0, 700, 750, 700, false),
        (2_000, 4_000, 0, 1_000, 4_000, false),
        (4_001, 4_000, 0, 1_000, 4_001, true),
    ];
    for (measured, covered, uncovered, f, want, wins) in table {
        let s = Slot0 {
            measured_cpu_millis: measured,
            covered_cpu_millis: covered,
            uncovered_cpu_millis: uncovered,
            ..Slot0::default()
        };
        let (left, got) = slot0_left(&s, f);
        assert_eq!(
            left.cpu_millis, want,
            "{measured} {covered} {uncovered} f{f}"
        );
        assert_eq!(got, wins, "{measured} {covered} {uncovered} f{f}");
    }
    // (memory all, working set, uncovered memory, memory left)
    for (all, ws, unc, want) in [(10, 5, 2, 10), (10, 9, 2, 11), (0, 0, 0, 0), (7, 7, 0, 7)] {
        let s = Slot0 {
            memory_all_bytes: all,
            working_set_bytes: ws,
            uncovered_memory_bytes: unc,
            ..Slot0::default()
        };
        assert_eq!(slot0_left(&s, 750).0.memory_bytes, want, "{all} {ws} {unc}");
    }
    // Saturation: the largest figures do not wrap.
    let s = Slot0 {
        measured_cpu_millis: u64::MAX,
        covered_cpu_millis: u64::MAX,
        uncovered_cpu_millis: u64::MAX,
        working_set_bytes: u64::MAX,
        uncovered_memory_bytes: 1,
        ..Slot0::default()
    };
    let (left, _) = slot0_left(&s, 1_000);
    assert_eq!((left.cpu_millis, left.memory_bytes), (u64::MAX, u64::MAX));
    let huge = Slot0 {
        covered_cpu_millis: u64::MAX,
        ..Slot0::default()
    };
    assert_eq!(
        slot0_left(&huge, 1_000_000).0.cpu_millis,
        u64::MAX,
        "floor clamps"
    );
}

/// Shows: `load(w, 0)` carries the measurement into the scorer: the same host with a
/// measurement above and below the floor is loaded by the larger figure, and a model host
/// whose measurement is dropped (the predicted sum alone) is a different answer. Catches
/// a slot 0 built from the predicted sum, the quiet-start figure the measurement exists to
/// correct.
#[test]
fn the_measurement_reaches_the_scorers_slot_zero() {
    let mut h = flat_host(CPU_CAP, 0);
    h.slot0 = Slot0 {
        measured_cpu_millis: 9_000,
        covered_cpu_millis: 8_000,
        uncovered_cpu_millis: 500,
        ..Slot0::default()
    };
    let busy = model(flat_lease(0, 1_000), &[("a", h)]);
    assert_eq!(busy.load(&id("a"), 0).cpu_millis, 9_500);
    h.slot0.measured_cpu_millis = 2_000;
    let quiet = model(flat_lease(0, 1_000), &[("a", h)]);
    assert_eq!(
        quiet.load(&id("a"), 0).cpu_millis,
        6_500,
        "the floor 0.75 * 8_000 + 500"
    );
    assert!(busy.measurement_wins(&id("a")));
    assert!(!quiet.measurement_wins(&id("a")));
}

/// Shows: the counter of the measurement term counts scored candidates only and counts the
/// strict wins. Three candidates, two won by the measurement; then a single candidate
/// (taken, not scored) adds nothing. Catches a counter that counts every host, one that
/// counts the only candidate, and one that counts ties.
#[test]
fn the_measurement_counter_counts_scored_candidates_it_won() {
    let wins = |measured| {
        let mut h = flat_host(CPU_CAP, 0);
        h.slot0 = Slot0 {
            measured_cpu_millis: measured,
            covered_cpu_millis: 4_000,
            ..Slot0::default()
        };
        h
    };
    let m = model(
        flat_lease(0, 1_000),
        &[
            ("a", wins(5_000)),
            ("b", wins(1_000)),
            ("c", wins(3_001)),
            ("d", wins(3_000)),
        ],
    );
    let mut ranker = ScoreRanker::new();
    let all = names(&["a", "b", "c", "d"]);
    assert!(ranker.place(&m, &all[..3]).is_some());
    assert_eq!(ranker.tally().measurement_wins, 2, "a and c");
    assert!(ranker.place(&m, &all[..1]).is_some());
    assert_eq!(
        ranker.tally().measurement_wins,
        2,
        "the only candidate is not scored"
    );
    assert!(ranker.place(&m, &names(&["b", "d"])).is_some());
    assert_eq!(ranker.tally().measurement_wins, 2, "a tie is the floor's");
    assert!(ranker.place(&m, &all).is_some());
    assert_eq!(ranker.tally().measurement_wins, 4);
}

/// Shows: the scorer evaluates exactly 64 slot-resource figures per scored candidate and
/// none for a lone candidate or an empty list, over a run of attempts of different sizes
/// (the identity `evaluations == 64 x scored`). Catches a candidate scored twice, a lone
/// candidate scored, and a tally that disagrees with the scorer's own count.
#[test]
fn evaluations_are_64_per_scored_candidate_and_a_lone_candidate_is_not_scored() {
    assert_eq!(EVALUATIONS_PER_WORKER, 64);
    let hosts: Vec<(&str, HostState)> = ["a", "b", "c", "d"]
        .iter()
        .enumerate()
        .map(|(i, n)| (*n, flat_host(CPU_CAP, 4_000 * i as u64)))
        .collect();
    let m = model(flat_lease(3, 1_000), &hosts);
    let all = names(&["a", "b", "c", "d"]);
    let mut ranker = ScoreRanker::new();
    let mut scored = 0u64;
    for take in [1usize, 2, 3, 4, 0, 2, 1] {
        let before = ranker.evaluations();
        let chosen = ranker.place(&m, &all[..take]);
        assert_eq!(chosen.is_some(), take > 0);
        let step = ranker.evaluations() - before;
        if take >= 2 {
            scored += take as u64;
            assert_eq!(step, 64 * take as u64, "{take} candidates");
        } else {
            assert_eq!(step, 0, "{take} candidates are not scored");
        }
    }
    let t = ranker.tally();
    assert_eq!(t.scored_candidates, scored);
    assert_eq!(ranker.evaluations(), 64 * t.scored_candidates);
    assert_eq!(
        (t.attempts, t.scored_attempts, t.scored_candidates),
        (6, 4, 11)
    );
    // The lone candidate is the one handed over, whatever it would score.
    let lone = names(&["d"]);
    assert_eq!(ranker.place(&m, &lone), Some(&lone[0]));
}

/// The 64-bit FNV-1a of `len(lease) as u32 LE, lease, len(worker) as u32 LE, worker`, with
/// the lease as `term-seq` text: the tie-break of the placement design, restated here.
fn oracle_hash(lease_ix: u64, worker: &str) -> u64 {
    let lease = format!("{LEASE_TERM}-{lease_ix}");
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in [lease.as_bytes(), worker.as_bytes()] {
        let len = u32::try_from(part.len()).unwrap().to_le_bytes();
        for byte in len.iter().chain(part) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// Shows: the lease id is minted from the replay's index alone, the same every time, and
/// with four identical hosts the lease goes to the one with the smallest hash of
/// (`term-seq`, worker), whatever order the candidates come in. Catches a minted id that
/// depends on call order, a different term or sequence, a candidate order that leaks into
/// the choice, and a hash that does not spread equal scores (every index one host).
#[test]
fn minted_lease_ids_are_stable_and_decide_equal_scores() {
    assert_eq!(lease_id(0), LeaseId { term: 1, seq: 0 });
    assert_eq!(lease_id(7), LeaseId { term: 1, seq: 7 });
    assert_eq!(lease_id(7), lease_id(7));
    assert_ne!(lease_id(7), lease_id(8));

    let hosts: Vec<(&str, HostState)> = ["h0", "h1", "h2", "h3"]
        .iter()
        .map(|n| (*n, flat_host(CPU_CAP, 8_000)))
        .collect();
    let forward = names(&["h0", "h1", "h2", "h3"]);
    let reverse = names(&["h3", "h2", "h1", "h0"]);
    let mut winners = BTreeSet::new();
    for ix in 0..64u64 {
        let m = model(flat_lease(ix, 1_000), &hosts);
        let want = ["h0", "h1", "h2", "h3"]
            .into_iter()
            .min_by_key(|n| oracle_hash(ix, n))
            .unwrap();
        for order in [&forward, &reverse] {
            let got = ScoreRanker::new()
                .place(&m, order)
                .unwrap()
                .as_str()
                .to_owned();
            assert_eq!(got, want, "lease {ix}");
        }
        winners.insert(want);
    }
    assert!(
        winners.len() >= 3,
        "equal scores spread by lease: {winners:?}"
    );
}

/// Shows: ranker 5 is `Scorer::choose` with the default flags and adds nothing: the same
/// model and candidates give the same worker through the ranker and through a scorer of
/// the default config, for candidates of different load, and an empty list is no choice
/// and counts nothing. Catches a ranker that reorders candidates or holds its own flags.
#[test]
fn ranker_five_is_the_scorers_choose() {
    let hosts: Vec<(&str, HostState)> = ["a", "b", "c"]
        .iter()
        .enumerate()
        .map(|(i, n)| (*n, flat_host(CPU_CAP, 10_000 * (i as u64 + 1))))
        .collect();
    let cands = names(&["a", "b", "c"]);
    for ix in 0..8 {
        let m = model(flat_lease(ix, 5_000), &hosts);
        let mut scorer = Scorer::new(ScoreConfig::default());
        assert_eq!(
            ScoreRanker::new().place(&m, &cands),
            scorer.choose(&m, &cands)
        );
    }
    let m = model(flat_lease(0, 5_000), &hosts);
    let mut ranker = ScoreRanker::new();
    assert_eq!(ranker.place(&m, &[]), None);
    assert_eq!(ranker.tally(), Default::default());
    assert_eq!(ranker.evaluations(), 0);
    // The fuller host that fits goes first: a flat world's best fit.
    assert_eq!(ranker.place(&m, &cands).map(WorkerId::as_str), Some("c"));
}

/// Shows: the adapter hands the scorer a slot structure it can use: the complementary
/// profiles of the design (a host busy early and idle later against a flat host, a lease
/// quiet then busy) go to the host the curves fit, where the load now would pick the other.
/// Catches slots shifted or collapsed to slot 0 by the adapter (slot 0 alone picks `b`).
#[test]
fn complementary_curves_go_through_the_slots() {
    let mut a = [10_000u64; SLOTS];
    a[..5].fill(60_000);
    let b = [30_000u64; SLOTS];
    let mut lease = [24_000u64; SLOTS];
    lease[5..=15].fill(60_000);
    let m = model(
        lease_with(0, lease),
        &[("a", curve_host(CPU_CAP, a)), ("b", curve_host(CPU_CAP, b))],
    );
    let cands = names(&["a", "b"]);
    // `b` is lighter now (30_000 against 60_000) and 2,000 over 88,000 at minutes 5 to 15.
    assert!(m.load(&id("b"), 0).cpu_millis < m.load(&id("a"), 0).cpu_millis);
    assert_eq!(ScoreRanker::new().place(&m, &cands), Some(&cands[0]));
    let key_b = Scorer::default().key(&m, &id("b"));
    assert!(
        key_b.slack() > 0,
        "b is over 100% plus the tolerance at minutes 5 to 15"
    );
    assert_eq!(Scorer::default().key(&m, &id("a")).slack(), 0);
}

// ---- the scorer's own cases, through the adapter ----

fn node() -> Resources {
    Resources::new(8_000, 32 * GIB)
}

/// Loads `worker` with `count` one-core actions by cordoning the others for the round.
fn load(r: &mut Rig, worker: &str, others: &[&str], count: u64) {
    for other in others {
        r.feed(Event::Cordon { worker: w(other) });
    }
    for _ in 0..count {
        r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    }
    let placed: Vec<String> = r
        .tick()
        .iter()
        .map(|s| s.worker.as_str().to_owned())
        .collect();
    assert_eq!(placed, vec![worker.to_owned(); count as usize]);
    for other in others {
        r.feed(Event::Uncordon { worker: w(other) });
    }
}

/// The adapter's host for `name`, built from the scheduler's own view of it. Every
/// action of these cases is one core, so the uncovered count is the uncovered cores.
fn host_of(r: &Rig, name: &str) -> HostState {
    let v = r.s.load_view(&w(name)).expect("registered");
    assert!(v.vm_cpu_millis == 0 && v.vm_memory_bytes == 0);
    let left = Charge {
        cpu_millis: v.left_cpu_millis,
        memory_bytes: v.left_memory_bytes,
    };
    HostState {
        cpu_capacity_millis: 8_000,
        memory_capacity_bytes: 32 * GIB,
        slot0: Slot0 {
            measured_cpu_millis: u64::from(v.measured_cpu_millis.unwrap_or(0)),
            covered_cpu_millis: v.predicted_cpu_millis - v.uncovered_cpu_millis,
            uncovered_cpu_millis: v.uncovered_cpu_millis,
            memory_all_bytes: v.predicted_memory_bytes,
            working_set_bytes: v.working_set_bytes.unwrap_or(0),
            uncovered_memory_bytes: v.uncovered_memory_bytes,
        },
        // The flat predicate charges every slot the same.
        ahead: [left; SLOTS - 1],
        same_key_millis: 0,
        uncovered_leases: u32::try_from(v.uncovered_cpu_millis / 1_000).unwrap(),
    }
}

fn model_of(r: &Rig, hosts: &[&str]) -> ScoreModel {
    let pairs = hosts.iter().map(|n| (id(n), host_of(r, n)));
    ScoreModel::new(flat_lease(0, 1_000), FLOOR_PERMILLE, pairs)
}

/// Asserts `model.load(w, 0)` equals the left side the scheduler computed for `name`.
fn assert_slot_zero(r: &Rig, m: &ScoreModel, name: &str) {
    let v = r.s.load_view(&w(name)).expect("registered");
    assert_eq!(
        m.load(&id(name), 0),
        Charge {
            cpu_millis: v.left_cpu_millis,
            memory_bytes: v.left_memory_bytes
        },
        "slot 0 of {name}"
    );
}

/// Shows: the scorer's case "a lease goes to the fuller worker that fits" gives the same
/// worker through the adapter as through the scheduler: `a` holds four cores, `b` none, and
/// the lease goes to `a`; the adapter's slot 0 equals the scheduler's left side on both.
/// Catches an adapter that disagrees with the predicate's own figures.
#[test]
fn conformance_the_fuller_worker_that_fits() {
    let mut r = Rig::on();
    r.up("a", node());
    r.up("b", node());
    load(&mut r, "a", &["b"], 4);
    r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    let m = model_of(&r, &["a", "b"]);
    assert_slot_zero(&r, &m, "a");
    assert_slot_zero(&r, &m, "b");
    assert_eq!(m.load(&id("a"), 0).cpu_millis, 4_000);
    let cands = names(&["a", "b"]);
    let ours = ScoreRanker::new().place(&m, &cands).map(WorkerId::as_str);
    let theirs: Vec<String> = r
        .tick()
        .iter()
        .map(|s| s.worker.as_str().to_owned())
        .collect();
    assert_eq!(theirs, ["a"]);
    assert_eq!(ours, Some("a"));
}

/// Shows: the scorer's case "a worker measured above its predictions loses" through the
/// adapter: with readings, the left side includes the measurement (2,500 and 2,000 against
/// a floor of 1,500), the host measured above its covered sum is distrusted, the lease
/// goes to the other, as the scheduler's does, and the counter records both measurements
/// winning. Catches a slot 0 that ignores the reading and a missing `measured > covered`.
#[test]
fn conformance_measured_above_predictions_loses_and_the_measurement_wins() {
    let mut r = Rig::on();
    r.up("a", node());
    r.up("b", node());
    load(&mut r, "a", &["b"], 2);
    load(&mut r, "b", &["a"], 2);
    let (held_a, held_b) = (r.held("a"), r.held("b"));
    for _ in 0..2 {
        r.window("a", 2_500, GIB, &held_a);
        r.window("b", 2_000, GIB, &held_b);
    }
    r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    let m = model_of(&r, &["a", "b"]);
    assert_slot_zero(&r, &m, "a");
    assert_slot_zero(&r, &m, "b");
    assert_eq!(
        m.load(&id("a"), 0).cpu_millis,
        2_500,
        "the measurement, not the 2,000 predicted"
    );
    assert!(m.host(&id("a")).measured_above_predicted);
    assert!(!m.host(&id("b")).measured_above_predicted);
    let cands = names(&["a", "b"]);
    let mut ranker = ScoreRanker::new();
    let ours = ranker.place(&m, &cands).map(WorkerId::as_str);
    assert_eq!(ranker.tally().measurement_wins, 2);
    let theirs: Vec<String> = r
        .tick()
        .iter()
        .map(|s| s.worker.as_str().to_owned())
        .collect();
    assert_eq!(theirs, ["b"]);
    assert_eq!(ours, Some("b"));
}

/// Shows: when the readings are below the floor the floor is the slot-0 CPU and the counter
/// stays at zero: both hosts read 500 against covered leases predicted at 2,000 (floor
/// 1,500), and the adapter's left side is the scheduler's 1,500. Catches a slot 0 that
/// takes the measurement when it is the smaller term.
#[test]
fn conformance_the_floor_binds_when_the_reading_is_low() {
    let mut r = Rig::on();
    r.up("a", node());
    r.up("b", node());
    load(&mut r, "a", &["b"], 2);
    load(&mut r, "b", &["a"], 2);
    let (held_a, held_b) = (r.held("a"), r.held("b"));
    for _ in 0..2 {
        r.window("a", 500, GIB, &held_a);
        r.window("b", 500, GIB, &held_b);
    }
    r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    let m = model_of(&r, &["a", "b"]);
    assert_slot_zero(&r, &m, "a");
    assert_slot_zero(&r, &m, "b");
    assert_eq!(m.load(&id("a"), 0).cpu_millis, 1_500);
    let mut ranker = ScoreRanker::new();
    assert!(ranker.place(&m, &names(&["a", "b"])).is_some());
    assert_eq!(ranker.tally().measurement_wins, 0);
}

/// Shows: a ranker built with a config runs that config: a fuller host holding 26% of its
/// CPU in the lease's key loses to an empty one at the default 25% key share and wins when
/// the share is the whole host. Catches `with_config` dropping its config.
#[test]
fn a_ranker_with_a_config_uses_it() {
    let mut a = flat_host(10_000, 4_000);
    a.same_key_millis = 2_000;
    let b = flat_host(10_000, 0);
    let m = model(flat_lease(0, 600), &[("a", a), ("b", b)]);
    let cands = names(&["a", "b"]);
    assert_eq!(ScoreRanker::new().place(&m, &cands), Some(&cands[1]));
    let wide = ScoreConfig::new(1, 10_000, 1_000_000).expect("balance in range");
    assert_eq!(
        ScoreRanker::with_config(wide).place(&m, &cands),
        Some(&cands[0])
    );
}
