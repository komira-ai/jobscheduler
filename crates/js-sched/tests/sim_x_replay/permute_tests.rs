//! Tests of the control's permutation of belief minutes and of the movable share.
//!
//! Each test names the defect it would catch. None reads a clock or asserts a timing.

use std::collections::BTreeSet;

use crate::engine::fixture::{Fixture, KeyIx, Outcome, Row};
use crate::engine::permute::{
    DID_ANYTHING_FLOOR_PPM, Movable, movable, permutation, permute_belief,
};
use crate::engine::predictor::{Belief, Observed, Predictor, Tail};
use crate::engine::truth::{self, TruthConfig};

const MINUTE_MS: u64 = 60_000;

fn ok(start_ms: u64, key: u16, wall_ms: u64) -> Row {
    Row {
        start_ms,
        key: KeyIx(key),
        wall_ms,
        cpu_ms: wall_ms,
        peak_mem_mib: 1,
        outcome: Outcome::Ok,
    }
}

/// Rows of a key whose ring shows `life` minutes: three runs reaching minute `life - 1`,
/// then a probe lease that sees all three. `base` offsets the key's rows in time.
fn key_rows(key: u16, life: u8, base: u64) -> Vec<Row> {
    let wall = u64::from(life - 1) * MINUTE_MS + 500;
    let probe = base + u64::from(life) * MINUTE_MS + 10_000;
    vec![
        ok(base, key, wall),
        ok(base + 1_000, key, wall),
        ok(base + 2_000, key, wall),
        ok(probe, key, wall),
    ]
}

/// A warm belief of life `life` over the given curves (every ring run shows them).
fn warm(cores: &[u64], mem: &[u64], life: u8) -> Belief {
    let rows = key_rows(0, life, 0);
    let (c, m) = (cores.to_vec(), mem.to_vec());
    let p = Predictor::build(&rows, |_, _| {
        Observed::new(c.clone(), m.clone()).expect("a series")
    });
    let b = p.belief(3).clone();
    assert!(!b.is_cold());
    assert_eq!(b.life_minutes(), life);
    b
}

/// The curve values the control's reader sees at minutes `0..n`.
fn reads(b: &Belief, n: usize) -> Vec<(u64, u64)> {
    (0..n)
        .map(|t| (b.cores_milli_at(t, Tail::Held), b.mem_mib_at(t, Tail::Held)))
        .collect()
}

/// A series of `n` minutes with every (cores, memory) pair distinct.
fn distinct(n: usize) -> (Vec<u64>, Vec<u64>) {
    (
        (0..n as u64).map(|t| 1_000 + 7 * t * t).collect(),
        (0..n as u64).map(|t| 50 + 13 * t).collect(),
    )
}

// ---- the permutation ----------------------------------------------------------------

#[test]
fn the_permutation_is_a_bijection_on_zero_to_l() {
    // Defect: a swap index out of range, a draw that duplicates a minute or drops one.
    for seed in 1..=4 {
        for key in [0u16, 1, 7, 65_535] {
            for life in 0..=40 {
                let perm = permutation(seed, KeyIx(key), life);
                assert_eq!(perm.len(), life);
                let set: BTreeSet<usize> = perm.iter().copied().collect();
                assert_eq!(set, (0..life).collect(), "seed {seed} key {key} L {life}");
            }
        }
    }
}

#[test]
fn the_permutation_is_pinned() {
    // Defect: a change to the swap rule, the domain or the argument order of the keyed
    // draw, which would move every shuffled world without a test going red.
    let at = |seed, key, life| permutation(seed, KeyIx(key), life);
    assert_eq!(at(1, 0, 2), [0, 1]);
    assert_eq!(at(2, 0, 2), [1, 0]);
    assert_eq!(at(1, 0, 3), [0, 2, 1]);
    assert_eq!(at(1, 2, 5), [1, 0, 3, 4, 2]);
    assert_eq!(at(2, 1, 5), [3, 0, 4, 1, 2]);
    assert_eq!(at(3, 0, 5), [4, 3, 0, 2, 1]);
}

