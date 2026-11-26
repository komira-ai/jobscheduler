//! The heavy-tail CPU rule: a key whose recent peaks are far above the median of its means
//! books the 90th percentile of its peaks; every other key books the median as before.

use super::*;

/// A quiet run that averaged `mean` millicores and peaked at `peak` (`None`: no peak).
fn peaked(mean: u64, peak: Option<u32>) -> Run {
    Run {
        cpu_peak_millis: peak,
        ..busy(mean, Some(0))
    }
}

/// The CPU `Summaries::cpu` books for `k` on [`DAY`] under the default bounds.
fn booked(s: &Summaries, k: &Key) -> u32 {
    s.cpu(k, DAY, &Bounds::default()).unwrap()
}

/// Rule: a key with a heavy tail books the 90th percentile of its peaks, above its median.
/// Ten compiles that averaged 2.75 cores and peaked at 16 to 70 book 52 cores (the 9th
/// smallest of ten peaks), where the median of their means is 2.75 cores. Catches a key
/// that keeps booking its median while its runs burst six times over it.
#[test]
fn a_key_with_a_heavy_tail_books_the_90th_percentile_of_its_peaks() {
    let mut s = Summaries::default();
    let k = linux(1);
    let peaks = [
        16_000, 16_000, 16_000, 17_000, 18_000, 20_000, 30_000, 40_000, 52_000, 70_000,
    ];
    for peak in peaks {
        s.observe(&k, &peaked(2750, Some(peak)), DAY, &Bounds::default());
    }
    assert_eq!(booked(&s, &k), 52_000);
    assert_eq!(cpu_estimate_of(&s, &k), 52_000, "the estimate agrees");
    let mut means_only = Summaries::default();
    feed(&mut means_only, &k, &peaked(2750, None), 10);
    assert_eq!(booked(&means_only, &k), 2750, "the same runs without peaks");
}

fn cpu_estimate_of(s: &Summaries, k: &Key) -> u32 {
    estimate(s, k).unwrap().cpu_millis
}

/// Rule: a key whose peaks stay within twice its median books its median, exactly as a key
/// with no peaks. Catches a small steady job booked at its burst.
#[test]
fn a_key_whose_peaks_stay_near_its_mean_books_its_median_unchanged() {
    let k = linux(1);
    let mut with_peaks = Summaries::default();
    let mut without = Summaries::default();
    for (i, mean) in [400u64, 500, 600, 450, 520, 480, 700, 300]
        .into_iter()
        .enumerate()
    {
        let peak = u32::try_from(mean + 250 - i as u64).unwrap();
        with_peaks.observe(&k, &peaked(mean, Some(peak)), DAY, &Bounds::default());
        without.observe(&k, &peaked(mean, None), DAY, &Bounds::default());
    }
    assert_eq!(booked(&with_peaks, &k), 500);
    assert_eq!(booked(&with_peaks, &k), booked(&without, &k));
}

/// Rule: the tail must be over twice the median booking: a 90th percentile of exactly
/// twice it books the median, one millicore more books the percentile rounded up to a
/// quarter core. Catches an off-by-one at the ratio.
#[test]
fn the_tail_must_be_over_twice_the_median_booking() {
    let k = linux(1);
    let at = |peak: u32| {
        let mut s = Summaries::default();
        feed(&mut s, &k, &peaked(1000, Some(peak)), 5);
        booked(&s, &k)
    };
    assert_eq!(at(2000), 1000);
    assert_eq!(at(2001), 2250);
}

/// Rule: the tail is judged only once five of the last 32 samples carry a peak; runs with
/// no peak, or a peak of 0, do not count. Catches one burst run lifting a key, and a run
/// with no peak measured taken as a peak of 0 (which would lower the percentile).
#[test]
fn the_tail_needs_five_runs_with_a_peak() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &peaked(1000, Some(40_000)), TAIL_MIN_PEAKS - 1);
    feed(&mut s, &k, &peaked(1000, None), 3);
    feed(&mut s, &k, &peaked(1000, Some(0)), 3);
    assert_eq!(booked(&s, &k), 1000, "four peaks are not a tail");
    feed(&mut s, &k, &peaked(1000, Some(40_000)), 1);
    assert_eq!(booked(&s, &k), 40_000, "the fifth is");
}

