//! Tests of the learner: what a lease is told to expect of its key.
//!
//! Each test names the defect it would catch. None reads a clock or asserts a timing.

use crate::engine::fixture::{Fixture, KeyIx, Outcome, Row, fnv1a};
use crate::engine::predictor::{
    COLD_CORES_MILLI, COLD_MEM_MIB, LIFE_HORIZON, MIN_RING, Observed, Predictor, RING, Tail,
    life_minutes_standin, mean_cores_milli, minutes_reached, nearest_rank_p90,
};

const MINUTE_MS: u64 = 60_000;

fn row(start_ms: u64, key: u16, wall_ms: u64, cpu_ms: u64, mem: u64, outcome: Outcome) -> Row {
    Row {
        start_ms,
        key: KeyIx(key),
        wall_ms,
        cpu_ms,
        peak_mem_mib: mem,
        outcome,
    }
}

fn ok(start_ms: u64, key: u16, wall_ms: u64, cpu_ms: u64, mem: u64) -> Row {
    row(start_ms, key, wall_ms, cpu_ms, mem, Outcome::Ok)
}

/// Beliefs with every run shown as flat.
fn flat(rows: &[Row]) -> Predictor {
    Predictor::build(rows, |_, r| Observed::flat(r))
}

fn bytes_of(p: &Predictor, ix: usize) -> Vec<u8> {
    let mut out = Vec::new();
    p.belief(ix).encode(&mut out);
    out
}

/// `n` one-second runs of key 0, two seconds apart, with the given mean cores (millicores)
/// and peak memory, then one probe lease of the key a minute after the last. The probe is
/// the last row.
fn ring_of(cores_milli: &[u64], mem_mib: &[u64]) -> Vec<Row> {
    assert_eq!(cores_milli.len(), mem_mib.len());
    let mut rows: Vec<Row> = cores_milli
        .iter()
        .zip(mem_mib)
        .enumerate()
        .map(|(i, (c, m))| ok(i as u64 * 2_000, 0, 1_000, *c, *m))
        .collect();
    rows.push(ok(
        cores_milli.len() as u64 * 2_000 + MINUTE_MS,
        0,
        1_000,
        1,
        1,
    ));
    rows
}

// ---- life_minutes_standin: the documented vectors of the function it stands for --------

/// The `u8` a run of `minutes` whole minutes plus a few seconds shows the ring.
fn reaching(minutes: u64) -> u8 {
    u8::try_from(minutes).unwrap_or(u8::MAX)
}

#[test]
fn life_is_the_leading_minutes_at_least_half_of_the_runs_reached() {
    // Defect: the count taken from the longest run (or the mean), or from a minute only
    // one run reached. Runs reaching 0, 3, 3, 5 give 4: minutes 0 to 3 were reached by at
    // least two of four, minute 4 by one.
    assert_eq!(life_minutes_standin(&[0, 3, 3, 5]), 4);
    // Minute 1 is reached by 2 of 4: exactly half counts; minute 2 by none does not.
    assert_eq!(life_minutes_standin(&[0, 0, 1, 1]), 2);
    assert_eq!(life_minutes_standin(&[0, 0, 0, 1]), 1);
}

#[test]
fn runs_under_a_minute_and_an_empty_ring_have_a_life_of_one() {
    // Defect: runs under a minute reporting more than 1, an empty ring reporting 0.
    assert_eq!(life_minutes_standin(&[0, 0, 0]), 1);
    assert_eq!(life_minutes_standin(&[]), 1);
}

#[test]
fn life_is_clamped_to_the_horizon() {
    // Defect: no upper clamp, or the clamp at the wrong end: 29 minutes still gives 30
    // (minute 29 is reached), 28 gives 29.
    assert_eq!(life_minutes_standin(&[reaching(120); 6]), 30);
    assert_eq!(life_minutes_standin(&[reaching(1_000); 6]), 30);
    assert_eq!(life_minutes_standin(&[reaching(29); 6]), 30);
    assert_eq!(life_minutes_standin(&[reaching(28); 6]), 29);
    assert_eq!(LIFE_HORIZON, 30);
}

