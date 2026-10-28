//! A run built from a row: its minute series, bursts, ticks, shapes and working set.

use super::row;
use crate::engine::fixture::{Fixture, KeyIx, Outcome, Row};
use crate::engine::tables::ONE_PPM;
use crate::engine::truth::{
    Fidelity, MEM_RAMP_TICKS, MEM_SPIKE_ONE_IN, Run, SHAPE_MIN_WALL_MS, Shape, ShapeMix,
    TruthConfig, apportion, build, hand_run_cpu_ms,
};

fn cfg(seed: u64) -> TruthConfig {
    TruthConfig::new(seed)
}

/// The key index of the first key whose class under `cfg` is `shape`.
fn key_of(shape: Shape, cfg: &TruthConfig) -> u16 {
    (0..u16::MAX)
        .find(|&k| cfg.mix.class_of(cfg.seed, KeyIx(k)) == shape)
        .expect("some key has every class")
}

/// Walls around every boundary: tick, window, minute, the shape threshold, long runs.
const WALLS: [u64; 14] = [
    1, 999, 1_000, 1_001, 4_999, 5_000, 59_999, 60_000, 60_001, 179_999, 180_000, 600_001,
    1_234_567, 3_725_000,
];

/// Cpu totals: nothing, one core-ms, a prime, many.
const CPUS: [u64; 4] = [0, 1, 7_919, 1_000_003];

fn assert_exact(run: &Run) {
    let id = format!(
        "lease {} wall {} cpu {}",
        run.lease_ix, run.wall_ms, run.cpu_ms
    );
    let minutes: u64 = (0..run.minutes()).map(|m| run.minute_core_ms(m)).sum();
    assert_eq!(minutes, run.cpu_ms, "minute series: {id}");
    let mut ticks = Vec::new();
    for m in 0..run.minutes() {
        let windows = run.minute_windows(m);
        assert_eq!(
            windows.iter().sum::<u64>(),
            run.minute_core_ms(m),
            "windows of minute {m}: {id}"
        );
        let t = run.minute_ticks(m);
        assert_eq!(t.len() as u64, run.ticks_in_minute(m), "{id}");
        assert_eq!(
            t.iter().sum::<u64>(),
            run.minute_core_ms(m),
            "ticks of minute {m}: {id}"
        );
        ticks.extend(t);
    }
    assert_eq!(ticks.len() as u64, run.ticks(), "{id}");
    assert_eq!(ticks, run.all_ticks(), "{id}");
    assert_eq!(ticks.iter().sum::<u64>(), run.cpu_ms, "ticks: {id}");
}

#[test]
fn ticks_sum_to_cpu_and_bursts_sum_to_the_series_exactly() {
    // A shape or a burst that adds or loses CPU (a rounding that is not carried, a
    // partial window or minute mis-sized) breaks one of these sums, and a host would then
    // see demand the fixture never had. Every boundary wall, several totals, every class,
    // both fidelities.
    let mut seen = std::collections::BTreeSet::new();
    for seed in 1..=2 {
        for fidelity in [Fidelity::Identical, Fidelity::Jittered] {
            for key in 0..24u16 {
                let c = TruthConfig {
                    fidelity,
                    ..cfg(seed)
                };
                for (i, &wall) in WALLS.iter().enumerate() {
                    let cpu = CPUS[(i + usize::from(key)) % CPUS.len()];
                    let run = Run::new(&row(key, wall, cpu), i as u32 + 100 * u32::from(key), &c);
                    assert_exact(&run);
                    if wall >= SHAPE_MIN_WALL_MS {
                        seen.insert(run.shape);
                    }
                }
            }
        }
    }
    assert_eq!(seen.len(), Shape::ALL.len(), "every class was exercised");
}

#[test]
fn a_sample_of_the_committed_trace_conserves_cpu() {
    // The same sums on real rows (non-ok ones included, replayed as recorded).
    let fx = Fixture::embedded().expect("the committed fixture loads");
    let runs = build(&fx.rows, &cfg(1));
    assert_eq!(runs.len(), fx.rows.len());
    let mut outcomes = std::collections::BTreeSet::new();
    for run in runs.iter().step_by(7) {
        assert_exact(run);
    }
    for (run, row) in runs.iter().zip(&fx.rows) {
        outcomes.insert(run.outcome);
        assert_eq!(
            (run.cpu_ms, run.wall_ms, run.outcome),
            (row.cpu_ms, row.wall_ms, row.outcome)
        );
    }
    assert!(
        outcomes.contains(&Outcome::Ok) && outcomes.len() > 1,
        "{outcomes:?}"
    );
}

