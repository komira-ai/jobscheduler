//! The curve fitted from a key's ring: what admission charges a new lease of the key, by
//! minute since its start.
//!
//! Per minute `t`, over the ring's runs that have a record for it: the cores are the P90
//! (the shipped percentile, nearest rank) and the working set is the maximum, because
//! memory cannot be compressed and a P90 curve is exceeded by about one run in ten by
//! construction (`docs/design/placement-by-load.md`, "Profile shape"). The values are
//! raw: the memory margin and rounding are admission's, applied in one place. Each value
//! is then dilated by [`DILATE_MINUTES`] on both sides, so a busy phase that starts a
//! little early or late is still covered.
//!
//! A run flagged as under node pressure ([`crate::ring::RunSeries::NODE_PRESSURE`]) showed less CPU
//! than it needed, so it does not shape the cores: they come from the other runs, and a
//! key with fewer than [`MIN_RUNS_FOR_CURVE`] of those has no curve. Its working set is
//! still real (memory does not shrink under CPU pressure), so it counts toward the
//! memory maximum, the reach of a minute, and the wall minutes. The censored bit is not
//! read.
//!
//! A minute at the start that no clean run recorded takes the first clean value; later
//! ones hold the previous minute.
//!
//! A minute reached by fewer than a quarter of the runs, and every later one, is not
//! emitted: [`js_types::sched::usage::Curve`] charges its last point to every later
//! minute, which is the "holds the previous value" tail. All arithmetic is integer.

use js_types::sched::usage::MinutePoint;

use crate::ring::{BUCKETS, Ring};

/// The fewest runs, not under node pressure, in the ring for a curve: with fewer, the key
/// is charged flat.
pub const MIN_RUNS_FOR_CURVE: usize = 3;

/// How many minutes each side a value is widened by.
pub const DILATE_MINUTES: usize = 2;

/// The nearest-rank 90th percentile of `sorted` (ascending): the `ceil(0.9 n)`-th
/// smallest, so one to four values give the maximum; 0 for no value. It is the definition
/// `js-sizing` uses for a key's flat CPU figure, so a flat and a curve charge agree.
#[must_use]
pub fn p90(sorted: &[u64]) -> u64 {
    let rank = (sorted.len() * 9).div_ceil(10);
    sorted.get(rank.saturating_sub(1)).copied().unwrap_or(0)
}

/// A key's fitted curve and the life of its runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fit {
    /// Minute `i` is point `i`; at most [`BUCKETS`] points, at least one.
    pub points: Vec<MinutePoint>,
    /// How many runs the ring held, those under node pressure included.
    pub runs: u16,
    /// The P90 of the runs' wall minutes (rounded up per run), at least 1: how long a new
    /// lease of the key is expected to last.
    pub wall_minutes_p90: u32,
}

