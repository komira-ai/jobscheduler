//! The binner over samples built by hand: minutes, straddles, gaps, folds and the queue.

use js_agent_usage::{
    Binner, ClosedMinute, LeaseSample, MAX_UNSENT_AGE_USEC, MIN_SPAN_USEC, MINUTE_USEC,
    Observation, Sample, TAIL_MINUTE, WindowPart, window_parts,
};

/// One second in microseconds.
const S: u64 = 1_000_000;

fn lease(id: &str, started: u64, cpu: u64, ws: u64) -> LeaseSample {
    LeaseSample {
        lease_id: id.to_owned(),
        started_usec: started,
        cpu_usage_usec: cpu,
        working_set_bytes: ws,
    }
}

fn sample(mono: u64, leases: Vec<LeaseSample>) -> Sample {
    Sample {
        mono_usec: mono,
        cpu_usage_usec: 0,
        working_set_bytes: 0,
        active_file_bytes: 0,
        cpu_limited: false,
        leases,
        unreadable_leases: 0,
    }
}

/// Feeds one lease a sample at each of `times`; `f` gives its counter and working set.
fn drive(
    binner: &mut Binner,
    id: &str,
    started: u64,
    times: impl IntoIterator<Item = u64>,
    f: impl Fn(u64) -> (u64, u64),
) -> Vec<Observation> {
    times
        .into_iter()
        .map(|t| {
            let (cpu, ws) = f(t);
            binner.observe(&sample(t, vec![lease(id, started, cpu, ws)]))
        })
        .collect()
}

/// Seconds `from`, `from + step`, ..., `to`, in microseconds.
fn secs(from: u64, to: u64, step: u64) -> Vec<u64> {
    (from..=to).step_by(step as usize).map(|s| s * S).collect()
}

/// A counter that stands still.
fn idle(_: u64) -> (u64, u64) {
    (0, 0)
}

fn minute(id: &str, minute: u16, mean: u32, peak: u32, ws: u64) -> ClosedMinute {
    ClosedMinute {
        lease_id: id.to_owned(),
        minute,
        cpu_mean_millis: mean,
        cpu_peak_window_millis: peak,
        ws_max_bytes: ws,
    }
}

/// Asserts that samples up to 59.999999 s close nothing and a sample at 60 s closes
/// minute 0, which is then a real idle minute (mean 0, peak 0), present and not absent.
/// It catches `>=` turned into `>`, a ceiling or a rounding where a floor belongs, and an
/// idle minute taken for a missing one.
#[test]
fn a_minute_closes_exactly_at_sixty_seconds() {
    let mut binner = Binner::new(10 * S);
    let mut times = secs(0, 50, 10);
    times.push(60 * S - 1);
    drive(&mut binner, "a", 0, times, idle);
    assert!(binner.take(10, 60 * S).is_empty());
    assert_eq!(binner.pending(), 0);
    drive(&mut binner, "a", 0, [60 * S], idle);
    assert_eq!(binner.take(10, 60 * S), vec![minute("a", 0, 0, 0, 0)]);
}

/// Asserts the exact parts of a window across a boundary and of one across two, the
/// handling of an empty window and a `from` before the start. It catches a whole window
/// given to one minute, a part that is not floored, a remainder given to the wrong part
/// and a part dropped.
#[test]
fn a_window_across_a_boundary_is_split_by_overlap() {
    let part = |minute, len_usec, cpu_usec| WindowPart {
        minute,
        len_usec,
        cpu_usec,
    };
    assert_eq!(
        window_parts(0, 57 * S, 62 * S, 500_000),
        vec![part(0, 3 * S, 300_000), part(1, 2 * S, 200_000)]
    );
    assert_eq!(
        window_parts(0, 50 * S, 130 * S, 7_777_777),
        vec![
            part(0, 10 * S, 972_222),
            part(1, 60 * S, 5_833_332),
            part(2, 10 * S, 972_223)
        ]
    );
    assert_eq!(window_parts(0, 0, 60 * S, 9), vec![part(0, 60 * S, 9)]);
    assert!(window_parts(0, 5, 5, 9).is_empty());
    assert!(window_parts(0, 6, 5, 9).is_empty());
    assert_eq!(window_parts(100, 3, 5, 9), vec![part(0, 2, 9)]);

    let mut binner = Binner::new(10 * S);
    let times = [0, 30 * S, 57 * S, 62 * S];
    let counter = |t: u64| (if t == 62 * S { 500_000 } else { 0 }, 0);
    drive(&mut binner, "a", 0, times, counter);
    assert_eq!(binner.take(10, 62 * S), vec![minute("a", 0, 5, 100, 0)]);
}

