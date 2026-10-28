//! The jitter and burst draws of a run, checked against the keyed draws they are defined
//! by: the shift of a shaped run, its per-minute noise, its window bursts, and the split
//! of a window over its ticks.

use std::collections::BTreeSet;

use super::row;
use super::series::{cfg, key_of};
use crate::engine::fixture::KeyIx;
use crate::engine::tables::{LN15, LN32, draw};
use crate::engine::truth::{
    DOMAIN_BURST, DOMAIN_NOISE, DOMAIN_SHIFT, Fidelity, Run, Shape, ShapeMix, TruthConfig,
    WINDOW_TICKS, mix, shifted,
};

/// First minute of the run with any demand.
fn first_busy(run: &Run) -> usize {
    (0..run.minutes())
        .find(|&m| run.minute_core_ms(m) > 0)
        .expect("a run with cpu has a busy minute")
}

#[test]
fn the_shift_is_the_keyed_draw_and_moves_the_prelude_by_whole_minutes() {
    // An idle-prelude run of 24 minutes is idle for its first 4. A shift of -1, 0 or +1
    // minute makes the first busy minute 3, 4 or 5, and the shift is the keyed draw
    // `mix(seed, DOMAIN_SHIFT, lease, 0) % 3 - 1`. A shift that is always 0, is
    // half a minute, is drawn from fewer values or from another domain or subject leaves
    // this.
    let wall = 24 * 60_000;
    let mut seen = BTreeSet::new();
    for seed in 1..=2 {
        let c = cfg(seed);
        let key = key_of(Shape::IdlePrelude, &c);
        for lease in 0..60u32 {
            let run = Run::new(&row(key, wall, 2_000_000), lease, &c);
            let shift = (mix(seed, DOMAIN_SHIFT, u64::from(lease), 0) % 3) as i64 - 1;
            assert_eq!(
                first_busy(&run) as i64,
                4 + shift,
                "seed {seed} lease {lease}"
            );
            seen.insert(first_busy(&run));
        }
    }
    assert_eq!(seen, BTreeSet::from([3, 4, 5]));
    // With identical fidelity there is no shift at all.
    let c = TruthConfig {
        fidelity: Fidelity::Identical,
        ..cfg(1)
    };
    let key = key_of(Shape::IdlePrelude, &c);
    assert!((0..20).all(|l| first_busy(&Run::new(&row(key, wall, 2_000_000), l, &c)) == 4));
}

#[test]
fn shifted_reads_the_template_a_whole_number_of_minutes_away_with_the_ends_held() {
    // Delayed by a minute, tick t reads tick t - 60 (not below the first); advanced, it
    // reads t + 60 and never past the last tick, where a ramp would exceed its range.
    for t in 0..100 {
        assert_eq!(shifted(t, 0, 100), t);
    }
    assert_eq!(shifted(0, 1, 100), 0);
    assert_eq!(shifted(59, 1, 100), 0);
    assert_eq!(shifted(60, 1, 100), 0);
    assert_eq!(shifted(61, 1, 100), 1);
    assert_eq!(shifted(99, 1, 100), 39);
    assert_eq!(shifted(10, -1, 100), 70);
    assert_eq!(shifted(39, -1, 100), 99);
    assert_eq!(shifted(40, -1, 100), 99);
    assert_eq!(shifted(99, -1, 100), 99);
}

#[test]
fn minute_noise_is_the_ln15_draw_of_its_lease_seed_and_minute() {
    // Past the prelude an idle-prelude run is level, so its minutes are proportional to
    // their noise: `minute[a] * noise[b]` equals `minute[b] * noise[a]` up to the one
    // core-ms each minute's cut can be off by. The noise is recomputed here from the
    // keyed draw, so a minute that reads the wrong table, domain, seed, lease or index
    // (two minutes sharing a draw) breaks the proportion.
    for seed in 1..=2 {
        let c = cfg(seed);
        let key = key_of(Shape::IdlePrelude, &c);
        for lease in 0..8u32 {
            let run = Run::new(&row(key, 24 * 60_000, 2_000_000_000), lease, &c);
            // Minutes 5.. are level whatever the shift (-1, 0 or +1).
            let noise: Vec<u128> = (0..24u64)
                .map(|m| {
                    let r = mix(seed, DOMAIN_NOISE, u64::from(lease), m);
                    u128::from(draw(&LN15, r))
                })
                .collect();
            for a in 5..24 {
                for b in (a + 1)..24 {
                    let (ca, cb) = (
                        u128::from(run.minute_core_ms(a)),
                        u128::from(run.minute_core_ms(b)),
                    );
                    let cross = (ca * noise[b]).abs_diff(cb * noise[a]);
                    assert!(
                        cross <= noise[a] + noise[b],
                        "seed {seed} lease {lease} minutes {a} {b}"
                    );
                }
            }
        }
    }
}