#[test]
fn bursts_move_demand_between_windows_but_not_between_minutes() {
    // A flat key's minute series is exactly proportional to its ticks, yet its windows
    // differ: the bursts are drawn and applied (a burst that is always 1 would leave the
    // windows equal and the 99th-percentile CPU unreachable).
    let c = cfg(1);
    let key = key_of(Shape::Flat, &c);
    let run = Run::new(&row(key, 3_600_000, 7_200_000), 5, &c);
    assert_eq!(run.minutes(), 60);
    for m in 0..60 {
        assert_eq!(run.minute_core_ms(m), 120_000, "minute {m} of a flat run");
    }
    let windows = run.minute_windows(10);
    assert_eq!(windows.len(), 12);
    assert!(windows.iter().any(|&w| w != windows[0]), "{windows:?}");
    // The widest window is above the mean window by a burst's worth, and none is idle.
    assert!(windows.iter().all(|&w| w > 0));
    let widest = windows.iter().max().copied().unwrap_or(0);
    assert!(widest > 10_000 && widest < 25_000, "{widest}");
}

#[test]
fn a_partial_minute_and_a_partial_window_are_cut_over_the_ticks_they_hold() {
    // 61 s: a full minute and a one-tick minute. 7 s: a window of five and one of two.
    let c = cfg(1);
    let long = Run::new(&row(0, 61_000, 6_100), 1, &c);
    assert_eq!(
        (
            long.minutes(),
            long.ticks_in_minute(0),
            long.ticks_in_minute(1)
        ),
        (2, 60, 1)
    );
    assert_eq!(long.minute_core_ms(0) + long.minute_core_ms(1), 6_100);
    assert_eq!(long.minute_ticks(1).len(), 1);
    let short = Run::new(&row(0, 7_000, 7_000), 2, &c);
    assert_eq!(short.minute_windows(0).len(), 2);
    assert_eq!(short.minute_windows(0).iter().sum::<u64>(), 7_000);
    assert_eq!(short.minute_ticks(0).len(), 7);
    // A window of two ticks takes two sevenths of the minute on average, whatever the
    // bursts: over 400 leases its share is within 26% to 31% (a window cut without regard
    // to its ticks would take half).
    let (mut last, mut all) = (0, 0);
    for lease in 0..400 {
        let windows = Run::new(&row(0, 7_000, 7_000), lease, &c).minute_windows(0);
        last += windows[1];
        all += windows[0] + windows[1];
    }
    assert_eq!(all, 400 * 7_000);
    assert!(
        26 * all <= 100 * last && 100 * last <= 31 * all,
        "{last} of {all}"
    );
}

#[test]
fn bursts_are_keyed_by_lease_and_seed() {
    // Same row, same lease_ix, same seed: the same windows. A change of either shifts
    // them; a stream shared between leases would make order matter.
    let key = 3;
    let r = row(key, 600_000, 1_200_000);
    let a = Run::new(&r, 9, &cfg(1));
    assert_eq!(
        a.minute_windows(2),
        Run::new(&r, 9, &cfg(1)).minute_windows(2)
    );
    assert_ne!(
        a.minute_windows(2),
        Run::new(&r, 10, &cfg(1)).minute_windows(2)
    );
    assert_ne!(
        a.minute_windows(2),
        Run::new(&r, 9, &cfg(2)).minute_windows(2)
    );
}

#[test]
fn adding_a_lease_changes_no_other_leases_draws() {
    // A shared stream would hand the extra lease's draws to its neighbours. Appending a
    // row leaves every earlier run equal, and a run built alone (or in the reverse
    // order) equals the one built in its world.
    let rows: Vec<Row> = (0..40u16)
        .map(|i| {
            row(
                i % 9,
                30_000 + 47_000 * u64::from(i),
                90_000 * (u64::from(i) + 1),
            )
        })
        .collect();
    let c = cfg(2);
    let world = build(&rows, &c);
    let mut more = rows.clone();
    more.push(row(4, 777_000, 1_000_000));
    let bigger = build(&more, &c);
    assert_eq!(bigger[..rows.len()], world[..]);
    for i in (0..rows.len()).rev() {
        assert_eq!(Run::new(&rows[i], i as u32, &c), world[i], "lease {i}");
    }
    // A removed row moves only the leases after it (their positions change).
    let mut fewer = rows.clone();
    fewer.remove(30);
    assert_eq!(build(&fewer, &c)[..30], world[..30]);
}