/// Asserts that the parts of a window add up to its CPU and its length exactly, and
/// follow one another minute by minute, across eight alignments (one window over three
/// boundaries, a window of a single microsecond, the largest counter). It catches a part
/// that loses a microsecond to a floor, a remainder dropped and a part skipped.
#[test]
fn splitting_conserves_cpu_across_alignments() {
    let m = MINUTE_USEC;
    let cases: [(u64, u64, u64, u64); 8] = [
        (0, 0, 1, 7),
        (0, 0, m, 13),
        (0, m - 1, m + 1, 3),
        (0, 50 * S, 130 * S, 7_777_777),
        (5 * S, 5 * S, 185 * S, 1_000_003),
        (100, 103, 3 * m + 99, 999_999_999),
        (7, 8, 5 * m + 10, u64::MAX),
        (0, 4 * m, 4 * m + 1, 0),
    ];
    for (started, from, to, cpu) in cases {
        let parts = window_parts(started, from, to, cpu);
        assert_eq!(
            parts.iter().map(|p| u128::from(p.cpu_usec)).sum::<u128>(),
            u128::from(cpu)
        );
        assert_eq!(parts.iter().map(|p| p.len_usec).sum::<u64>(), to - from);
        assert!(parts.iter().all(|p| p.len_usec > 0 && p.len_usec <= m));
        assert!(parts.windows(2).all(|w| w[1].minute == w[0].minute + 1));
        assert_eq!(parts[0].minute, (from - started) / m);
    }
}

/// Asserts that a 55 s stall is a gap, not a window, so the 5 s after it leave the minute
/// with 5 s of span and absent. It catches a missing gap rule, which would report a full
/// minute for 5 s of observation.
#[test]
fn a_stall_is_a_gap_not_a_minute() {
    let mut binner = Binner::new(5 * S);
    let seen = drive(&mut binner, "a", 0, [0, 55 * S, 60 * S], |t| (t, 0));
    assert!(seen[1].covered.is_empty());
    assert_eq!(seen[2].covered, ["a"]);
    assert_eq!(binner.counters().gaps, 1);
    assert_eq!(binner.counters().thin_minutes, 1);
    assert!(binner.take(10, 60 * S).is_empty());
}

/// Asserts the span threshold: 29.999999 s is absent, 30 s is present and its mean is over
/// the 30 s observed (100), not over the minute (50). It catches `>=` against `>`, a mean
/// over the wall minute and an edited threshold.
#[test]
fn minimum_span_decides_presence() {
    assert_eq!(MIN_SPAN_USEC, 30 * S);
    let mut thin = Binner::new(30 * S);
    drive(&mut thin, "a", 0, [0, MIN_SPAN_USEC - 1], |t| (t / 10, 0));
    thin.lease_ended("a");
    assert!(thin.take(10, MIN_SPAN_USEC).is_empty());
    assert_eq!(thin.counters().thin_minutes, 1);

    let mut whole = Binner::new(30 * S);
    drive(&mut whole, "a", 0, [0, MIN_SPAN_USEC], |t| (t / 10, 0));
    whole.lease_ended("a");
    assert_eq!(
        whole.take(10, MIN_SPAN_USEC),
        vec![minute("a", 0, 100, 100, 0)]
    );
}

/// Asserts that a lease first seen at 7 s with 9 s of CPU already on its counter does not
/// have that CPU in minute 0. It catches a baseline at zero.
#[test]
fn the_first_sample_is_a_baseline() {
    let mut binner = Binner::new(10 * S);
    let times = [7, 17, 27, 37, 47, 57, 60].map(|s| s * S);
    let seen = drive(&mut binner, "a", 0, times, |_| (9 * S, 0));
    assert!(seen[0].covered.is_empty());
    assert_eq!(binner.take(10, 60 * S), vec![minute("a", 0, 0, 0, 0)]);
}