#[test]
fn life_follows_the_last_32_runs() {
    // Defect: a ring that keeps old runs for ever. The window is the last 32 of the
    // runs seen, as the vectors of the sizing summary slide it.
    let window = |runs: &[u8]| life_minutes_standin(&runs[runs.len().saturating_sub(32)..]);
    let mut runs = vec![10u8; 32];
    assert_eq!(window(&runs), 11);
    runs.extend([0u8; 15]);
    assert_eq!(window(&runs), 11, "17 of 32 still reach minute 10");
    runs.push(0);
    assert_eq!(window(&runs), 11, "16 of 32 is exactly half");
    runs.push(0);
    assert_eq!(window(&runs), 1, "15 of 32 is under half");
}

#[test]
fn the_ring_of_the_predictor_slides_the_same_way() {
    // The same vectors through the predictor: a run reaches minute 10 when its wall is
    // 10 minutes plus a few seconds. Defect: the ring keeping more than 32 runs, or
    // dropping the newest instead of the oldest, or reading wall time in the wrong unit.
    let long = |i: u64| ok(i * 1_000_000, 0, 10 * MINUTE_MS + 5_000, 1_000_000, 100);
    let short = |i: u64| ok(i * 1_000_000, 0, 5_000, 1_000, 100);
    let probe = |i: u64| ok(i * 1_000_000, 0, 1_000, 1_000, 100);
    let life_after = |longs: u64, shorts: u64| {
        let mut rows: Vec<Row> = (0..longs).map(long).collect();
        rows.extend((longs..longs + shorts).map(short));
        rows.push(probe(longs + shorts));
        let p = flat(&rows);
        p.belief(rows.len() - 1).life_minutes()
    };
    assert_eq!(life_after(32, 0), 11);
    assert_eq!(life_after(32, 15), 11);
    assert_eq!(life_after(32, 16), 11);
    assert_eq!(life_after(32, 17), 1);
}

// ---- cold and warm ---------------------------------------------------------------------

#[test]
fn a_key_is_cold_below_three_runs_and_warm_at_three() {
    // Defect: the threshold off by one in either direction. A cold lease is charged a
    // flat 2 cores and 4 GiB at every minute and has L = 1.
    let rows = vec![
        ok(0, 0, 1_000, 500, 100),
        ok(10_000, 0, 1_000, 700, 200),
        ok(20_000, 0, 1_000, 900, 300),
        ok(30_000, 0, 1_000, 100, 50),
    ];
    let p = flat(&rows);
    let cold: Vec<bool> = (0..4).map(|i| p.belief(i).is_cold()).collect();
    assert_eq!(cold, [true, true, true, false]);
    assert_eq!(MIN_RING, 3);
    let c = p.belief(2);
    assert_eq!(c.life_minutes(), 1);
    assert_eq!(c.p_bar_cores_milli(), COLD_CORES_MILLI);
    assert_eq!(c.p_bar_mem_mib(), COLD_MEM_MIB);
    assert_eq!((COLD_CORES_MILLI, COLD_MEM_MIB), (2_000, 4_096));
    for minute in [0, 1, 29, 30, 500] {
        assert_eq!(c.cores_milli_at(minute, Tail::Held), 2_000);
        assert_eq!(c.mem_mib_at(minute, Tail::Held), 4_096);
        // A cold belief has no known end, so it is not dropped.
        assert_eq!(c.cores_milli_at(minute, Tail::Eol), 2_000);
        assert_eq!(c.mem_mib_at(minute, Tail::Eol), 4_096);
    }
    assert_eq!(c.curve_len(), 1);
    let w = p.belief(3);
    assert!(!w.is_cold());
    assert_eq!(w.p_bar_cores_milli(), 900);
    assert_eq!(w.p_bar_mem_mib(), 300);
}