#[test]
fn identical_fidelity_gives_every_run_of_a_key_the_same_series() {
    // Fidelity 0 is the upper bound a learner could reach: no shift, no noise. Two runs
    // of one key of the same size have the same minute series; jittered runs differ.
    let c = TruthConfig {
        fidelity: Fidelity::Identical,
        ..cfg(1)
    };
    let j = TruthConfig {
        fidelity: Fidelity::Jittered,
        ..cfg(1)
    };
    for shape in Shape::ALL.into_iter().filter(|&s| s != Shape::Flat) {
        let key = key_of(shape, &c);
        let r = row(key, 1_500_000, 3_000_000);
        let series = |run: &Run| {
            (0..run.minutes())
                .map(|m| run.minute_core_ms(m))
                .collect::<Vec<_>>()
        };
        let first = series(&Run::new(&r, 0, &c));
        for lease in 1..6 {
            assert_eq!(
                series(&Run::new(&r, lease, &c)),
                first,
                "{shape:?} lease {lease}"
            );
        }
        let jittered: Vec<_> = (0..6).map(|l| series(&Run::new(&r, l, &j))).collect();
        assert!(
            jittered.iter().any(|s| *s != jittered[0]),
            "{shape:?} jitter does nothing"
        );
        assert!(
            jittered.iter().all(|s| *s != first),
            "{shape:?} jitter reproduces the template"
        );
    }
}

#[test]
fn flat_keys_and_short_runs_are_flat_in_every_world() {
    // Only runs of 180 s and over, of shaped keys, have a shape; the rest split cpu in
    // proportion to ticks, with no shift and no noise even when jittered.
    let c = cfg(1);
    let flat = Run::new(&row(key_of(Shape::Flat, &c), 1_500_000, 3_000_000), 3, &c);
    let shaped_key = key_of(Shape::Sleeper, &c);
    let short = Run::new(&row(shaped_key, SHAPE_MIN_WALL_MS - 1, 5_999_940), 3, &c);
    // Without jitter, so the template is not moved a minute.
    let exact = TruthConfig {
        fidelity: Fidelity::Identical,
        ..c
    };
    let at_threshold = Run::new(&row(shaped_key, SHAPE_MIN_WALL_MS, 6_000_000), 3, &exact);
    for run in [&flat, &short] {
        for m in 0..run.minutes() {
            let want = run.cpu_ms * run.ticks_in_minute(m) / run.ticks();
            let got = run.minute_core_ms(m);
            assert!(got.abs_diff(want) <= 1, "minute {m}: {got} vs {want}");
        }
    }
    assert_eq!(short.minutes(), 3);
    // At the threshold the sleeper is idle for its first two fifths: the first minute
    // is (nearly) empty.
    assert_eq!(at_threshold.ticks(), 180);
    assert!(
        at_threshold.minute_core_ms(0) < at_threshold.cpu_ms / 20,
        "{}",
        at_threshold.minute_core_ms(0)
    );
}

#[test]
fn templates_have_mean_one_over_a_runs_life() {
    // A template that adds or removes area would make a shaped run's totals disagree
    // with the unshaped one's. Exact at 1200 ticks (every breakpoint on a tick), within
    // half a percent at an awkward length.
    for shape in Shape::ALL {
        for (ticks, tol_per_mille) in [(1200u64, 1u128), (3000, 1), (997, 5)] {
            let sum: u128 = (0..ticks)
                .map(|t| u128::from(shape.weight_ppm(t, ticks)))
                .sum();
            let want = u128::from(ONE_PPM) * u128::from(ticks);
            assert!(
                sum.abs_diff(want) * 1000 <= want * tol_per_mille,
                "{shape:?} over {ticks} ticks: {sum} vs {want}"
            );
        }
    }
}