/// Asserts that a lease first seen in its eighth minute has that minute alone: nothing
/// below it and no synthetic zeros. It catches a zero fill of the minutes before the first
/// sample.
#[test]
fn a_lease_first_seen_late_has_no_early_minutes() {
    let mut binner = Binner::new(10 * S);
    let times = secs(425, 485, 10);
    drive(&mut binner, "a", 0, times, idle);
    assert_eq!(binner.take(10, 485 * S), vec![minute("a", 7, 0, 0, 0)]);
}

/// Asserts that a flat counter for a full minute is a present minute of mean 0 and peak 0,
/// not a missing one. It catches 0 treated as missing.
#[test]
fn an_idle_lease_reports_a_real_zero() {
    let mut binner = Binner::new(10 * S);
    drive(&mut binner, "a", 0, secs(0, 60, 10), |_| (123, 0));
    assert_eq!(binner.take(10, 60 * S), vec![minute("a", 0, 0, 0, 0)]);
}

/// Asserts that two leases in the same samples, started seven minutes apart, count their
/// minutes from their own starts: the older one's is 7, the newer one's 0. It catches a
/// base taken from the node or from the sample.
#[test]
fn minutes_count_from_each_leases_own_start() {
    let mut binner = Binner::new(10 * S);
    for t in secs(420, 480, 10) {
        binner.observe(&sample(
            t,
            vec![lease("a", 0, 0, 0), lease("b", 420 * S, 0, 0)],
        ));
    }
    let got = binner.take(10, 480 * S);
    assert_eq!(got, vec![minute("a", 7, 0, 0, 0), minute("b", 0, 0, 0, 0)]);
}

/// Asserts that a spike window puts the peak (300) above the mean (50) and that the
/// working set of a minute is its highest value (9 of 5, 9, 3), not its last. It catches a
/// peak equal to the mean and a working set that is the last sample's.
#[test]
fn peak_and_ws_max() {
    let mut binner = Binner::new(10 * S);
    let f = |t: u64| {
        let cpu = if t >= 30 * S { 3 * S } else { 0 };
        let ws = match t / S {
            10 => 5,
            20 => 9,
            30 => 3,
            _ => 1,
        };
        (cpu, ws)
    };
    drive(&mut binner, "a", 0, secs(0, 60, 10), f);
    assert_eq!(binner.take(10, 60 * S), vec![minute("a", 0, 50, 300, 9)]);
}

/// A small deterministic generator.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// Asserts, over a hundred seeded runs of irregular samples that straddle and fold, that
/// no closed minute has a mean above its peak. It catches a rounding in the split that
/// would make the wire's checks drop good minutes.
#[test]
fn mean_never_exceeds_peak() {
    let mut seen = 0;
    for seed in 0..100 {
        let mut rng = Lcg(seed);
        let mut binner = Binner::new((5 + rng.below(10)) * S);
        let started = rng.below(120) * S + rng.below(S);
        let (mut t, mut cpu) = (started + rng.below(S), rng.below(S));
        let mut closed = Vec::new();
        for _ in 0..400 {
            binner.observe(&sample(t, vec![lease("a", started, cpu, 0)]));
            closed.extend(binner.take(1000, 0));
            t += rng.below(25 * S) + 1;
            cpu += rng.below(40 * S);
        }
        binner.lease_ended("a");
        closed.extend(binner.take(1000, 0));
        for got in closed {
            assert!(got.cpu_mean_millis <= got.cpu_peak_window_millis, "{got:?}");
            assert!(got.minute <= TAIL_MINUTE);
            seen += 1;
        }
    }
    assert!(seen > 100, "{seen}");
}