#[test]
fn the_permutation_depends_on_the_seed_and_on_the_key() {
    // Defect: the seed ignored (four seeds would be one draw and the control's seed mean
    // would average nothing), or every key shuffled alike.
    let key = KeyIx(9);
    let by_seed: BTreeSet<Vec<usize>> = (1..=4).map(|s| permutation(s, key, 30)).collect();
    assert_eq!(by_seed.len(), 4, "four seeds, four draws");
    let by_key: BTreeSet<Vec<usize>> = (0..8).map(|k| permutation(1, KeyIx(k), 30)).collect();
    assert_eq!(by_key.len(), 8, "eight keys, eight draws");
    assert_eq!(permutation(3, key, 30), permutation(3, key, 30));
}

#[test]
fn a_life_of_one_or_less_is_the_identity() {
    // Defect: a one-minute life shuffled to a minute that does not exist.
    for seed in 1..=4 {
        assert!(permutation(seed, KeyIx(3), 0).is_empty());
        assert_eq!(permutation(seed, KeyIx(3), 1), [0]);
    }
    let one = warm(&[900], &[77], 1);
    for seed in 1..=4 {
        assert_eq!(permute_belief(&one, seed, KeyIx(3)), one);
    }
    // A cold belief is its own: the same flat guess, the same cached curve.
    let rows = vec![ok(0, 0, 1_000)];
    let cold = Predictor::build(&rows, |_, r| Observed::flat(r))
        .belief(0)
        .clone();
    assert!(cold.is_cold());
    let out = permute_belief(&cold, 2, KeyIx(0));
    assert_eq!(out, cold);
    assert!(out.shares_curves_with(&cold));
}

// ---- the belief ---------------------------------------------------------------------

#[test]
fn minutes_from_l_on_and_the_h_plus_slot_are_untouched() {
    // Defect: a shuffle over the whole stored curve, which would also move the held tail
    // the predicate reads past the fit window and the H+ slot (31 minutes on).
    for life in [2u8, 5, 29, 30] {
        let (c, m) = distinct(40);
        let before = warm(&c, &m, life);
        for seed in 1..=4 {
            let after = permute_belief(&before, seed, KeyIx(5));
            let l = usize::from(life);
            assert_eq!(
                reads(&after, 80)[l..],
                reads(&before, 80)[l..],
                "L {life} seed {seed}"
            );
            assert_eq!(
                after.cores_milli_at(31, Tail::Held),
                before.cores_milli_at(31, Tail::Held)
            );
            assert_eq!(
                after.mem_mib_at(31, Tail::Held),
                before.mem_mib_at(31, Tail::Held)
            );
            assert_eq!(after.life_minutes(), before.life_minutes());
            assert_eq!(after.p_bar_cores_milli(), before.p_bar_cores_milli());
            assert_eq!(after.p_bar_mem_mib(), before.p_bar_mem_mib());
            assert!(!after.is_cold());
        }
    }
}

#[test]
fn a_curve_shorter_than_its_life_still_reads_the_same_past_l() {
    // Defect: the shuffle reading or writing off the end of a short curve, or the held tail
    // changing once the stored points are rewritten (the last point is no longer the
    // original one).
    let (c, m) = distinct(3);
    let before = warm(&c, &m, 6);
    for seed in 1..=4 {
        let after = permute_belief(&before, seed, KeyIx(1));
        assert_eq!(reads(&after, 40)[6..], reads(&before, 40)[6..]);
        let mut got = reads(&after, 6);
        let mut want = reads(&before, 6);
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want);
    }
}

