//! The quantile tables and the draw that reads them.

use crate::engine::tables::{LEN, LN15, LN32, LN2245, ONE_PPM, Table, draw};

const ALL: [(&str, &Table); 3] = [("LN32", &LN32), ("LN15", &LN15), ("LN2245", &LN2245)];

/// The draw whose top 20 bits are `r`.
fn at(table: &Table, r: u64) -> u64 {
    draw(table, r << 44)
}

/// The sum of the value over every one of the 2^20 distinct draws.
fn total(table: &Table) -> u64 {
    (0..1u64 << 20).map(|r| at(table, r)).sum()
}

#[test]
fn tables_are_full_positive_and_non_decreasing() {
    // A generator that sorts wrongly, drops an entry or emits a zero is caught here.
    for (name, table) in ALL {
        assert_eq!(table.len(), LEN, "{name}");
        assert!(table.iter().all(|&v| v > 0), "{name} has a zero");
        assert!(
            table.windows(2).all(|w| w[0] <= w[1]),
            "{name} is not monotone"
        );
    }
}

#[test]
fn drawn_distribution_has_mean_one_within_a_twentieth_of_a_percent() {
    // The mean of what `draw` really samples (interpolation included), exactly, over
    // every draw: a table not renormalised, or an interpolation that leans, shifts every
    // run's demand by the same factor.
    let n = 1u64 << 20;
    for (name, table) in ALL {
        let sum = total(table);
        let off = sum.abs_diff(ONE_PPM * n);
        assert!(
            off * 2000 <= ONE_PPM * n,
            "{name}: mean off by {off}/{}",
            ONE_PPM * n
        );
    }
}

#[test]
fn burst_table_p99_is_two_and_the_others_have_their_spreads() {
    // A wrong sigma (a generator mistake that keeps the mean) moves these quantiles.
    let p = |table: &Table, per_mille: u64| at(table, (per_mille << 20) / 1000);
    let p99 = p(&LN32, 990);
    assert!((1_900_000..=2_100_000).contains(&p99), "LN32 p99 {p99}");
    // Median over P90 of the hand-world run means: the doc's 0.75.
    let (median, p90) = (p(&LN2245, 500), p(&LN2245, 900));
    assert!(
        74 * p90 <= 100 * median && 100 * median <= 76 * p90,
        "LN2245 median {median}, p90 {p90}"
    );
    // The shape noise is gentler: median over P90 near 0.82.
    let (median, p90) = (p(&LN15, 500), p(&LN15, 900));
    assert!(
        81 * p90 <= 100 * median && 100 * median <= 84 * p90,
        "LN15 median {median}, p90 {p90}"
    );
}

#[test]
fn draw_interpolates_between_entries_and_holds_the_last() {
    // Entry i at 1000 * i; the top ten bits pick the entry, the low ten the fraction.
    let mut t: Table = [0; LEN];
    for (i, v) in t.iter_mut().enumerate() {
        *v = 1000 * i as u32;
    }
    assert_eq!(at(&t, 0), 0);
    assert_eq!(at(&t, 1), 0, "1/1024 of 1000 floors to 0");
    assert_eq!(at(&t, 512), 500);
    assert_eq!(at(&t, 1023), 999);
    assert_eq!(at(&t, 1024), 1000);
    assert_eq!(at(&t, 3 * 1024 + 256), 3250);
    // Past the last entry the value is held, not read out of bounds.
    assert_eq!(at(&t, (1 << 20) - 1), 1_023_000);
    assert_eq!(draw(&t, u64::MAX), 1_023_000);
    // Only the top 20 bits matter.
    assert_eq!(draw(&t, (7 << 44) | ((1 << 44) - 1)), draw(&t, 7 << 44));
}

#[test]
fn draw_is_non_decreasing_in_the_draw() {
    // A draw that reorders quantiles would make a large `u` a small burst.
    for (name, table) in ALL {
        let mut last = 0;
        for r in (0..1u64 << 20).step_by(37) {
            let v = at(table, r);
            assert!(v >= last, "{name} at {r}");
            last = v;
        }
    }
}

#[test]
fn table_ends_are_pinned() {
    // The committed integers, at the two ends: an accidental regeneration or a table
    // edited by hand shows here first.
    assert_eq!(at(&LN32, 0), 330_419);
    assert_eq!(draw(&LN32, u64::MAX), 2_725_932);
    assert_eq!((LN15[0], LN15[LEN - 1]), (602_715, 1_620_683));
    assert_eq!((LN2245[0], LN2245[LEN - 1]), (464_797, 2_042_726));
}