#[test]
fn a_lease_sees_only_runs_that_ended_strictly_before_it_started() {
    // Defect: a run visible at the instant it ends (look-ahead by one tick), or one
    // that becomes visible a tick late. Runs of 1 s start every second, so the lease
    // starting at 3,000 ms sees the runs that ended at 1,000 and 2,000 (cold) and not the
    // one ending at 3,000; the one at 4,000 sees three.
    let rows: Vec<Row> = (0..5).map(|i| ok(i * 1_000, 0, 1_000, 1_000, 10)).collect();
    let p = flat(&rows);
    let cold: Vec<bool> = (0..5).map(|i| p.belief(i).is_cold()).collect();
    assert_eq!(cold, [true, true, true, true, false]);
}

#[test]
fn keys_do_not_see_each_other() {
    // Defect: a ring shared by all keys. Key 1's runs never warm key 0.
    let mut rows: Vec<Row> = (0..5)
        .map(|i| ok(i * 10_000, 1, 1_000, 1_000, 10))
        .collect();
    rows.push(ok(100_000, 0, 1_000, 1_000, 10));
    rows.sort_by_key(|r| r.start_ms);
    let p = flat(&rows);
    let last = rows.len() - 1;
    assert_eq!(rows[last].key, KeyIx(0));
    assert!(p.belief(last).is_cold());
    assert!(!p.belief(last - 1).is_cold());
}

#[test]
fn runs_that_did_not_end_ok_enter_no_ring() {
    // Defect: a killed, timed-out or evicted run teaching the learner its truncated
    // numbers. Three such runs leave the key cold; a warm ring is not moved by them.
    let mut rows = vec![
        row(0, 0, 1_000, 9_000, 9_000, Outcome::KilledMemory),
        row(10_000, 0, 1_000, 9_000, 9_000, Outcome::TimedOut),
        row(20_000, 0, 1_000, 9_000, 9_000, Outcome::Evicted),
        ok(30_000, 0, 1_000, 100, 10),
    ];
    let p = flat(&rows);
    assert!(p.belief(3).is_cold());
    for (i, base) in [100, 200, 300].into_iter().enumerate() {
        rows.push(ok(100_000 + i as u64 * 10_000, 0, 1_000, base, base / 10));
    }
    rows.push(row(
        140_000,
        0,
        1_000,
        99_000,
        99_000,
        Outcome::KilledMemory,
    ));
    rows.push(ok(150_000, 0, 1_000, 1, 1));
    let p = flat(&rows);
    // The ring of the last lease: the ok run at 30,000 and the three at 100,000 and
    // after. The killed run at 140,000 is not in it.
    let last = p.belief(rows.len() - 1);
    assert!(!last.is_cold());
    assert_eq!(last.p_bar_cores_milli(), 300);
    assert_eq!(last.p_bar_mem_mib(), 30);
}

// ---- P90, nearest rank -----------------------------------------------------------------

#[test]
fn nearest_rank_p90_picks_the_ceil_of_nine_tenths() {
    // Defect: interpolation, rank = floor, the maximum, or an index off by one. Ranks:
    // n = 1..=10 -> ceil(0.9 n) = 1,2,3,4,5,6,7,8,9,9; n = 11 -> 10; n = 32 -> 29.
    let ranks = [1, 2, 3, 4, 5, 6, 7, 8, 9, 9, 10];
    for (n, rank) in (1..=11).zip(ranks) {
        let mut v: Vec<u64> = (1..=n).rev().map(|x| x * 100).collect();
        assert_eq!(nearest_rank_p90(&mut v), rank * 100, "n = {n}");
    }
    let mut v: Vec<u64> = (1..=32).rev().collect();
    assert_eq!(nearest_rank_p90(&mut v), 29);
    assert_eq!(nearest_rank_p90(&mut []), 0);
    let mut dup = [5, 5, 5];
    assert_eq!(nearest_rank_p90(&mut dup), 5);
}