/// Rule: the tail is of the last 32 samples: a key whose bursts stop books its median again
/// once 32 runs without a burst have replaced them. Catches a tail that never ages out.
#[test]
fn a_tail_that_stops_ages_out_after_32_runs() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &peaked(1000, Some(30_000)), 8);
    assert_eq!(booked(&s, &k), 30_000);
    feed(&mut s, &k, &peaked(1000, Some(1200)), 28);
    assert_eq!(
        booked(&s, &k),
        30_000,
        "four bursts of 32 are the 90th percentile"
    );
    feed(&mut s, &k, &peaked(1000, Some(1200)), 4);
    assert_eq!(booked(&s, &k), 1000);
}

/// Rule: a heavy tail books at most the node's CPUs.
#[test]
fn a_tail_books_at_most_the_node() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &peaked(1000, Some(u32::MAX)), 5);
    assert_eq!(booked(&s, &k), Bounds::default().node_cpu_millis);
}

/// Rule: a pressured or held run enters (with its peak) only when its mean is above the
/// median of the key's means, not above its tail booking; otherwise its peak is dropped
/// with it. Catches a pressured run's peak lifting a key whose mean it did not raise, and a
/// tail booking that stops every pressured mean from raising the median.
#[test]
fn a_pressured_run_enters_with_its_peak_only_above_the_median_mean() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &peaked(1000, Some(1500)), 5);
    let low = Run {
        cpu_peak_millis: Some(60_000),
        ..busy(900, Some(500))
    };
    for _ in 0..5 {
        assert_eq!(
            s.observe(&k, &low, DAY, &Bounds::default()).cpu,
            CpuSample::Dropped
        );
    }
    assert_eq!(booked(&s, &k), 1000);
    let mut tailed = Summaries::default();
    feed(&mut tailed, &k, &peaked(1000, Some(30_000)), 5);
    let high = Run {
        cpu_peak_millis: Some(30_000),
        cpu_held: true,
        ..busy(3000, Some(0))
    };
    let seen = tailed.observe(&k, &high, DAY, &Bounds::default());
    assert_eq!(
        seen.cpu,
        CpuSample::Raised,
        "above the median, below the tail"
    );
    assert_eq!(booked(&tailed, &k), 30_000);
}

/// Rule: a run with a peak but no CPU time is no CPU sample: the sample is the mean.
#[test]
fn a_peak_without_cpu_time_is_no_sample() {
    let mut s = Summaries::default();
    let k = linux(1);
    let seen = s.observe(&k, &peaked(0, Some(24_000)), DAY, &Bounds::default());
    assert_eq!(seen.cpu, CpuSample::Skipped);
    assert_eq!(s.cpu(&k, DAY, &Bounds::default()), None);
}

/// Rule 3.5: the peaks survive storage, and a version 2 record (before peaks were kept)
/// still decodes, with no peak, so its key books the median of its means. Catches a
/// record of an older version that no longer decodes, or that decodes to a tail.
#[test]
fn peaks_survive_storage_and_a_version_2_record_books_its_median() {
    let k = linux(1);
    let mut s = Summaries::default();
    feed(&mut s, &k, &peaked(1000, Some(30_000)), 6);
    let summary = s.get(&k).unwrap().clone();
    let bounds = Bounds::default();
    assert_eq!(
        Summary::decode(&summary.encode()).unwrap().cpu(&bounds),
        30_000
    );
    let bytes = summary.encode();
    let mut v2 = bytes[..bytes.len() - 8 - 4 * 32].to_vec();
    v2[0] = 2;
    let sum = crate::codec::checksum(&v2);
    v2.extend_from_slice(&sum.to_le_bytes());
    let old = Summary::decode(&v2).expect("version 2 decodes");
    assert_eq!(old.cpu(&bounds), 1000);
    assert_eq!(old.life_minutes(), summary.life_minutes());
}