/// Asserts that a counter that goes back drops that window (counted, no minute of
/// 10 cores made by a wrapped subtraction) and that the next windows count from the new
/// baseline. It catches `wrapping_sub`.
#[test]
fn a_counter_that_goes_back_drops_the_window() {
    let mut binner = Binner::new(10 * S);
    let cpu = |t: u64| match t / S {
        0 => 5 * S,
        10 => 6 * S,
        20 => 100,
        s => 100 + (s - 20) * S / 10,
    };
    let seen = drive(&mut binner, "a", 0, secs(0, 60, 10), |t| (cpu(t), 0));
    assert!(seen[2].covered.is_empty());
    assert_eq!(binner.counters().counter_back, 1);
    assert_eq!(binner.take(10, 60 * S), vec![minute("a", 0, 100, 100, 0)]);
}

/// Asserts that a clock that goes back is counted, ends the lease's series (its short
/// minute is thin) and starts a new one that counts from the new baseline, so the bad
/// window adds no span. It catches a missing guard, which would subtract past zero.
#[test]
fn a_clock_that_goes_back_drops_the_window() {
    let mut binner = Binner::new(10 * S);
    let times = [0, 10, 20, 5, 15, 25, 35, 45, 55, 65].map(|s| s * S);
    let seen = drive(&mut binner, "a", 0, times, |t| (t / 10, 0));
    assert!(seen[3].covered.is_empty());
    assert_eq!(binner.counters().clock_back, 1);
    assert_eq!(binner.counters().thin_minutes, 1);
    assert_eq!(binner.take(10, 65 * S), vec![minute("a", 0, 100, 100, 0)]);
}

/// Asserts that ending a lease at 45 s emits its minute once, that a second end emits
/// nothing and leaves no state, that an unknown lease is ignored and that a lease ended at
/// 20 s is absent. It catches a missing flush, a double flush and a state leak.
#[test]
fn lease_ended_flushes_once_and_forgets() {
    let mut binner = Binner::new(10 * S);
    drive(
        &mut binner,
        "a",
        0,
        [0, 10, 20, 30, 40, 45].map(|s| s * S),
        idle,
    );
    drive(&mut binner, "b", 0, [0, 10, 20].map(|s| s * S), idle);
    assert_eq!(binner.tracked(), 2);
    binner.lease_ended("a");
    binner.lease_ended("a");
    binner.lease_ended("nobody");
    binner.lease_ended("b");
    assert_eq!(binner.tracked(), 0);
    assert_eq!(binner.pending(), 1);
    assert_eq!(binner.counters().thin_minutes, 1);
    assert_eq!(binner.take(10, 45 * S), vec![minute("a", 0, 0, 0, 0)]);
    assert!(binner.take(10, 45 * S).is_empty());
}

/// Asserts that a lease id used again starts clean, both after an end and without one:
/// its minutes count from the new start (minute 0, not the old index) and its values are
/// its own. It catches a minute index or accumulators that are not reset.
#[test]
fn a_reused_lease_id_starts_clean() {
    let mut binner = Binner::new(10 * S);
    drive(&mut binner, "x", 0, secs(0, 40, 10), |t| (t / 10, 5));
    binner.lease_ended("x");
    assert_eq!(binner.take(10, 40 * S), vec![minute("x", 0, 100, 100, 5)]);
    drive(&mut binner, "x", 100 * S, secs(100, 160, 10), |t| {
        (t / 5, 3)
    });
    assert_eq!(binner.take(10, 160 * S), vec![minute("x", 0, 200, 200, 3)]);

    drive(&mut binner, "y", 0, secs(0, 40, 10), |t| (t / 10, 5));
    let seen = drive(&mut binner, "y", 200 * S, secs(200, 260, 10), |t| {
        (t / 5, 3)
    });
    assert!(seen[0].covered.is_empty());
    assert_eq!(binner.take(10, 260 * S), vec![minute("y", 0, 200, 200, 5)]);
}

