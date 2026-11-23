use super::*;

const S: u64 = 1_000_000;

/// A series of a run made at 0 that uses `cores` (in sixteenths of a core) in each of its
/// first `seconds` seconds, sampled once a second; `cores` gives the rate of second `i`.
fn steady(seconds: u64, cores: impl Fn(u64) -> u64) -> Option<CpuSeries> {
    let mut recorder = SeriesRecorder::starting(0, 0);
    let mut used = 0;
    for second in 0..seconds {
        used += cores(second) * S / 16;
        if second + 1 < seconds {
            recorder.observe((second + 1) * S, used);
        }
    }
    recorder.finish(seconds * S, used)
}

fn series(granularity_secs: u32, points: Vec<u16>, run_usec: u64) -> Option<CpuSeries> {
    Some(CpuSeries {
        granularity_secs,
        points,
        run_usec,
        partial: false,
    })
}

/// Catches a combine by the max (or the sum) of a pair: points of 2, 6, 4 and 0 sixteenths of
/// a core over 129 seconds combine into 4 and 2, their means, and a point over the 129th
/// second follows them.
#[test]
fn a_full_series_combines_neighbours_by_their_mean() {
    let pattern = [2, 6, 4, 0];
    let got = steady(129, |second| pattern[(second % 4) as usize]).expect("a series");
    assert_eq!(got.granularity_secs, 2);
    assert_eq!(got.points.len(), 65);
    let mut want: Vec<u16> = [4, 2].repeat(32);
    // The 129th second ran at the first value of the pattern, 2, alone in its point.
    want.push(2);
    assert_eq!(got.points, want);
}

/// Catches a granularity that doubles before the series is full (or never): 128 seconds fill
/// the 128 points at 1 s, and the 129th second is what doubles it.
#[test]
fn the_granularity_doubles_exactly_when_the_series_is_full() {
    let full = steady(128, |_| 16).expect("a series");
    assert_eq!((full.granularity_secs, full.points.len()), (1, 128));
    let past = steady(129, |_| 16).expect("a series");
    assert_eq!((past.granularity_secs, past.points.len()), (2, 65));
}

/// Catches the wrong number of points or granularity for runs of the lengths the design
/// names: 1 s is one point at 1 s; 128 s is 128 at 1 s; 129 s is 65 at 2 s; an hour is 113 at
/// 32 s (2,048 s fill 128 points at 16 s; at 32 s an hour needs ceil(3600 / 32) = 113).
#[test]
fn runs_of_one_second_to_an_hour_give_the_expected_points_and_granularity() {
    for (seconds, granularity, count) in [
        (1, 1, 1),
        (128, 1, 128),
        (129, 2, 65),
        (256, 2, 128),
        (257, 4, 65),
        (3600, 32, 113),
    ] {
        let got = steady(seconds, |_| 32).expect("a series");
        assert_eq!(
            (got.granularity_secs, got.points.len(), got.run_usec),
            (granularity, count, seconds * S),
            "{seconds} s"
        );
        assert!(got.points.iter().all(|&point| point == 32), "{seconds} s");
    }
}

/// Catches CPU across a gap between samples put in the point where the gap ends: one sample
/// after 4 seconds of 8 cores reads as 8 cores in each of the four seconds.
#[test]
fn a_gap_between_samples_spreads_its_cpu_over_the_gap() {
    let mut recorder = SeriesRecorder::starting(0, 0);
    recorder.observe(S, S);
    assert_eq!(
        recorder.finish(5 * S, S + 32 * S),
        series(1, vec![16, 128, 128, 128, 128], 5 * S)
    );
}

/// Catches samples off the second boundaries counted whole in one point: polls at 0.5 s and
/// 1.5 s of a steady 2 cores give 2 cores in each second.
#[test]
fn samples_between_second_boundaries_split_their_cpu_by_time() {
    let mut recorder = SeriesRecorder::starting(0, 0);
    recorder.observe(S / 2, S);
    recorder.observe(3 * S / 2, 3 * S);
    assert_eq!(
        recorder.finish(2 * S, 4 * S),
        series(1, vec![32, 32], 2 * S)
    );
}

/// Catches a gap across the end of the 128 points spread at the old granularity: 200 seconds
/// of one core in a single gap reads as one core in every point at 2 s.
#[test]
fn a_gap_past_the_full_series_is_spread_after_the_combine() {
    let recorder = SeriesRecorder::starting(0, 0);
    assert_eq!(
        recorder.finish(200 * S, 200 * S),
        series(2, vec![16; 100], 200 * S)
    );
}