#[test]
fn p_bar_is_the_p90_of_mean_cores_and_of_peak_memory_separately() {
    // Defect: p_bar from the mean, from the maximum, or memory taken from the run that
    // gave the cores. Eleven runs: cores 100..1100 rising, memory falling.
    let cores: Vec<u64> = (1..=11).map(|x| x * 100).collect();
    let mem: Vec<u64> = (1..=11).rev().map(|x| x * 10).collect();
    let p = flat(&ring_of(&cores, &mem));
    let b = p.belief(11);
    assert_eq!(b.p_bar_cores_milli(), 1_000);
    assert_eq!(b.p_bar_mem_mib(), 100);
    // Three runs: the maximum of the three (rank ceil(2.7) = 3).
    let p = flat(&ring_of(&[300, 100, 200], &[5, 7, 6]));
    assert_eq!(p.belief(3).p_bar_cores_milli(), 300);
    assert_eq!(p.belief(3).p_bar_mem_mib(), 7);
}

#[test]
fn mean_cores_are_cpu_over_wall_in_millicores() {
    // Defect: the wrong unit, a rounding up, or a division by a zero wall.
    assert_eq!(mean_cores_milli(&ok(0, 0, 1_000, 4_500, 1)), 4_500);
    assert_eq!(mean_cores_milli(&ok(0, 0, 3_000, 1_000, 1)), 333);
    assert_eq!(mean_cores_milli(&ok(0, 0, 90_000, 1_000, 1)), 11);
    assert_eq!(mean_cores_milli(&ok(0, 0, 1, u64::MAX, 1)), u64::MAX);
    assert_eq!(mean_cores_milli(&ok(0, 0, 0, 5, 1)), 5_000);
    assert_eq!(minutes_reached(&ok(0, 0, 59_999, 1, 1)), 0);
    assert_eq!(minutes_reached(&ok(0, 0, 60_000, 1, 1)), 1);
    assert_eq!(minutes_reached(&ok(0, 0, 400 * MINUTE_MS, 1, 1)), 255);
}

// ---- the curve -------------------------------------------------------------------------

/// Eleven runs of key 0 with series of 1, 2 (nine runs) and 3 minutes, then a probe.
/// Run `k` has cores `100 (k + 1)` at minute 0, `1000 (k + 1)` at minute 1 and 77 at
/// minute 2, and working sets `10 (11 - k)`, `5 (k + 1)` and 999.
fn shaped() -> (Vec<Row>, Predictor) {
    let mut rows: Vec<Row> = (0..11).map(|k| ok(k * 10_000, 0, 1_000, 1, 1)).collect();
    rows.push(ok(1_000_000, 0, 1_000, 1, 1));
    let p = Predictor::build(&rows, |i, _| {
        let k = i as u64;
        let len = match k {
            0 => 1,
            10 => 3,
            _ => 2,
        };
        let cores = [100 * (k + 1), 1_000 * (k + 1), 77];
        let ws = [10 * (11 - k), 5 * (k + 1), 999];
        Observed::new(cores[..len].to_vec(), ws[..len].to_vec()).expect("a series")
    });
    (rows, p)
}

#[test]
fn a_curve_point_is_the_p90_over_the_runs_that_reached_that_minute() {
    // Defect: a point taken over all runs (a run that did not reach the minute counted
    // as zero), the maximum, a sub-minute run dropped, or cores and working set mixed up.
    // Minute 0: 11 runs, rank 10 -> k = 9 -> 1000. Minute 1: 10 runs (k = 1..=10),
    // rank 9 -> k = 9 -> 10,000. Minute 2: one run -> 77.
    let (rows, p) = shaped();
    let b = p.belief(rows.len() - 1);
    assert_eq!(b.curve_len(), 3);
    let cores: Vec<u64> = (0..3).map(|t| b.cores_milli_at(t, Tail::Held)).collect();
    assert_eq!(cores, [1_000, 10_000, 77]);
    // Memory minute 0: 10 (11 - k), k = 0..=10 -> 110, 100, ..., 10; rank 10 -> 100.
    // Minute 1: 5 (k + 1) for k = 1..=10 -> 10..=55; rank 9 -> 50. Minute 2: 999.
    let mem: Vec<u64> = (0..3).map(|t| b.mem_mib_at(t, Tail::Held)).collect();
    assert_eq!(mem, [100, 50, 999]);
}