/// The fit of `ring`, or `None` when it holds fewer than [`MIN_RUNS_FOR_CURVE`] runs that
/// were not under node pressure.
#[must_use]
pub fn fit(ring: &Ring) -> Option<Fit> {
    let runs = ring.len();
    if ring.runs().filter(|r| !r.under_node_pressure()).count() < MIN_RUNS_FOR_CURVE {
        return None;
    }
    let mut raw: Vec<MinutePoint> = Vec::new();
    // Leading minutes no clean run recorded: they take the first clean value below.
    let mut lead = 0;
    for t in 0..BUCKETS {
        let reached = ring.runs().filter(|r| r.buckets.len() > t).count();
        if reached * 4 < runs {
            break;
        }
        let prev = raw.last().copied().unwrap_or_default();
        // Reached but never recorded: hold the previous minute, for each figure apart.
        let mut cores: Vec<u64> = ring
            .runs()
            .filter(|r| !r.under_node_pressure())
            .filter_map(|r| r.buckets.get(t).copied().flatten())
            .map(|b| u64::from(b.cores_milli))
            .collect();
        cores.sort_unstable();
        let working_set_bytes = ring
            .runs()
            .filter_map(|r| r.buckets.get(t).copied().flatten())
            .map(|b| b.ws_bytes)
            .max()
            .unwrap_or(prev.working_set_bytes);
        let point = MinutePoint {
            cores_milli: if cores.is_empty() {
                prev.cores_milli
            } else {
                u32::try_from(p90(&cores)).unwrap_or(u32::MAX)
            },
            working_set_bytes,
        };
        if cores.is_empty() && lead == raw.len() {
            lead += 1;
        }
        raw.push(point);
    }
    // Nothing earlier to hold: the start of the curve is the first clean value, not 0.
    if let Some(first) = raw.get(lead).map(|p| p.cores_milli) {
        for p in &mut raw[..lead] {
            p.cores_milli = first;
        }
    }
    let points = (0..raw.len())
        .map(|t| {
            let window =
                &raw[t.saturating_sub(DILATE_MINUTES)..(t + DILATE_MINUTES + 1).min(raw.len())];
            MinutePoint {
                cores_milli: window.iter().map(|p| p.cores_milli).max().unwrap_or(0),
                working_set_bytes: window
                    .iter()
                    .map(|p| p.working_set_bytes)
                    .max()
                    .unwrap_or(0),
            }
        })
        .collect();
    let mut lives: Vec<u64> = ring.runs().map(|r| u64::from(r.wall_minutes)).collect();
    lives.sort_unstable();
    Some(Fit {
        points,
        // The ring holds at most 32 runs.
        runs: u16::try_from(runs).unwrap_or(u16::MAX),
        wall_minutes_p90: u32::try_from(p90(&lives)).unwrap_or(u32::MAX).max(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::{RunSeries, tests::series};

    fn pressured(minutes: &[(u32, u64)]) -> crate::ring::RunSeries {
        let mut s = series(minutes);
        s.flags = RunSeries::NODE_PRESSURE;
        s
    }

    fn ring_of(runs: &[&[(u32, u64)]]) -> Ring {
        let mut ring = Ring::new();
        for r in runs {
            ring.push(&series(r));
        }
        ring
    }

    fn cores(f: &Fit) -> Vec<u32> {
        f.points.iter().map(|p| p.cores_milli).collect()
    }

    fn mem(f: &Fit) -> Vec<u64> {
        f.points.iter().map(|p| p.working_set_bytes).collect()
    }

    /// Catches: a P90 that is a floor (index `0.9 n` rounded down), and one that is not
    /// the nearest rank `js-sizing` pins (1 to 4 values give the maximum).
    #[test]
    fn p90_is_the_nearest_rank_as_js_sizing_has_it() {
        assert_eq!(p90(&[]), 0);
        assert_eq!(p90(&[7]), 7);
        assert_eq!(p90(&[1, 2]), 2);
        assert_eq!(p90(&[1, 2, 3, 4]), 4, "four values: rank 4");
        assert_eq!(p90(&[1, 2, 3, 4, 5]), 5, "five values: ceil 4.5 = 5");
        let ten: Vec<u64> = (1..=10).collect();
        assert_eq!(p90(&ten), 9, "ten values: rank 9");
        let eleven: Vec<u64> = (1..=11).collect();
        assert_eq!(p90(&eleven), 10, "eleven: ceil 9.9 = 10");
        let ring: Vec<u64> = (1..=32).collect();
        assert_eq!(p90(&ring), 29, "a full ring: ceil 28.8 = 29");
        assert_eq!(p90(&[5, 5, 5]), 5, "ties");
    }

    /// Catches: a curve from two runs (the key is charged flat), and one withheld at three.
    #[test]
    fn two_runs_give_none_and_three_give_a_curve() {
        let run: &[(u32, u64)] = &[(100, 1)];
        assert_eq!(fit(&Ring::new()), None);
        assert_eq!(fit(&ring_of(&[run, run])), None);
        let f = fit(&ring_of(&[run, run, run])).unwrap();
        assert_eq!(f.runs, 3);
        assert_eq!(
            f.points,
            vec![MinutePoint {
                cores_milli: 100,
                working_set_bytes: 1
            }]
        );
    }

    /// Catches: the cores taking the maximum or the median instead of the P90, and the
    /// memory taking the P90 (one high run must raise the memory at once).
    #[test]
    fn cores_are_the_p90_and_memory_the_maximum_of_the_runs() {
        // Ten runs: P90 is the 9th smallest of the cores; memory is the largest.
        let runs: Vec<Vec<(u32, u64)>> = (1..=10).map(|i| vec![(i * 100, u64::from(i))]).collect();
        let refs: Vec<&[(u32, u64)]> = runs.iter().map(Vec::as_slice).collect();
        let f = fit(&ring_of(&refs)).unwrap();
        assert_eq!(cores(&f), vec![900]);
        assert_eq!(mem(&f), vec![10]);
    }

    /// Catches: the tail threshold `<` vs `<=` and a quarter measured as a fifth: with 8
    /// runs, minute 1 reached by exactly 2 is kept; reached by 1 it is the tail.
    #[test]
    fn a_minute_reached_by_exactly_a_quarter_is_kept_and_fewer_is_the_tail() {
        let short: &[(u32, u64)] = &[(100, 1)];
        let long: &[(u32, u64)] = &[(100, 1), (900, 9)];
        let mut at_quarter = vec![short; 6];
        at_quarter.extend([long, long]);
        let f = fit(&ring_of(&at_quarter)).unwrap();
        assert_eq!(f.points.len(), 2, "2 of 8 reached minute 1");
        assert_eq!(f.points[1].cores_milli, 900);

        let mut below = vec![short; 7];
        below.push(long);
        let f = fit(&ring_of(&below)).unwrap();
        assert_eq!(
            f.points.len(),
            1,
            "1 of 8 is below a quarter: the tail, not emitted"
        );
        assert_eq!(
            f.points[0].cores_milli, 100,
            "the tail is held by the last point"
        );
    }

    /// Catches: dilation of one or three minutes, dilation that reads past the array ends,
    /// and one that shrinks a value.
    #[test]
    fn a_spike_is_widened_by_two_minutes_and_not_three() {
        let mut v: Vec<(u32, u64)> = vec![(100, 10); 11];
        v[5] = (900, 90);
        let f = fit(&ring_of(&[&v, &v, &v])).unwrap();
        assert_eq!(
            cores(&f),
            vec![100, 100, 100, 900, 900, 900, 900, 900, 100, 100, 100]
        );
        assert_eq!(mem(&f)[3], 90);
        assert_eq!(mem(&f)[2], 10);
        assert_eq!(mem(&f)[8], 10);
    }

    /// Catches: dilation indexing out of range at either end of the curve.
    #[test]
    fn dilation_at_the_array_ends_clips() {
        let mut v: Vec<(u32, u64)> = vec![(100, 1); 6];
        v[0] = (800, 1);
        v[5] = (700, 1);
        let f = fit(&ring_of(&[&v, &v, &v])).unwrap();
        assert_eq!(cores(&f), vec![800, 800, 800, 700, 700, 700]);
    }

    /// Catches: a minute reached by enough runs but recorded by none being emitted as 0
    /// (which would lower the charge) and not holding the previous value.
    #[test]
    fn a_reached_minute_nobody_recorded_holds_the_previous_value() {
        // Minute 1 is missing in every run (a fifth missing of 5 minutes is allowed). Held
        // at 500 it keeps minute 3 at 500 through dilation; taken as 0 it would not.
        let mut ring = Ring::new();
        for _ in 0..3 {
            let mut s = series(&[(500, 5), (0, 0), (100, 1), (100, 1), (100, 1)]);
            s.minutes.retain(|m| m.minute != 1);
            ring.push(&s);
        }
        let f = fit(&ring).unwrap();
        assert_eq!(cores(&f), vec![500, 500, 500, 500, 100]);
    }

    /// Catches: a first minute nobody recorded panicking or borrowing from minute 1.
    #[test]
    fn a_first_minute_nobody_recorded_starts_empty() {
        let mut ring = Ring::new();
        for _ in 0..3 {
            let mut s = series(&[(0, 0), (400, 4), (400, 4), (400, 4), (400, 4)]);
            s.minutes.retain(|m| m.minute != 0);
            ring.push(&s);
        }
        let f = fit(&ring).unwrap();
        assert_eq!(
            cores(&f),
            vec![400; 5],
            "dilation fills the empty first minute"
        );
        assert_eq!(f.points.len(), 5);
    }

    /// Catches: a curve that starts at 0 cores when the first clean value comes after the
    /// dilation reach: minutes 0 to 2 are recorded by no clean run (a pressured run reached
    /// them), so the start is the first clean value, not the default. Three missing minutes
    /// of 15 is the most a run may lack (a fifth) and still be admitted.
    #[test]
    fn a_start_no_clean_run_recorded_takes_the_first_clean_value() {
        let mut ring = Ring::new();
        for _ in 0..3 {
            let mut s = series(&[(400, 4); 15]);
            s.minutes.retain(|m| m.minute >= 3);
            ring.push(&s);
        }
        ring.push(&pressured(&[(900, 9); 15]));
        let f = fit(&ring).unwrap();
        assert_eq!(f.points.len(), 15);
        assert_eq!(cores(&f), vec![400; 15]);
    }

    /// Catches: the life being a mean or a floor P90, and a life under a minute.
    #[test]
    fn the_life_is_the_p90_of_the_wall_minutes() {
        let runs: Vec<Vec<(u32, u64)>> = (1..=10).map(|n| vec![(1, 1); n]).collect();
        let refs: Vec<&[(u32, u64)]> = runs.iter().map(Vec::as_slice).collect();
        let f = fit(&ring_of(&refs)).unwrap();
        assert_eq!(f.wall_minutes_p90, 9);
        let one: &[(u32, u64)] = &[(1, 1)];
        assert_eq!(fit(&ring_of(&[one, one, one])).unwrap().wall_minutes_p90, 1);
    }

    /// Catches: a run longer than the buckets being cut off: the curve ends at bucket 63
    /// and that point stands for later minutes.
    #[test]
    fn a_long_run_gives_at_most_64_points() {
        let long: Vec<(u32, u64)> = (0..100).map(|i| (100 + i, 1)).collect();
        let f = fit(&ring_of(&[&long, &long, &long])).unwrap();
        assert_eq!(f.points.len(), BUCKETS);
        assert_eq!(
            f.points[63].cores_milli, 199,
            "bucket 63 is the maximum from minute 63 on"
        );
        assert_eq!(f.wall_minutes_p90, 100);
    }

    /// Catches: a run under node pressure pulling the cores down, or the minimum run count
    /// being met by such runs.
    #[test]
    fn runs_under_node_pressure_do_not_shape_the_cores() {
        let clean: &[(u32, u64)] = &[(800, 8), (800, 8)];
        let mut with = ring_of(&[clean, clean, clean]);
        let alone = fit(&with).unwrap();
        // 3 clean at 800 plus 27 pressured at 100 are 30 values, so the P90 rank is
        // ceil(0.9 * 30) = 27: the 27th smallest. Were the pressured runs read, the 27
        // values of 100 sort first and the P90 is 100; without them it is 800.
        for _ in 0..27 {
            with.push(&pressured(&[(100, 8), (100, 8)]));
        }
        let f = fit(&with).unwrap();
        assert_eq!(f.points, alone.points);
        assert_eq!(cores(&f), vec![800, 800]);
    }

    /// Catches: pressured runs counting toward the three needed for a curve.
    #[test]
    fn a_key_whose_runs_are_all_under_node_pressure_has_no_curve() {
        let mut ring = Ring::new();
        for _ in 0..3 {
            ring.push(&pressured(&[(100, 1)]));
        }
        assert_eq!(fit(&ring), None);
        let clean: &[(u32, u64)] = &[(100, 1)];
        ring.push(&series(clean));
        ring.push(&series(clean));
        assert_eq!(fit(&ring), None, "two clean runs among five");
        ring.push(&series(clean));
        assert!(fit(&ring).is_some());
    }

    /// Catches: a pressured run's working set being dropped from the memory maximum, its
    /// wall time from the life, or its minutes from the reach; and a minute only pressured
    /// runs reached taking its cores from them.
    #[test]
    fn a_run_under_node_pressure_still_counts_for_memory_and_life() {
        let clean: &[(u32, u64)] = &[(100, 1)];
        let mut ring = ring_of(&[clean, clean, clean]);
        ring.push(&pressured(&[
            (900, 9),
            (900, 9),
            (900, 9),
            (900, 9),
            (900, 90),
            (900, 9),
        ]));
        let f = fit(&ring).unwrap();
        assert_eq!(mem(&f), vec![9, 9, 90, 90, 90, 90]);
        assert_eq!(cores(&f), vec![100; 6], "held, never the pressured 900");
        assert_eq!(f.wall_minutes_p90, 6);
        assert_eq!(f.runs, 4);
    }
}