#[test]
fn window_bursts_are_the_ln32_draw_of_their_lease_seed_and_window_of_the_run() {
    // A flat key has no shift and no noise, so its windows are proportional to their
    // bursts alone. The burst is recomputed from the keyed draw with the window counted
    // from the start of the RUN: a window indexed within its minute would repeat the
    // same twelve bursts every minute, and the 5 s p99 would be correlated across them.
    for seed in 1..=2 {
        let c = cfg(seed);
        let key = key_of(Shape::Flat, &c);
        for lease in 3..5u32 {
            let run = Run::new(&row(key, 3_600_000, 7_200_000_000), lease, &c);
            let mut vectors = BTreeSet::new();
            for m in [0usize, 1, 2, 7, 30, 59] {
                let windows = run.minute_windows(m);
                vectors.insert(windows.clone());
                let burst: Vec<u128> = (0..12u64)
                    .map(|j| {
                        let r = mix(seed, DOMAIN_BURST, u64::from(lease), 12 * m as u64 + j);
                        u128::from(draw(&LN32, r))
                    })
                    .collect();
                for a in 0..12 {
                    for b in (a + 1)..12 {
                        let (wa, wb) = (u128::from(windows[a]), u128::from(windows[b]));
                        let cross = (wa * burst[b]).abs_diff(wb * burst[a]);
                        assert!(
                            cross <= burst[a] + burst[b],
                            "seed {seed} lease {lease} minute {m} windows {a} {b}"
                        );
                    }
                }
            }
            assert_eq!(vectors.len(), 6, "every minute has bursts of its own");
        }
    }
}

#[test]
fn a_windows_core_ms_are_split_over_its_ticks_with_the_odd_ones_first() {
    // The split of a window over its ticks is equal up to one core-ms, and the first
    // `window % ticks` ticks are the ones carrying the extra, in a full window of five
    // and in the short last one.
    let c = cfg(1);
    let key = key_of(Shape::Flat, &c);
    let mut uneven = 0;
    for (wall, cpu) in [(3_725_000, 1_000_003), (61_000, 6_101)] {
        let run = Run::new(&row(key, wall, cpu), 2, &c);
        for m in 0..run.minutes() {
            let windows = run.minute_windows(m);
            let ticks = run.minute_ticks(m);
            for (window, chunk) in windows.iter().zip(ticks.chunks(WINDOW_TICKS as usize)) {
                assert_eq!(chunk.iter().sum::<u64>(), *window);
                assert!(chunk.windows(2).all(|w| w[0] >= w[1]), "{chunk:?}");
                assert!(chunk[0] - chunk[chunk.len() - 1] <= 1, "{chunk:?}");
                uneven += usize::from(chunk[0] != chunk[chunk.len() - 1]);
            }
        }
    }
    assert!(uneven > 10, "{uneven} windows had an odd core-ms to place");
}

#[test]
fn neighbouring_keys_draw_their_classes_independently() {
    // Two keys with adjacent indices are the same class as often as chance gives: the
    // sum of squared shares, 25.5% for the default mix. A class drawn from a coarser
    // subject (keys sharing a bucket) would make neighbours equal most of the time.
    let same = (0..20_000u16)
        .filter(|&k| {
            ShapeMix::DEFAULT.class_of(1, KeyIx(k)) == ShapeMix::DEFAULT.class_of(1, KeyIx(k + 1))
        })
        .count();
    assert!((4_400..=5_800).contains(&same), "{same} of 20000 pairs");
}