#[test]
fn the_last_point_is_held_past_the_curve() {
    // Defect: reading past the end (a panic), or zero where the curve stops.
    let (rows, p) = shaped();
    let b = p.belief(rows.len() - 1);
    for minute in [3, 4, 30, 31, 10_000] {
        assert_eq!(b.cores_milli_at(minute, Tail::Held), 77);
        assert_eq!(b.mem_mib_at(minute, Tail::Held), 999);
    }
}

#[test]
fn eol_drops_a_warm_belief_to_zero_from_l_and_held_does_not() {
    // Defect: the two tails the same; eol dropping at L - 1 or at L + 1. Four runs of
    // 2 min 30 s reach minute 2, so L = 3 and the curve has three points.
    let rows = vec![
        ok(0, 0, 150_000, 150_000, 64),
        ok(200_000, 0, 150_000, 150_000, 64),
        ok(400_000, 0, 150_000, 150_000, 64),
        ok(600_000, 0, 150_000, 150_000, 64),
        ok(1_000_000, 0, 1_000, 1, 1),
    ];
    let p = flat(&rows);
    let b = p.belief(4);
    assert_eq!(b.life_minutes(), 3);
    assert_eq!(b.curve_len(), 3);
    for t in 0..3 {
        assert_eq!(b.cores_milli_at(t, Tail::Eol), 1_000, "minute {t}");
        assert_eq!(b.mem_mib_at(t, Tail::Eol), 64);
    }
    for t in [3, 4, 100] {
        assert_eq!(b.cores_milli_at(t, Tail::Eol), 0, "minute {t}");
        assert_eq!(b.mem_mib_at(t, Tail::Eol), 0);
        assert_eq!(b.cores_milli_at(t, Tail::Held), 1_000, "minute {t}");
        assert_eq!(b.mem_mib_at(t, Tail::Held), 64);
    }
}

#[test]
fn observed_series_are_checked() {
    // Defect: an empty series (which would make a ring run reach no minute at all) or
    // cores and memory of different lengths.
    assert!(Observed::new(vec![], vec![]).is_none());
    assert!(Observed::new(vec![1, 2], vec![1]).is_none());
    let o = Observed::new(vec![1, 2], vec![3, 4]).expect("two points");
    assert_eq!(o.minutes(), 2);
    let f = Observed::flat(&ok(0, 0, 130_000, 260_000, 7));
    assert_eq!(f.minutes(), 3);
    assert_eq!(Observed::flat(&ok(0, 0, 59_999, 1, 7)).minutes(), 1);
}

// ---- the ring and its cache ------------------------------------------------------------

#[test]
fn the_ring_keeps_the_last_32_runs_by_end_not_by_start() {
    // Defect: ring order by start. Run 0 starts first and ends last of the 33; the ring
    // must drop the first of the short ones, which ended first, and keep run 0. It is
    // visible through the curve: only run 0 lasts 2 hours, so only it gives the curve
    // its 121 points.
    let mut rows = vec![ok(0, 0, 120 * MINUTE_MS, 1_000, 10)];
    rows.extend((0..32).map(|i| ok(10_000 + i * 1_000, 0, 500, 1_000, 10)));
    rows.push(ok(121 * MINUTE_MS, 0, 1_000, 1, 1));
    let p = flat(&rows);
    let last = p.belief(rows.len() - 1);
    assert_eq!(last.curve_len(), 121);
    // One more short run that ends before the long one: 33 short ones. Now the long run
    // is still the newest by end, so it stays and the oldest short one goes.
    let mut more = rows.clone();
    more.insert(33, ok(60_000, 0, 500, 1_000, 10));
    let p = flat(&more);
    assert_eq!(p.belief(more.len() - 1).curve_len(), 121);
    assert_eq!(RING, 32);
}