#[test]
fn templates_have_the_shapes_the_design_names() {
    // Idle prelude is the step of the CPU sims' `phased()`: demand 0 for 20 of 120
    // units, then constant at 6/5 of the mean. Sleeper: 40 of 100 idle. Late peak: 0.5
    // until 0.7, then 2.0, divided by its mean 0.95. Ramp: 2u.
    let vector = |shape: Shape, ticks| {
        (0..ticks)
            .map(|t| shape.weight_ppm(t, ticks))
            .collect::<Vec<_>>()
    };
    let steps = |idle: usize, awake: u64, total: usize| {
        let mut v = vec![0; idle];
        v.extend(std::iter::repeat_n(awake, total - idle));
        v
    };
    assert_eq!(vector(Shape::IdlePrelude, 120), steps(20, 1_200_000, 120));
    assert_eq!(vector(Shape::Sleeper, 100), steps(40, 1_666_667, 100));
    let late = vector(Shape::LatePeak, 100);
    assert!(late[..70].iter().all(|&w| w == 526_316) && late[70..].iter().all(|&w| w == 2_105_263));
    assert!(
        late[99].abs_diff(4 * late[0]) <= 4,
        "levels differ by 4x up to rounding"
    );
    let ramp = vector(Shape::Ramp, 100);
    assert!(ramp.windows(2).all(|w| w[0] < w[1]));
    assert_eq!((ramp[0], ramp[99]), (10_000, 1_990_000));
    assert_eq!(vector(Shape::Flat, 7), vec![ONE_PPM; 7]);
}

#[test]
fn a_shaped_runs_minutes_follow_its_template() {
    // The template reaches the series: an idle-prelude run of 24 minutes with no jitter
    // is empty for its first four and level after, a ramp's minutes rise.
    let c = TruthConfig {
        fidelity: Fidelity::Identical,
        ..cfg(1)
    };
    let wall = 24 * 60_000;
    let idle = Run::new(&row(key_of(Shape::IdlePrelude, &c), wall, 2_000_000), 0, &c);
    let minutes: Vec<u64> = (0..24).map(|m| idle.minute_core_ms(m)).collect();
    assert!(minutes[..4].iter().all(|&v| v == 0), "{minutes:?}");
    assert!(
        minutes[4..].iter().all(|&v| v.abs_diff(100_000) <= 1),
        "{minutes:?}"
    );
    let ramp = Run::new(&row(key_of(Shape::Ramp, &c), wall, 2_000_000), 0, &c);
    let minutes: Vec<u64> = (0..24).map(|m| ramp.minute_core_ms(m)).collect();
    assert!(minutes.windows(2).all(|w| w[0] < w[1]), "{minutes:?}");
}

#[test]
fn the_default_mix_and_the_null_mix_assign_classes_as_stated() {
    // Counts over 20,000 keys against the mix, within 2 points; the null mix has only
    // flat keys, in every seed.
    let count = |mix: ShapeMix, seed| {
        let mut n = [0u32; 5];
        for k in 0..20_000u16 {
            let class = mix.class_of(seed, KeyIx(k));
            n[Shape::ALL
                .iter()
                .position(|&s| s == class)
                .expect("a listed shape")] += 1;
        }
        n
    };
    for (got, want) in count(ShapeMix::DEFAULT, 1)
        .into_iter()
        .zip([400, 200, 150, 150, 100])
    {
        assert!(got.abs_diff(want * 20) <= 400, "{got} vs {}", want * 20);
    }
    for seed in 1..=3 {
        assert_eq!(count(ShapeMix::NULL, seed), [20_000, 0, 0, 0, 0]);
    }
    // A mix that does not sum to 1000 is refused.
    assert!(std::panic::catch_unwind(|| ShapeMix::new([1, 0, 0, 0, 0])).is_err());
    assert_eq!(ShapeMix::new([400, 200, 150, 150, 100]), ShapeMix::DEFAULT);
}

#[test]
fn the_null_world_has_no_shape_at_all() {
    // Every long run of every key is flat in the null mix, jittered or not.
    let c = TruthConfig {
        mix: ShapeMix::NULL,
        ..cfg(3)
    };
    for key in 0..40 {
        let run = Run::new(&row(key, 1_200_000, 2_400_000), u32::from(key), &c);
        assert_eq!(run.shape, Shape::Flat);
        assert!((0..20).all(|m| run.minute_core_ms(m) == 120_000));
    }
}

