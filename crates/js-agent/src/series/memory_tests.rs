use super::*;

const MIB: u64 = 1 << 20;
const S: u64 = SECOND_USEC;

/// A series started at 0 given `samples` as `(seconds, MiB)` pairs, finished at `end` seconds.
fn series(samples: &[(u64, u64)], end: u64) -> MemorySeries {
    let mut recorder = MemoryRecorder::starting(0);
    for &(at, mib) in samples {
        recorder.observe(at * S, mib * MIB);
    }
    recorder.finish(end * S).expect("a series")
}

/// Asserts that a point keeps the largest of the samples in its span, not the last or the
/// mean, and that the run is from the start to the end given. Catches a level averaged like a
/// rate.
#[test]
fn a_point_keeps_the_largest_sample_of_its_span() {
    let got = series(&[(0, 2), (0, 6), (0, 4), (1, 1)], 2);
    assert_eq!(got.points, vec![6, 1]);
    assert_eq!(
        (got.granularity_secs, got.run_usec, got.partial),
        (1, 2 * S, false)
    );
}

/// Asserts that a full series combines neighbours by their maximum: a run of 256 one-second
/// samples alternating 2 and 6 MiB, then 4 and 0, gives 128 points at 2 s, each the larger of
/// its pair. Catches a combine by the mean (4 and 2) or the sum, which a CPU series uses.
#[test]
fn a_full_series_combines_neighbours_by_their_maximum() {
    let samples: Vec<(u64, u64)> = (0..256)
        .map(|second| {
            let mib = match second % 4 {
                0 => 2,
                1 => 6,
                2 => 4,
                _ => 0,
            };
            (second, mib)
        })
        .collect();
    let got = series(&samples, 256);
    assert_eq!(got.granularity_secs, 2);
    assert_eq!(got.points.len(), 128);
    assert!(
        got.points.chunks(2).all(|pair| pair == [6, 4]),
        "{:?}",
        got.points
    );
}

/// Asserts that the granularity doubles exactly when the series is full: samples each second
/// for 128 s fit at 1 s a point; one at 128 s makes 65 points at 2 s, and one at an hour makes
/// 4096 s a point (at most 128 points). Catches a combine one point early or late.
#[test]
fn the_granularity_doubles_exactly_when_the_series_is_full() {
    let upto = |last: u64| -> Vec<(u64, u64)> { (0..=last).map(|s| (s, 1)).collect() };
    let got = series(&upto(127), 128);
    assert_eq!((got.granularity_secs, got.points.len()), (1, 128));
    let got = series(&upto(128), 129);
    assert_eq!((got.granularity_secs, got.points.len()), (2, 65));
    let got = series(&[(0, 1), (3_600, 1)], 3_600);
    assert_eq!(got.granularity_secs, 32);
    assert_eq!(got.points.len(), 113);
}

/// Asserts that a span with no sample of its own holds the last sample before it: samples of
/// 5 MiB at 0 s and 3 MiB at 4 s give 5, 5, 5, 5, 3. Catches a pause of the agent read as a job
/// that held nothing.
#[test]
fn a_gap_holds_the_last_sample() {
    assert_eq!(series(&[(0, 5), (4, 3)], 5).points, vec![5, 5, 5, 5, 3]);
}

/// Asserts that a point is in MiB rounded up, so a byte over a MiB is two, and that a point too
/// large for 32 bits saturates. Catches a series that reads below the job's peak.
#[test]
fn a_point_is_in_mib_rounded_up() {
    let mut recorder = MemoryRecorder::starting(0);
    recorder.observe(0, MIB + 1);
    recorder.observe(S, MIB);
    recorder.observe(2 * S, u64::MAX);
    let got = recorder.finish(3 * S).unwrap();
    assert_eq!(got.points, vec![2, 1, u32::MAX]);
}

/// Asserts what is not a sample: one earlier than the last, and one before the series' start,
/// add nothing. Catches a late reading of the clock that rewrites a point already taken.
#[test]
fn a_sample_out_of_order_adds_nothing() {
    let mut recorder = MemoryRecorder::starting(10 * S);
    recorder.observe(5 * S, 99 * MIB);
    recorder.observe(12 * S, 4 * MIB);
    recorder.observe(11 * S, 99 * MIB);
    let got = recorder.finish(13 * S).unwrap();
    assert_eq!(got.points, vec![0, 0, 4]);
    assert_eq!(got.run_usec, 3 * S);
}

/// Asserts the end of the series: the time the run was seen to end when there is one (not the
/// later time it is finished at), else the time given, and never before the last sample; no
/// series for a run with no sample or with no time. Catches a series stretched to the moment
/// the result was written.
#[test]
fn the_series_ends_where_the_run_was_seen_to_end() {
    let mut recorder = MemoryRecorder::starting(0);
    recorder.observe(S, MIB);
    recorder.end_at(3 * S);
    assert_eq!(recorder.clone().finish(9 * S).unwrap().run_usec, 3 * S);
    let mut early = MemoryRecorder::starting(0);
    early.observe(4 * S, MIB);
    early.end_at(2 * S);
    assert_eq!(early.finish(9 * S).unwrap().run_usec, 4 * S);
    assert_eq!(MemoryRecorder::starting(0).finish(S), None);
    let mut instant = MemoryRecorder::starting(0);
    instant.observe(0, MIB);
    assert_eq!(instant.finish(0), None);
}

/// Asserts that an adopted run's series is partial and starts at its first sample. Catches the
/// time before the agent's restart counted as a run with nothing held.
#[test]
fn an_adopted_run_starts_at_its_first_sample_and_is_partial() {
    let mut recorder = MemoryRecorder::adopted();
    recorder.observe(100 * S, 7 * MIB);
    recorder.observe(101 * S, 2 * MIB);
    let got = recorder.finish(102 * S).unwrap();
    assert_eq!(got.points, vec![7, 2]);
    assert_eq!((got.run_usec, got.partial), (2 * S, true));
}