#[test]
fn a_push_invalidates_the_cached_curve_and_no_push_reuses_it() {
    // Defect: a stale cache (a lease after a push reading the curve built before it), or
    // no cache (equal curves rebuilt per lease). Leases between two pushes share one
    // allocation; the lease after a push reads the new curve.
    let rows = vec![
        ok(0, 0, 1_000, 100, 10),
        ok(2_000, 0, 1_000, 200, 20),
        ok(4_000, 0, 1_000, 300, 30),
        ok(10_000, 0, 100_000, 100, 40),
        ok(12_000, 0, 1_000, 9_000, 90),
        ok(20_000, 0, 1_000, 1, 1),
        ok(30_000, 0, 1_000, 1, 1),
    ];
    let p = flat(&rows);
    // Lease 3 and lease 4 (before the run at 12,000 ends at 13,000) see the same ring.
    assert!(p.belief(3).shares_curves_with(p.belief(4)));
    assert_eq!(p.belief(3).p_bar_cores_milli(), 300);
    // Lease 5 starts after the 9-core run ended: the curve and p_bar moved.
    assert!(!p.belief(5).shares_curves_with(p.belief(4)));
    assert_eq!(p.belief(5).p_bar_cores_milli(), 9_000);
    assert_eq!(p.belief(5).cores_milli_at(0, Tail::Held), 9_000);
    assert_eq!(p.belief(5).mem_mib_at(0, Tail::Held), 90);
    // Lease 6 also sees lease 5, which ended at 21,000: another push, another curve.
    assert!(!p.belief(6).shares_curves_with(p.belief(5)));
}

// ---- causality and the trace clock -----------------------------------------------------

/// What lease `i` is told, by brute force from the rows alone: the last 32 `Ok` runs of its
/// key that ended before it started, in end order, as (cold, p_bar cores, p_bar memory,
/// curve of cores at minutes 0..40, L). Written without sharing code with the learner
/// except the two small public helpers it is also meant to match.
fn oracle(rows: &[Row], i: usize, wait: &dyn Fn(usize) -> u64) -> (bool, u64, u64, Vec<u64>, u8) {
    let at = |j: usize| rows[j].start_ms + wait(j);
    let end = |j: usize| at(j) + rows[j].wall_ms;
    let mut seen: Vec<usize> = (0..rows.len())
        .filter(|&j| rows[j].key == rows[i].key && rows[j].outcome == Outcome::Ok)
        .filter(|&j| end(j) < at(i))
        .collect();
    seen.sort_by_key(|&j| (end(j), j));
    let seen = &seen[seen.len().saturating_sub(32)..];
    if seen.len() < 3 {
        return (true, 2_000, 4_096, vec![2_000; 40], 1);
    }
    let rank = |mut v: Vec<u64>| {
        v.sort_unstable();
        v[(v.len() * 9).div_ceil(10) - 1]
    };
    let cores = rank(seen.iter().map(|&j| mean_cores_milli(&rows[j])).collect());
    let mem = rank(seen.iter().map(|&j| rows[j].peak_mem_mib).collect());
    let curve = (0..40)
        .map(|t| {
            let reach: Vec<u64> = seen
                .iter()
                .filter(|&&j| usize::from(minutes_reached(&rows[j])) >= t)
                .map(|&j| mean_cores_milli(&rows[j]))
                .collect();
            if reach.is_empty() {
                u64::MAX
            } else {
                rank(reach)
            }
        })
        .collect::<Vec<_>>();
    let last_real = curve.iter().rposition(|v| *v != u64::MAX).unwrap_or(0);
    let curve = (0..40).map(|t| curve[t.min(last_real)]).collect::<Vec<_>>();
    let mut life = 0u8;
    while life < 30
        && seen
            .iter()
            .filter(|&&j| minutes_reached(&rows[j]) >= life)
            .count()
            * 2
            >= seen.len()
    {
        life += 1;
    }
    (false, cores, mem, curve, life.clamp(1, 30))
}

fn told(p: &Predictor, i: usize) -> (bool, u64, u64, Vec<u64>, u8) {
    let b = p.belief(i);
    let curve = (0..40).map(|t| b.cores_milli_at(t, Tail::Held)).collect();
    (
        b.is_cold(),
        b.p_bar_cores_milli(),
        b.p_bar_mem_mib(),
        curve,
        b.life_minutes(),
    )
}

