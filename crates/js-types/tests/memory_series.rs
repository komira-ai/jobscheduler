//! The well-formedness of a job's memory series, and the peak it gives.

use js_types::{MEMORY_SERIES_BYTES_PER_POINT, MEMORY_SERIES_MAX_POINTS, MemorySeries};

/// Asserts that a series with a power-of-two granularity that fits 16 bits and at most 128
/// points is kept as given, with its run length and its partial flag.
#[test]
fn a_well_formed_series_is_kept_as_given() {
    let series = MemorySeries::new(4, vec![0, 16, u32::MAX], 11_500_000, true).unwrap();
    assert_eq!(series.granularity_secs, 4);
    assert_eq!(series.points, vec![0, 16, u32::MAX]);
    assert_eq!(series.run_usec, 11_500_000);
    assert!(series.partial);
    assert!(MemorySeries::new(1, Vec::new(), 0, false).is_some());
    assert!(MemorySeries::new(32768, vec![1], 1, false).is_some());
    assert!(MemorySeries::new(1, vec![0; MEMORY_SERIES_MAX_POINTS], 1, false).is_some());
}

/// Asserts that a granularity of zero, one that is not a power of two, one that does not fit 16
/// bits and a series of 129 points are not kept. Catches a series stored that no agent writes.
#[test]
fn a_series_that_is_not_well_formed_is_refused() {
    for granularity in [0, 3, 6, 65536, 1 << 31] {
        assert!(
            MemorySeries::new(granularity, vec![1], 1, false).is_none(),
            "{granularity}"
        );
    }
    assert!(MemorySeries::new(1, vec![0; MEMORY_SERIES_MAX_POINTS + 1], 1, false).is_none());
}

/// Asserts the peak of a series: its largest point in bytes (a MiB a unit), wherever it is in
/// the run, none for a series with no point, and the largest point read without overflow.
/// Catches the last or the first point read as the peak, and MiB read as bytes.
#[test]
fn the_peak_is_the_largest_point_in_bytes() {
    let series = |points: Vec<u32>| MemorySeries::new(1, points, 1, false).unwrap();
    assert_eq!(MEMORY_SERIES_BYTES_PER_POINT, 1 << 20);
    assert_eq!(series(vec![3, 9, 2]).peak_bytes(), Some(9 << 20));
    assert_eq!(series(vec![9, 3]).peak_bytes(), Some(9 << 20));
    assert_eq!(series(vec![0]).peak_bytes(), Some(0));
    assert_eq!(series(Vec::new()).peak_bytes(), None);
    assert_eq!(
        series(vec![u32::MAX]).peak_bytes(),
        Some(u64::from(u32::MAX) << 20)
    );
}