/// Asserts that a lease that runs 200 minutes holds at most 64 pending entries, that a
/// spike at minute 100 shows in the tail bucket (63) and one at minute 10 does not, and
/// that a lease adopted with a start stamped 70,000 minutes ago lands in bucket 63 without
/// a panic. It catches an `as u16` truncation (70,000 would become 4,464), a series that
/// grows without bound, and a fold by minimum or by last value.
#[test]
fn a_long_lease_folds_into_the_tail_bucket() {
    let mut binner = Binner::new(10 * S);
    // CPU: 0.1 core, plus 5 extra seconds in the window (6000 s, 6010 s] and 9 extra
    // seconds in the window (600 s, 610 s]; the counter is cumulative.
    let counter = |t: u64| {
        let mut cpu = t / 10;
        if t >= 6_010 * S {
            cpu += 5 * S;
        }
        if t >= 610 * S {
            cpu += 9 * S;
        }
        (cpu, 0)
    };
    let mut most = 0;
    let mut all = Vec::new();
    for t in secs(0, 12_000, 10) {
        binner.observe(&sample(t, vec![lease("a", 0, counter(t).0, 0)]));
        most = most.max(binner.pending());
        if t == 12_000 * S {
            // Now 0: nothing is old, so the whole series is still there to read.
            all = binner.take(1000, 0);
        }
    }
    assert!(most <= 64, "{most}");
    let tail = all.iter().find(|m| m.minute == 63).unwrap();
    assert_eq!(tail.cpu_peak_window_millis, 600);
    let early = all.iter().find(|m| m.minute == 10).unwrap();
    assert_eq!(early.cpu_peak_window_millis, 1000);

    let mut adopted = Binner::new(10 * S);
    let from = 70_000 * 60;
    drive(&mut adopted, "z", 0, secs(from, from + 60, 10), idle);
    assert_eq!(
        adopted.take(10, (from + 60) * S),
        vec![minute("z", 63, 0, 0, 0)]
    );
}

/// Asserts the queue: 70 pending minutes from 35 leases over two closes come out 64 at a
/// time, oldest close first and then by lease id, a `take(0)` gives none and loses none,
/// and the rest follow. It catches `max + 1`, a sort by lease first and a drain of all.
#[test]
fn take_caps_and_orders() {
    let mut binner = Binner::new(10 * S);
    for t in secs(0, 120, 10) {
        let leases = (0..35)
            .map(|i| lease(&format!("l{i:02}"), 0, 0, 0))
            .collect();
        binner.observe(&sample(t, leases));
    }
    assert_eq!(binner.pending(), 70);
    assert!(binner.take(0, 120 * S).is_empty());
    assert_eq!(binner.pending(), 70);
    let first = binner.take(64, 120 * S);
    assert_eq!(first.len(), 64);
    let keys: Vec<(String, u16)> = first
        .iter()
        .map(|m| (m.lease_id.clone(), m.minute))
        .collect();
    let mut expected: Vec<(String, u16)> = (0..35).map(|i| (format!("l{i:02}"), 0)).collect();
    expected.extend((0..29).map(|i| (format!("l{i:02}"), 1)));
    assert_eq!(keys, expected);
    let second = binner.take(64, 120 * S);
    let keys: Vec<(String, u16)> = second
        .iter()
        .map(|m| (m.lease_id.clone(), m.minute))
        .collect();
    let expected: Vec<(String, u16)> = (29..35).map(|i| (format!("l{i:02}"), 1)).collect();
    assert_eq!(keys, expected);
    assert!(binner.take(64, 120 * S).is_empty());
}

/// Asserts that a pending minute is kept at exactly 600 s old and dropped (and counted) a
/// microsecond later. It catches `>=` against `>` and a missing expiry.
#[test]
fn old_pending_minutes_are_dropped_and_counted() {
    let ended = || {
        let mut binner = Binner::new(10 * S);
        drive(&mut binner, "a", 0, secs(0, 45, 5), idle);
        binner.lease_ended("a");
        binner
    };
    let mut kept = ended();
    assert_eq!(kept.take(10, 45 * S + MAX_UNSENT_AGE_USEC).len(), 1);
    assert_eq!(kept.counters().aged_out, 0);
    let mut dropped = ended();
    assert!(
        dropped
            .take(10, 45 * S + MAX_UNSENT_AGE_USEC + 1)
            .is_empty()
    );
    assert_eq!(dropped.counters().aged_out, 1);
    assert_eq!(dropped.pending(), 0);
}