#[test]
fn the_area_is_preserved_per_key() {
    // Defect: a shuffle that duplicates a minute (the sum moves) or scales a value.
    let (c, m) = distinct(30);
    let before = warm(&c, &m, 30);
    for seed in 1..=4 {
        let after = permute_belief(&before, seed, KeyIx(2));
        let sorted = |b: &Belief, pick: fn(&(u64, u64)) -> u64| {
            let mut v: Vec<u64> = reads(b, 30).iter().map(pick).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(sorted(&after, |p| p.0), sorted(&before, |p| p.0));
        assert_eq!(sorted(&after, |p| p.1), sorted(&before, |p| p.1));
        let area = |b: &Belief| {
            (0..30)
                .map(|t| b.cores_milli_at(t, Tail::Held))
                .sum::<u64>()
        };
        assert_eq!(area(&after), area(&before));
        assert_ne!(
            reads(&after, 30),
            reads(&before, 30),
            "seed {seed} moved something"
        );
    }
}

#[test]
fn cores_and_memory_are_permuted_jointly() {
    // Defect: two draws, one for each curve: a minute's cores would no longer sit beside
    // the memory the key had at that minute.
    let (c, m) = distinct(30);
    let before = warm(&c, &m, 30);
    let pair_at = |t: usize| (c[t], m[t]);
    for seed in 1..=4 {
        let after = permute_belief(&before, seed, KeyIx(4));
        let perm = permutation(seed, KeyIx(4), 30);
        for (t, source) in perm.iter().enumerate() {
            let got = (
                after.cores_milli_at(t, Tail::Held),
                after.mem_mib_at(t, Tail::Held),
            );
            assert_eq!(got, pair_at(*source), "seed {seed} minute {t}");
        }
    }
}

// ---- the movable share --------------------------------------------------------------

/// Rows and beliefs of keys, each `(key, cores curve, memory curve)` of life 2: three ring
/// runs and a probe each, so one warm lease per key and three cold ones.
fn hand(keys: &[(u16, [u64; 2], [u64; 2])]) -> (Vec<Row>, Predictor) {
    let mut rows = Vec::new();
    for (i, (key, ..)) in keys.iter().enumerate() {
        rows.extend(key_rows(*key, 2, i as u64 * 10_000_000));
    }
    let p = Predictor::build(&rows, |_, r| {
        let (_, c, m) = keys
            .iter()
            .find(|(k, ..)| *k == r.key.0)
            .expect("a known key");
        Observed::new(c.to_vec(), m.to_vec()).expect("a series")
    });
    (rows, p)
}

#[test]
fn the_movable_share_counts_what_a_swap_changes_and_what_an_identity_does_not() {
    // Seed 1 over two minutes: key 0 draws the identity, key 2 the swap (pinned above).
    // Defect: counting every warm lease as moved (a draw that leaves the curve alone would
    // count), or a flat curve counted because a swap happened.
    let shaped = ([100, 300], [10, 10]);
    let flat = ([200, 200], [10, 10]);
    let (rows, p) = hand(&[
        (0, shaped.0, shaped.1),
        (2, shaped.0, shaped.1),
        (3, flat.0, flat.1),
    ]);
    assert_eq!(permutation(1, KeyIx(3), 2).len(), 2);
    let got = movable(&rows, &p, 1);
    assert_eq!(
        got,
        Movable {
            leases: 12,
            warm: 3,
            moved: 1,
            core_milli_minutes: 1_200,
            moved_core_milli_minutes: 400,
            cores_moved_core_milli_minutes: 400,
        }
    );
    assert_eq!(got.lease_ppm(), 83_333);
    assert_eq!(got.core_minute_ppm(), 333_333);
}

#[test]
fn a_minute_moves_when_either_curve_changes() {
    // Seed 2 swaps both minutes of keys 0 to 2 (pinned above). Key 0 differs in memory
    // only, key 1 in nothing, key 2 in cores only. Defect: a share that looks at cores
    // alone (key 0 missed) or at memory alone (key 2 missed), or that counts a swap of
    // equal pairs.
    let (rows, p) = hand(&[
        (0, [200, 200], [10, 20]),
        (1, [200, 200], [10, 10]),
        (2, [100, 300], [10, 10]),
    ]);
    for k in 0..3 {
        assert_eq!(permutation(2, KeyIx(k), 2), [1, 0]);
    }
    let got = movable(&rows, &p, 2);
    assert_eq!((got.warm, got.moved), (3, 2));
    assert_eq!(
        (got.core_milli_minutes, got.moved_core_milli_minutes),
        (1_200, 800)
    );
    assert_eq!(got.core_minute_ppm(), 666_666);
    // The cores alone move only in key 2.
    assert_eq!(got.cores_moved_core_milli_minutes, 400);
    assert_eq!(got.cores_only_ppm(), 333_333);
}

#[test]
fn the_share_agrees_with_what_permute_belief_returns() {
    // Defect: the share and the control it describes computed by two different readings of
    // the permutation. Brute force over the beliefs the control would read.
    let rows = Fixture::embedded().expect("loads").rows;
    let rows = &rows[..rows.len().min(4_000)];
    let p = Predictor::build(rows, |_, r| {
        let n = usize::try_from(r.wall_ms / MINUTE_MS).unwrap_or(0) + 1;
        let cores = (0..n as u64)
            .map(|t| r.cpu_ms % 997 + 31 * (t % 5))
            .collect();
        let mem = (0..n as u64)
            .map(|t| r.peak_mem_mib + 3 * (t % 4))
            .collect();
        Observed::new(cores, mem).expect("a series")
    });
    for seed in [1, 2] {
        let mut want = Movable::default();
        for (i, row) in rows.iter().enumerate() {
            let before = p.belief(i);
            want.leases += 1;
            if before.is_cold() {
                continue;
            }
            want.warm += 1;
            let after = permute_belief(before, seed, row.key);
            let l = usize::from(before.life_minutes());
            let (a, b) = (reads(&after, l), reads(&before.clone(), l));
            if a != b {
                want.moved += 1;
            }
            for (x, y) in a.iter().zip(&b) {
                want.core_milli_minutes += u128::from(y.0);
                if x != y {
                    want.moved_core_milli_minutes += u128::from(y.0);
                }
                if x.0 != y.0 {
                    want.cores_moved_core_milli_minutes += u128::from(y.0);
                }
            }
        }
        assert_eq!(movable(rows, &p, seed), want, "seed {seed}");
        assert!(want.moved > 0 && want.warm > want.moved);
    }
}

#[test]
fn an_empty_world_has_no_share() {
    // Defect: a division by zero on an empty trace.
    let p = Predictor::build(&[], |_, r| Observed::flat(r));
    let got = movable(&[], &p, 1);
    assert_eq!(got, Movable::default());
    assert_eq!((got.lease_ppm(), got.core_minute_ppm()), (0, 0));
}

// ---- the committed trace ------------------------------------------------------------

/// The share the permutation moves in the committed trace for world and permutation seed
/// `seed`: truth with the default jittered shapes, the learner on the trace clock, each run
/// observed as its own minute series.
fn measure(seed: u64) -> Movable {
    let fixture = Fixture::embedded().expect("loads");
    measure_rows(&fixture.rows, &TruthConfig::new(seed))
}

fn measure_rows(rows: &[Row], cfg: &TruthConfig) -> Movable {
    let seed = cfg.seed;
    let runs = truth::build(rows, cfg);
    let p = Predictor::build(rows, |i, _| {
        let run = &runs[i];
        let cores = (0..run.minutes())
            .map(|m| run.minute_millicores(m))
            .collect();
        let mem = (0..run.minutes())
            .map(|m| run.minute_working_set_mib(m))
            .collect();
        Observed::new(cores, mem).expect("a run touches a minute")
    });
    movable(rows, &p, seed)
}

#[test]
fn the_measured_share_is_above_the_registered_floor() {
    // Defect: a control that shuffles (almost) nothing, which would pass "the gain
    // vanishes" for the wrong reason, and a registered floor that no seed reaches. Prints
    // the measure the floor was registered from.
    for seed in 1..=4 {
        let got = measure(seed);
        eprintln!(
            "seed {seed}: leases {} warm {} moved {} ({} ppm of leases); core-minutes moved {} \
             ppm ({} of {} milli), by cores alone {} ppm",
            got.leases,
            got.warm,
            got.moved,
            got.lease_ppm(),
            got.core_minute_ppm(),
            got.moved_core_milli_minutes,
            got.core_milli_minutes,
            got.cores_only_ppm(),
        );
        assert!(
            got.core_minute_ppm() >= DID_ANYTHING_FLOOR_PPM,
            "seed {seed}: {} ppm under the floor {DID_ANYTHING_FLOOR_PPM}",
            got.core_minute_ppm()
        );
    }
    // A floor under half of 10% would mean the measure was under the 10% rule.
    const { assert!(DID_ANYTHING_FLOOR_PPM >= 50_000) };
}

#[test]
fn the_ci_subsample_moves_as_much_as_the_floor_asks() {
    // Defect: a floor registered on the whole day that the sampled keys (key index below
    // 16 of every 100, the pool the asserted runs use) do not reach.
    let fixture = Fixture::embedded().expect("loads");
    let rows: Vec<Row> = fixture
        .rows
        .iter()
        .filter(|r| r.key.0 % 100 < 16)
        .copied()
        .collect();
    let got = measure_rows(&rows, &TruthConfig::new(1));
    eprintln!(
        "subsample: {} leases, {} warm, {} ppm of core-minutes move, {} by cores alone",
        got.leases,
        got.warm,
        got.core_minute_ppm(),
        got.cores_only_ppm()
    );
    assert!(got.warm > 0 && got.core_minute_ppm() >= DID_ANYTHING_FLOOR_PPM);
}