/// Catches a last point read over a whole point of time the run did not live: a run of 130.5
/// seconds at one core ends in a point at 2 s that covers 2.5 s, still one core.
#[test]
fn the_last_point_is_its_mean_over_the_time_the_run_lived_in_it() {
    let recorder = SeriesRecorder::starting(0, 0);
    let got = recorder
        .finish(130 * S + S / 2, 130 * S + S / 2)
        .expect("a series");
    assert_eq!(got.granularity_secs, 2);
    assert_eq!(got.points, vec![16; 66]);
    assert_eq!(got.run_usec, 130 * S + S / 2);
}

/// Catches a run with no time between its start and its end reporting a series, and samples
/// that go back in time counted.
#[test]
fn a_run_with_no_time_has_no_series_and_late_samples_add_nothing() {
    assert_eq!(SeriesRecorder::starting(7 * S, 3).finish(7 * S, 9), None);
    let mut recorder = SeriesRecorder::starting(2 * S, 0);
    recorder.observe(3 * S, S);
    recorder.observe(S, 50 * S);
    recorder.observe(3 * S, 50 * S);
    assert_eq!(
        recorder.finish(4 * S, 2 * S),
        series(1, vec![16, 16], 2 * S)
    );
}

/// Catches a counter that fell read as a huge rise, and a rate past the largest point
/// wrapping: a fall counts as no use, and 5,000 cores saturate at the largest point.
#[test]
fn a_falling_counter_is_no_use_and_a_huge_rate_saturates() {
    let mut recorder = SeriesRecorder::starting(0, 9 * S);
    recorder.observe(S, S);
    assert_eq!(
        recorder.finish(2 * S, S + 5000 * S),
        series(1, vec![0, u16::MAX], 2 * S)
    );
}

/// Catches an adopted run's series sent as whole, and one that starts before the run's first
/// sample: it takes no CPU from that sample (what the run used before is not known), counts
/// what it used from there, and says it is partial.
#[test]
fn an_adopted_run_starts_at_its_first_sample_and_is_partial() {
    let mut recorder = SeriesRecorder::adopted();
    recorder.observe(10 * S, 400 * S);
    recorder.observe(11 * S, 401 * S);
    let got = recorder.finish(12 * S, 403 * S).expect("a series");
    assert_eq!(
        got,
        CpuSeries {
            granularity_secs: 1,
            points: vec![16, 32],
            run_usec: 2 * S,
            partial: true,
        }
    );
}

/// Catches an adopted run that is never sampled reporting a series: with no sample before its
/// end, its end is its first sample and there is no time in it.
#[test]
fn an_adopted_run_never_sampled_has_no_series() {
    assert_eq!(SeriesRecorder::adopted().finish(5 * S, 9 * S), None);
}

/// Catches a series that runs on to the moment its usage was read: a run seen to end at 3 s is
/// 3 s long even when its usage is read at 10 s, and the counter's last rise is spread over
/// the gap before the end, not up to the later reading.
#[test]
fn a_series_ends_where_the_end_was_seen_not_where_the_usage_was_read() {
    let mut recorder = SeriesRecorder::starting(0, 0);
    recorder.observe(S, S);
    recorder.end_at(3 * S);
    assert_eq!(
        recorder.finish(10 * S, 3 * S),
        series(1, vec![16, 16, 16], 3 * S)
    );
}

/// Catches an end recorded before the last sample moving the series backwards: the run is as
/// long as its last sample, and the counter's rise after it adds nothing.
#[test]
fn an_end_before_the_last_sample_does_not_shorten_the_series() {
    let mut recorder = SeriesRecorder::starting(0, 0);
    recorder.observe(2 * S, 2 * S);
    recorder.end_at(S);
    assert_eq!(
        recorder.finish(5 * S, 9 * S),
        series(1, vec![16, 16], 2 * S)
    );
}

/// Catches rounding that loses a sixteenth: 1/32 of a core rounds up to one point, and less
/// rounds down to none.
#[test]
fn points_round_to_the_nearest_sixteenth_of_a_core() {
    assert_eq!(to_point(S / 32, S), 1);
    assert_eq!(to_point(S / 32 - 1, S), 0);
    assert_eq!(mul_div(u64::MAX, 1, 1), u64::MAX);
}