/// Asserts that requeueing what was taken gives it back once, that requeueing twice still
/// gives one, that a repeat merges by the maximum of each field (the tail bucket included)
/// and that a minute 200 is the tail bucket. It also asserts a requeued minute keeps the
/// age it was closed with, so a server that stays down cannot keep it alive. It catches an
/// append instead of a merge, a delivered minute added again and an age that restarts.
#[test]
fn requeue_merges_and_never_duplicates() {
    let mut binner = Binner::new(10 * S);
    drive(&mut binner, "a", 0, secs(0, 45, 5), idle);
    binner.lease_ended("a");
    let taken = binner.take(10, 50 * S);
    assert_eq!(binner.pending(), 0);
    binner.requeue(taken.clone());
    binner.requeue(taken.clone());
    assert_eq!(binner.take(10, 50 * S), taken);
    assert!(binner.take(10, 50 * S).is_empty());

    let mut stale = Binner::new(10 * S);
    drive(&mut stale, "a", 0, secs(0, 45, 5), idle);
    stale.lease_ended("a");
    stale.observe(&sample(300 * S, vec![lease("b", 0, 0, 0)]));
    let taken = stale.take(10, 50 * S);
    stale.requeue(taken);
    let old = 45 * S + MAX_UNSENT_AGE_USEC + 1;
    assert!(stale.take(10, old).is_empty());
    assert_eq!(stale.counters().aged_out, 1);

    binner.requeue(vec![minute("t", 63, 5, 50, 7)]);
    binner.requeue(vec![minute("t", 200, 9, 40, 3)]);
    assert_eq!(binner.take(10, 0), vec![minute("t", 63, 9, 50, 7)]);
}

/// Asserts which leases a sample covers: not a first sample, not a window after the counter
/// went back, not one after a gap, not one after the lease ended, not a repeat of the
/// previous time, not a lease that has not started (counted); a steady one yes. It catches
/// `covered` listing every lease.
#[test]
fn covered_lists_whole_window_leases_only() {
    let mut binner = Binner::new(10 * S);
    let at = |binner: &mut Binner, t: u64, leases: Vec<LeaseSample>| {
        binner.observe(&sample(t * S, leases)).covered
    };
    let a = |cpu| lease("a", 0, cpu, 0);
    let b = |cpu| lease("b", 0, cpu, 0);
    assert!(at(&mut binner, 0, vec![a(10)]).is_empty());
    assert_eq!(at(&mut binner, 10, vec![a(20), b(5)]), ["a"]);
    assert_eq!(at(&mut binner, 20, vec![a(30), b(6)]), ["a", "b"]);
    assert_eq!(at(&mut binner, 30, vec![a(5), b(7)]), ["b"]);
    assert!(at(&mut binner, 100, vec![a(6), b(8)]).is_empty());
    assert_eq!(at(&mut binner, 110, vec![a(7), b(9)]), ["a", "b"]);
    assert!(at(&mut binner, 110, vec![a(8), b(9)]).is_empty());
    binner.lease_ended("a");
    assert_eq!(at(&mut binner, 120, vec![a(9), b(10)]), ["b"]);
    let late = lease("c", 1_000 * S, 1, 1);
    assert!(at(&mut binner, 130, vec![late]).is_empty());
    assert_eq!(binner.counters().before_start, 1);
    assert_eq!(binner.tracked(), 2);
    let c = binner.counters();
    assert_eq!(
        (c.gaps, c.counter_back, c.before_start, c.clock_back),
        (2, 1, 1, 0)
    );
}

/// Asserts that a lease that vanishes without being ended is forgotten once it has been
/// absent for more than 600 s (kept at exactly 600 s), counted, and that its closed minute
/// is still taken. It catches a leak when `lease_ended` is never called.
#[test]
fn a_vanished_lease_is_forgotten_after_the_age_limit() {
    let mut binner = Binner::new(10 * S);
    drive(&mut binner, "a", 0, secs(0, 70, 10), idle);
    let far = 70 * S + MAX_UNSENT_AGE_USEC;
    binner.observe(&sample(far, vec![lease("b", 0, 0, 0)]));
    assert_eq!(binner.tracked(), 2);
    binner.observe(&sample(far + 1, vec![lease("b", 0, 0, 0)]));
    assert_eq!(binner.tracked(), 1);
    assert_eq!(binner.counters().idle_forgotten, 1);
    assert_eq!(binner.take(10, 600 * S), vec![minute("a", 0, 0, 0, 0)]);
}