#[test]
fn the_seed_enters_the_class_draw_and_a_key_keeps_its_class() {
    // The control's noise floor must cover class-assignment variance, so the seed has to
    // move classes; and every run of one key must share its class.
    let moved = (0..1000u16)
        .filter(|&k| {
            ShapeMix::DEFAULT.class_of(1, KeyIx(k)) != ShapeMix::DEFAULT.class_of(2, KeyIx(k))
        })
        .count();
    assert!(
        (500..900).contains(&moved),
        "{moved} of 1000 keys changed class"
    );
    let c = cfg(1);
    let a = Run::new(&row(17, 500_000, 1_000_000), 0, &c);
    let b = Run::new(&row(17, 61_000, 5), 99, &c);
    assert_eq!(a.shape, b.shape);
    assert_eq!(a.shape, c.mix.class_of(1, KeyIx(17)));
}

#[test]
fn working_set_ramps_to_the_peak_by_minute_three_or_the_end() {
    // A ramp that overshoots, undershoots or never arrives gives memory kills the wrong
    // timing.
    let c = cfg(1);
    let long = Run::new(
        &Row {
            peak_mem_mib: 1800,
            ..row(0, 900_000, 1)
        },
        0,
        &c,
    );
    assert_eq!(long.working_set_mib(0), 10);
    assert_eq!(long.working_set_mib(89), 900);
    assert_eq!(long.working_set_mib(MEM_RAMP_TICKS - 1), 1800);
    assert_eq!(long.working_set_mib(500), 1800);
    assert_eq!(long.minute_working_set_mib(0), 600);
    assert_eq!(long.minute_working_set_mib(3), 1800);
    assert_eq!(long.minute_working_set_mib(14), 1800);
    let short = Run::new(
        &Row {
            peak_mem_mib: 1000,
            ..row(0, 4_000, 1)
        },
        0,
        &c,
    );
    assert_eq!((short.ticks(), short.working_set_mib(0)), (4, 250));
    assert_eq!(short.working_set_mib(3), 1000);
    assert_eq!(short.minute_working_set_mib(0), 1000);
    let none = Run::new(
        &Row {
            peak_mem_mib: 0,
            ..row(0, 4_000, 1)
        },
        0,
        &c,
    );
    assert_eq!(none.minute_working_set_mib(0), 0);
    // Non-decreasing over the whole run.
    let all: Vec<u64> = (0..long.ticks()).map(|t| long.working_set_mib(t)).collect();
    assert!(all.windows(2).all(|w| w[0] <= w[1]));
}

#[test]
fn the_memory_scenario_scales_one_key_span_in_ten_and_shares_it() {
    // Scenario cpu leaves peaks alone. Scenario mem multiplies about one (key, 60 s) draw
    // in ten by the factor; runs of one key starting in one 60 s span share the draw (a
    // fan-out misses together), and other spans draw again.
    let plain = cfg(1);
    let mem = TruthConfig {
        mem_scale_permille: 1300,
        ..cfg(1)
    };
    let at = |key: u16, start_ms: u64, c: &TruthConfig| {
        Run::new(
            &Row {
                start_ms,
                peak_mem_mib: 1000,
                ..row(key, 5_000, 1)
            },
            0,
            c,
        )
        .peak_mem_mib
    };
    let scaled = (0..2000u16).filter(|&k| at(k, 0, &mem) == 1300).count();
    assert!(
        (140..=260).contains(&scaled),
        "{scaled} of 2000 keys scaled"
    );
    assert!((0..2000u16).all(|k| matches!(at(k, 0, &mem), 1000 | 1300)));
    assert!((0..2000u16).all(|k| at(k, 0, &plain) == 1000));
    for k in 0..2000u16 {
        assert_eq!(
            at(k, 0, &mem),
            at(k, 59_999, &mem),
            "key {k} within one span"
        );
    }
    let spans: Vec<u64> = (0..300).map(|s| at(5, s * 60_000, &mem)).collect();
    assert!(spans.contains(&1000) && spans.contains(&1300));
    assert_eq!(MEM_SPIKE_ONE_IN, 10);
}

