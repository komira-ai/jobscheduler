//! The well-formedness of a job's CPU series: which granularities and lengths are kept.

use js_types::{CPU_SERIES_MAX_POINTS, CpuSeries};

/// Asserts that a series with a power-of-two granularity that fits 16 bits and at most 128
/// points is kept as given, with its run length and its partial flag.
#[test]
fn a_well_formed_series_is_kept_as_given() {
    let series = CpuSeries::new(4, vec![0, 16, 65535], 11_500_000, true).unwrap();
    assert_eq!(series.granularity_secs, 4);
    assert_eq!(series.points, vec![0, 16, 65535]);
    assert_eq!(series.run_usec, 11_500_000);
    assert!(series.partial);
    assert!(CpuSeries::new(1, Vec::new(), 0, false).is_some());
    assert!(CpuSeries::new(32768, vec![1], 1, false).is_some());
    assert!(CpuSeries::new(1, vec![0; CPU_SERIES_MAX_POINTS], 1, false).is_some());
}

/// Asserts that a granularity of zero, one that is not a power of two, one that does not fit 16
/// bits and a series of 129 points are not kept. Catches a series stored that no agent
/// writes, which a reader would then have to guard against.
#[test]
fn a_series_that_is_not_well_formed_is_refused() {
    for granularity in [0, 3, 6, 65536, 1 << 31] {
        assert!(
            CpuSeries::new(granularity, vec![1], 1, false).is_none(),
            "{granularity}"
        );
    }
    assert!(CpuSeries::new(1, vec![0; CPU_SERIES_MAX_POINTS + 1], 1, false).is_none());
}