/// Asserts that a binner made with a window of 0 reads it as 1 microsecond: a 1 us step
/// is a window, a 10 us step is a gap, and nothing divides by zero. It catches a clamp
/// that is missing.
#[test]
fn a_zero_window_is_clamped() {
    let mut binner = Binner::new(0);
    let seen = drive(&mut binner, "a", 0, [0, 1, 11], |t| (t, 0));
    assert_eq!(seen[1].covered, ["a"]);
    assert!(seen[2].covered.is_empty());
    assert_eq!(binner.counters().gaps, 1);
}

/// Asserts that two samples at the same time contribute nothing and lose nothing: the
/// repeat is not covered, and the CPU it carried is counted in the next window (peak 200,
/// mean 116 over 60 s). It catches a repeat that divides by a zero window or drops CPU.
#[test]
fn a_repeated_sample_carries_its_cpu_to_the_next_window() {
    let mut binner = Binner::new(10 * S);
    let rows = [
        (0, 0),
        (10, 1),
        (10, 2),
        (20, 3),
        (30, 4),
        (40, 5),
        (50, 6),
        (60, 7),
    ];
    let seen: Vec<Observation> = rows
        .iter()
        .map(|&(t, cpu)| binner.observe(&sample(t * S, vec![lease("a", 0, cpu * S, 0)])))
        .collect();
    assert!(seen[2].covered.is_empty());
    assert_eq!(seen[3].covered, ["a"]);
    assert_eq!(binner.take(10, 60 * S), vec![minute("a", 0, 116, 200, 0)]);
}

/// Asserts that a window spanning a whole minute gives that minute the larger of the
/// working sets at the window's two ends rather than 0, and that its CPU is its share.
/// It catches a middle minute opened with `Open::new(minute, 0)`, which reports an
/// invented zero next to a real CPU mean.
#[test]
fn a_minute_inside_one_window_takes_the_bracketing_working_set() {
    let mut binner = Binner::new(30 * S);
    binner.observe(&sample(50 * S, vec![lease("a", 0, 0, 100)]));
    binner.observe(&sample(130 * S, vec![lease("a", 0, 80 * S, 300)]));
    assert_eq!(
        binner.take(10, 130 * S),
        vec![minute("a", 1, 1000, 1000, 300)]
    );
}

/// Asserts that a window which crosses a boundary and ends exactly on the next one gives
/// the whole minute it covers the bracketing working set: samples at 50 s (100) and
/// 120 s (300) leave minute 1 with a maximum of 300. It catches a bracket withheld from
/// the last part, which opens minute 1 at zero and reports it next to a real CPU mean.
#[test]
fn a_window_ending_on_a_boundary_gives_its_last_minute_the_bracketing_working_set() {
    let mut binner = Binner::new(30 * S);
    binner.observe(&sample(50 * S, vec![lease("a", 0, 0, 100)]));
    binner.observe(&sample(120 * S, vec![lease("a", 0, 70 * S, 300)]));
    assert_eq!(
        binner.take(10, 120 * S),
        vec![minute("a", 1, 1000, 1000, 300)]
    );
}

/// Asserts that a huge CPU delta over a short span saturates the mean and the peak at
/// `u32::MAX` and keeps the mean at or under the peak. It catches an `as u32` cast, which
/// would wrap, and a saturation applied to one field only.
#[test]
fn rates_saturate_at_u32_max() {
    let mut binner = Binner::new(10 * S);
    let huge = u64::MAX / 4;
    for t in secs(0, 70, 10) {
        let cpu = if t == 0 { 0 } else { huge };
        binner.observe(&sample(t, vec![lease("a", 0, cpu, 0)]));
    }
    let got = binner.take(10, 70 * S);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].cpu_peak_window_millis, u32::MAX);
    assert_eq!(got[0].cpu_mean_millis, u32::MAX);
    assert!(got[0].cpu_mean_millis <= got[0].cpu_peak_window_millis);
}