#[test]
fn hand_world_run_means_vary_around_the_key_mean() {
    // Over 20,000 leases the mean is the key's (the table has mean 1), the median over P90
    // is the doc's 0.75, and the draw is keyed by lease.
    let mut xs: Vec<u64> = (0..20_000)
        .map(|l| hand_run_cpu_ms(1, l, 1_000_000))
        .collect();
    let sum: u64 = xs.iter().sum();
    assert!(
        sum.abs_diff(1_000_000 * 20_000) * 100 <= 1_000_000 * 20_000,
        "{sum}"
    );
    xs.sort_unstable();
    let (median, p90) = (xs[10_000], xs[18_000]);
    assert!(
        73 * p90 <= 100 * median && 100 * median <= 77 * p90,
        "{median} {p90}"
    );
    assert_eq!(
        hand_run_cpu_ms(1, 7, 1_000_000),
        hand_run_cpu_ms(1, 7, 1_000_000)
    );
    assert_ne!(
        hand_run_cpu_ms(1, 7, 1_000_000),
        hand_run_cpu_ms(1, 8, 1_000_000)
    );
    assert_ne!(
        hand_run_cpu_ms(1, 7, 1_000_000),
        hand_run_cpu_ms(2, 7, 1_000_000)
    );
    assert_eq!(hand_run_cpu_ms(1, 7, 0), 0);
    // Pinned: the factor of lease 7 under seeds 1 and 2 and of lease 8 under seed 1.
    assert_eq!(hand_run_cpu_ms(1, 7, 1_000_000), 712_144);
    assert_eq!(hand_run_cpu_ms(2, 7, 1_000_000), 1_260_902);
    assert_eq!(hand_run_cpu_ms(1, 8, 1_000_000), 1_419_182);
    assert_eq!(hand_run_cpu_ms(1, 7, 2_000), 1_424);
}

#[test]
fn mean_cores_are_the_rows_and_the_minute_millicores_floor() {
    let c = cfg(1);
    let run = Run::new(&row(0, 250_000, 500_000), 0, &c);
    assert_eq!(run.mean_millicores(), 2_000);
    assert_eq!(Run::new(&row(0, 3, 10), 0, &c).mean_millicores(), 3_333);
    assert_eq!(Run::new(&row(0, 1, 0), 0, &c).mean_millicores(), 0);
    let flat = Run::new(&row(key_of(Shape::Flat, &c), 120_000, 240_000), 0, &c);
    assert_eq!(
        (flat.minute_millicores(0), flat.minute_millicores(1)),
        (2_000, 2_000)
    );
    let part = Run::new(&row(key_of(Shape::Flat, &c), 90_000, 90_000), 0, &c);
    assert_eq!(part.minute_millicores(1), 1_000);
}

#[test]
fn apportion_cuts_exactly_ties_to_the_lower_index_and_never_feeds_a_zero() {
    assert_eq!(apportion(10, &[]), Vec::<u64>::new());
    assert_eq!(apportion(10, &[1, 1, 1]), vec![4, 3, 3]);
    assert_eq!(apportion(11, &[1, 1, 1]), vec![4, 4, 3]);
    assert_eq!(apportion(7, &[0, 5, 0, 5]), vec![0, 4, 0, 3]);
    assert_eq!(apportion(5, &[3, 1]), vec![4, 1]);
    // The largest remainder wins, not the largest weight: 10 * 2/7 = 2.857, 10 * 5/7 = 7.14.
    assert_eq!(apportion(10, &[2, 5]), vec![3, 7]);
    // All-zero weights cut equally.
    assert_eq!(apportion(7, &[0, 0, 0]), vec![3, 2, 2]);
    assert_eq!(apportion(0, &[4, 9]), vec![0, 0]);
    // The widest inputs neither overflow nor lose a unit.
    let parts = apportion(u64::MAX, &[u128::from(u64::MAX), 1, u128::from(u64::MAX)]);
    assert_eq!(parts.iter().sum::<u64>(), u64::MAX);
    // A scatter of totals and weights always sums.
    for total in [1u64, 59, 1_000, 123_456_789] {
        let weights: Vec<u128> = (1..40u128).map(|i| i * i % 13).collect();
        assert_eq!(apportion(total, &weights).iter().sum::<u64>(), total);
    }
}