/// The first `n` rows of the committed trace.
fn prefix(n: usize) -> Vec<Row> {
    Fixture::embedded().expect("loads").rows[..n].to_vec()
}

#[test]
fn beliefs_match_a_brute_force_reading_of_the_committed_trace() {
    // Defect: any disagreement between the incremental ring and the definition: a
    // run missed, seen early or late, a ring that keeps the wrong 32, a rank off by one,
    // a curve point over the wrong runs, a cold flat charge. Over the first 4,000 rows.
    let rows = prefix(4_000);
    let p = flat(&rows);
    let none = |_: usize| 0u64;
    let mut warm = 0;
    for i in 0..rows.len() {
        let want = oracle(&rows, i, &none);
        assert_eq!(told(&p, i), want, "lease {i}");
        warm += usize::from(!want.0);
    }
    assert!(warm > 500, "the trace warms its keys ({warm} warm leases)");
}

/// A series that depends on the seed and the run: mean cores moved by up to 20% each
/// minute, a working set moved the same way.
fn seeded(seed: u64) -> impl Fn(usize, &Row) -> Observed {
    move |i, r| {
        let mix = |t: usize, salt: u8| {
            let mut b = Vec::new();
            b.extend_from_slice(&seed.to_le_bytes());
            b.extend_from_slice(&(i as u64).to_le_bytes());
            b.extend_from_slice(&(t as u64).to_le_bytes());
            b.push(salt);
            fnv1a(&b) % 41
        };
        let points = usize::from(minutes_reached(r)) + 1;
        let cores = (0..points)
            .map(|t| mean_cores_milli(r) * (80 + mix(t, 0)) / 100)
            .collect();
        let ws = (0..points)
            .map(|t| r.peak_mem_mib * (80 + mix(t, 1)) / 100)
            .collect();
        Observed::new(cores, ws).expect("a series")
    }
}

#[test]
fn beliefs_are_a_function_of_the_fixture_and_the_seed_only() {
    // Defect (N1): a ring filled when the engine finishes a run, so that a lease that
    // waited, or a ranker that placed leases in another order, learns another thing. The
    // predictor takes no placement input, so the check is made against the definition:
    // for three toy engines (no waits; a wait of 0 to 30 s by lease; a wait of up to 40
    // minutes, placed in reverse order) the beliefs read are identical bytes and equal the
    // trace-clock brute force. The same toy engines with an engine-clock learner (ring
    // filled at placement + wall) would differ, which the last assertion shows, so the
    // equality above is a property and not an accident of the trace.
    let rows = prefix(2_000);
    let p = Predictor::build(&rows, seeded(7));
    let reference = p.to_bytes();
    type Wait = Box<dyn Fn(usize) -> u64>;
    let engines: [(Wait, bool); 3] = [
        (Box::new(|_| 0), false),
        (Box::new(|i| (i as u64 * 7_919) % 30_000), false),
        (Box::new(|i| (i as u64 * 104_729) % (40 * MINUTE_MS)), true),
    ];
    let mut engine_clock_differs = false;
    for (wait, reverse) in &engines {
        // The engine places lease i at start + wait; it asks for the belief then.
        let mut order: Vec<usize> = (0..rows.len()).collect();
        if *reverse {
            order.reverse();
        } else {
            order.sort_by_key(|&i| (rows[i].start_ms + wait(i), i));
        }
        let mut read = vec![Vec::new(); rows.len()];
        for &i in &order {
            read[i] = bytes_of(&p, i);
        }
        assert_eq!(read.concat(), reference);
        // The engine-clock learner, by the same brute force with the waits in it.
        let none = |_: usize| 0u64;
        for i in (0..rows.len()).step_by(7) {
            if oracle(&rows, i, wait) != oracle(&rows, i, &none) {
                engine_clock_differs = true;
            }
        }
    }
    assert!(
        engine_clock_differs,
        "waits can change what a lease is told"
    );
    // The trace-clock brute force equals the predictor for the flat series too.
    let q = flat(&rows);
    let none = |_: usize| 0u64;
    for i in (0..rows.len()).step_by(11) {
        assert_eq!(told(&q, i), oracle(&rows, i, &none), "lease {i}");
    }
    // Same seed, same bytes; another seed moves them.
    assert_eq!(Predictor::build(&rows, seeded(7)).to_bytes(), reference);
    assert_ne!(Predictor::build(&rows, seeded(8)).to_bytes(), reference);
    assert_eq!(Predictor::build(&rows, seeded(7)).digest(), p.digest());
}