/// Asserts that a window straddling a boundary leaves the old minute with the largest
/// working set sampled inside it: samples at 57 s (100) and 62 s (900) give minute 0 a
/// maximum of 100, and the 900 stays with the minute it was seen in. It catches the
/// bracketing working set handed to the already-open first part of a window.
#[test]
fn a_straddle_keeps_each_minutes_own_working_set() {
    let mut binner = Binner::new(30 * S);
    for (t, ws) in [(0, 10), (30 * S, 40), (57 * S, 100), (62 * S, 900)] {
        binner.observe(&sample(t, vec![lease("a", 0, 0, ws)]));
    }
    assert_eq!(binner.take(10, 62 * S), vec![minute("a", 0, 0, 0, 100)]);
    binner.observe(&sample(120 * S, vec![lease("a", 0, 0, 900)]));
    assert_eq!(binner.take(10, 120 * S), vec![minute("a", 1, 0, 0, 900)]);
}

/// Asserts that a 55 s step is a gap under a 5 s window and an ordinary window once the
/// window is set to 20 s: the same samples give a gap and a thin minute in one binner and
/// a full minute (mean 1000, peak 1000) in the other, and the sample before the change is
/// not reread.
#[test]
fn a_longer_window_makes_a_former_gap_valid() {
    let series = [0, 55 * S, 60 * S];

    let mut kept = Binner::new(5 * S);
    let seen = drive(&mut kept, "a", 0, series, |t| (t, 0));
    assert!(seen[1].covered.is_empty());
    assert_eq!(kept.counters().gaps, 1);
    assert!(kept.take(10, 60 * S).is_empty());

    let mut longer = Binner::new(5 * S);
    drive(&mut longer, "a", 0, [series[0]], |t| (t, 0));
    longer.set_window(20 * S);
    let seen = drive(&mut longer, "a", 0, [series[1], series[2]], |t| (t, 0));
    assert_eq!(seen[0].covered, ["a"]);
    assert_eq!(seen[1].covered, ["a"]);
    assert_eq!(longer.counters().gaps, 0);
    assert_eq!(longer.take(10, 60 * S), vec![minute("a", 0, 1000, 1000, 0)]);
}

/// Asserts that narrowing the window turns a step that was valid into a gap: the 55 s step
/// is an ordinary window under a 20 s window (full minute, no gaps) and is a gap, counted
/// in `gaps` with no span, when the window is set to 5 s after the first sample. A
/// `set_window` that only ever grows the window would leave the step valid.
#[test]
fn a_shorter_window_makes_a_former_valid_step_a_gap() {
    let series = [0, 55 * S, 60 * S];

    let mut kept = Binner::new(20 * S);
    let seen = drive(&mut kept, "a", 0, series, |t| (t, 0));
    assert_eq!(seen[1].covered, ["a"]);
    assert_eq!(kept.counters().gaps, 0);
    assert_eq!(kept.take(10, 60 * S), vec![minute("a", 0, 1000, 1000, 0)]);

    let mut shorter = Binner::new(20 * S);
    drive(&mut shorter, "a", 0, [series[0]], |t| (t, 0));
    shorter.set_window(5 * S);
    let seen = drive(&mut shorter, "a", 0, [series[1], series[2]], |t| (t, 0));
    assert!(seen[0].covered.is_empty());
    assert_eq!(shorter.counters().gaps, 1);
    assert!(shorter.take(10, 60 * S).is_empty());
}

/// Asserts that a window of 0 is read as 1 microsecond, both by `set_window` and by `new`:
/// a step of 3 microseconds (three windows) is a window, a step of 4 is a gap. A zero
/// window taken literally would make every step a gap.
#[test]
fn set_window_zero_is_clamped() {
    let mut made_with_zero = Binner::new(0);
    let mut set_to_zero = Binner::new(10 * S);
    set_to_zero.set_window(0);
    for binner in [&mut made_with_zero, &mut set_to_zero] {
        let seen = drive(binner, "a", 0, [0, 3, 7], |t| (t, 0));
        assert!(seen[0].covered.is_empty());
        assert_eq!(seen[1].covered, ["a"]);
        assert!(seen[2].covered.is_empty());
        assert_eq!(binner.counters().gaps, 1);
    }
}
