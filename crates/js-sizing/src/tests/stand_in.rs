//! `Summaries::median_memory` as a stand-in: the figure a kind books for a target with no
//! run of its own. A kill floor belongs to the key that was killed: the stand-in's own floor
//! counts only while its kills are frequent (the key is raise-only: two own-limit kill events
//! within `KILL_WINDOW_RUNS` runs, for `RAISE_ONLY_DAYS` days).

use super::*;

fn median_on(s: &Summaries, k: &Key, day: Day) -> Option<u64> {
    s.median_memory(k, day, &Bounds::default())
}

fn median(s: &Summaries, k: &Key) -> Option<u64> {
    median_on(s, k, DAY)
}

/// `peaks_mib` fed in order, one success each.
fn feed_mib(s: &mut Summaries, k: &Key, peaks_mib: &[u64]) {
    for peak in peaks_mib {
        feed(s, k, &success(peak * MIB), 1);
    }
}

/// Rule: one kill event leaves a floor on the stand-in's key ([`Summaries::key_floor`]) but
/// not in what it books for others: a kind of 0.5 GiB runs with one kill that floored it at
/// 16 GiB books 1 GiB for a new target, and the kill is one sample of its booking among the
/// others. Catches a stand-in that books its floor (it booked 16 GiB).
#[test]
fn a_stand_in_leaves_out_the_floor_of_one_kill_event() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(500 * MIB), 10);
    s.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    assert_eq!(floor(&s, &k), 16 * GIB, "the key keeps its floor");
    assert!(!s.get(&k).unwrap().raise_only(DAY));
    assert_eq!(median(&s, &k), Some(GIB), "one kill: not the floor");
    let mut t = Summaries::default();
    feed(&mut t, &k, &success(500 * MIB), 10);
    feed(&mut t, &k, &kill(4 * GIB, GIB, Some(0)), 1);
    assert_eq!(floor(&t, &k), 4 * GIB + 256 * MIB);
    assert_eq!(
        median(&t, &k),
        Some(GIB),
        "the kill is one 4 GiB sample of 11"
    );
}

/// Rule, the farm's measured stand-ins: `busybox_pin`, floored at 8 GiB by one kill at the
/// 4 GiB cold booking, whose last 32 runs peak at 0.21 GiB at the median and 1.45 GiB at most,
/// books 1 GiB for a new target; `mojo_compile`, floored at 19.5 GiB by one kill at 9.75 GiB,
/// whose last 32 runs peak at 2.38 GiB at the median and 3.16 GiB at most, books 3 GiB
/// (`1.2 x 2,437 MiB + 128 MiB`, rounded up to 256 MiB). Catches the 8 GiB and 19.5 GiB both
/// booked before.
#[test]
fn the_farms_busybox_pin_and_mojo_compile_stand_ins_book_their_median() {
    let busybox = [
        90, 110, 130, 150, 160, 170, 180, 190, 200, 205, 208, 210, 212, 213, 214, 215, 215, 220,
        240, 260, 300, 350, 400, 450, 500, 600, 700, 730, 800, 1000, 1200, 1485,
    ];
    let mojo = [
        860, 1000, 1200, 1400, 1600, 1800, 1900, 2000, 2100, 2200, 2250, 2300, 2350, 2400, 2420,
        2430, 2437, 2450, 2500, 2550, 2600, 2650, 2700, 2750, 2800, 2850, 2900, 2950, 3000, 3100,
        3200, 3236,
    ];
    let cases = [
        (
            &busybox,
            kill(4 * GIB, 4 * GIB, Some(8 * GIB)),
            8 * GIB,
            GIB,
        ),
        (
            &mojo,
            kill(39 * GIB / 4, 39 * GIB / 4, Some(39 * GIB / 2)),
            39 * GIB / 2,
            3 * GIB,
        ),
    ];
    for (peaks, killed, kill_floor, books) in cases {
        let mut s = Summaries::default();
        let k = linux(1);
        feed(&mut s, &k, &killed, 1);
        feed_mib(&mut s, &k, peaks);
        assert_eq!(floor(&s, &k), kill_floor);
        assert_eq!(median(&s, &k), Some(books), "floor {kill_floor}");
    }
}

/// `between` successes of 500 MiB between a kill that floors the key at 8 GiB (booked 4 GiB)
/// and one booked at that floor that raises it to 16 GiB: two kill events.
fn killed_twice(between: usize) -> Summaries {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(500 * MIB), 10);
    feed(&mut s, &k, &kill(4 * GIB, GIB, Some(8 * GIB)), 1);
    feed(&mut s, &k, &success(500 * MIB), between);
    feed(&mut s, &k, &kill(8 * GIB, GIB, Some(16 * GIB)), 1);
    s
}

/// Rule: while a stand-in's kills are frequent (two kill events within 256 runs make it
/// raise-only for 7 days), it books its floor for a new target, through the sixth day; on
/// the seventh it books its median again, its floor still on its own key. Catches a stand-in
/// whose floor never lapses (it booked 16 GiB on the seventh day) and one that drops the
/// floor of a kind that keeps being killed.
#[test]
fn a_stand_in_books_its_floor_while_its_kills_are_frequent() {
    let s = killed_twice(10);
    let k = linux(1);
    assert!(s.get(&k).unwrap().raise_only(DAY));
    assert_eq!(floor(&s, &k), 16 * GIB);
    assert_eq!(median(&s, &k), Some(16 * GIB), "frequent kills: the floor");
    let sixth = Day(DAY.0 + RAISE_ONLY_DAYS - 1);
    assert_eq!(median_on(&s, &k, sixth), Some(16 * GIB));
    let seventh = Day(DAY.0 + RAISE_ONLY_DAYS);
    assert_eq!(median_on(&s, &k, seventh), Some(GIB), "the window lapsed");
    assert_eq!(s.key_floor(&k, &Bounds::default()), 16 * GIB);
}

/// Rule, boundary: kills are frequent when the second event is within 256 runs of the first
/// (254 runs between them, the second the 255th run after the first), not at 255 between.
/// Catches a window one run too wide (the floor booked at 255 between) or too narrow.
#[test]
fn kills_are_frequent_only_within_256_runs() {
    let k = linux(1);
    for (between, books) in [(254, 16 * GIB), (255, GIB)] {
        let s = killed_twice(between);
        assert_eq!(floor(&s, &k), 16 * GIB);
        assert_eq!(median(&s, &k), Some(books), "{between} between");
    }
}