#[test]
fn editing_a_later_run_leaves_earlier_predictions_byte_identical() {
    // Defect: look-ahead. A lease reads only runs that ended before it started, so
    // rewriting one run changes only the leases of its key that start after the run's
    // end (old or new). Every other lease, of any key, keeps its bytes.
    let rows = prefix(3_000);
    let p = Predictor::build(&rows, seeded(3));
    // An `Ok` run after which the key has a lease that is warm and sees the run: three
    // or more `Ok` runs of the key (this one among them) ended before that lease began.
    let sees = |j: usize, i: usize| {
        let ended = |r: &Row| r.start_ms + r.wall_ms < rows[i].start_ms;
        let warm = rows
            .iter()
            .filter(|r| r.key == rows[j].key && r.outcome == Outcome::Ok && ended(r))
            .count();
        rows[i].key == rows[j].key && warm >= 3 && ended(&rows[j])
    };
    let j = (1_000..rows.len())
        .find(|&j| rows[j].outcome == Outcome::Ok && (j + 1..rows.len()).any(|i| sees(j, i)))
        .expect("a run with later leases of its key");
    let mut edited = rows.clone();
    edited[j].wall_ms = 61 * MINUTE_MS;
    edited[j].cpu_ms = 999_000_000;
    edited[j].peak_mem_mib = 40_000;
    let q = Predictor::build(&edited, seeded(3));
    let old_end = rows[j].start_ms + rows[j].wall_ms;
    let new_end = edited[j].start_ms + edited[j].wall_ms;
    let horizon = old_end.min(new_end);
    let mut changed = 0;
    for i in 0..rows.len() {
        let same = bytes_of(&p, i) == bytes_of(&q, i);
        if rows[i].key != rows[j].key || rows[i].start_ms <= horizon {
            assert!(same, "lease {i} read a run that had not ended");
        }
        changed += usize::from(!same);
    }
    assert!(changed > 0, "the edit reached the leases that could see it");
}

#[test]
fn the_committed_trace_builds_with_every_lease_believed() {
    // Defect: a lease dropped or duplicated by the sweep (a count off from the rows), a
    // belief with no curve. Some leases are cold and some warm.
    let fixture = Fixture::embedded().expect("loads");
    let p = flat(&fixture.rows);
    assert_eq!(p.len(), fixture.rows.len());
    assert!(!p.is_empty());
    let cold = (0..p.len()).filter(|&i| p.belief(i).is_cold()).count();
    assert!(cold > 0 && cold < p.len(), "{cold} cold of {}", p.len());
    assert!((0..p.len()).all(|i| p.belief(i).curve_len() >= 1 && p.belief(i).life_minutes() >= 1));
    assert!(Predictor::build(&[], |_, r| Observed::flat(r)).is_empty());
}

#[test]
fn the_learner_does_not_import_the_truth_model() {
    // Defect: a belief computed from what really happens to a lease. The learner's
    // source names no truth module and no unordered map.
    let src = include_str!("engine/predictor.rs");
    for line in src.lines().filter(|l| l.trim_start().starts_with("use ")) {
        assert!(!line.contains("truth"), "{line}");
        assert!(!line.contains("tables"), "{line}");
    }
    assert!(!src.contains("HashMap") && !src.contains("HashSet"));
    assert!(!src.contains("f64") && !src.contains("f32"));
}
