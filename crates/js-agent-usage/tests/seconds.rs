//! The node's CPU per second over samples built by hand, with the times given: busy and
//! pressure rates, a counter that falls, an interval longer than a second, missing
//! pressure, the cap and the one-second gap.

use js_agent_usage::{SECOND_USEC, SecondRates, SecondSample, Seconds};

const S: u64 = SECOND_USEC;

fn at(mono: u64, cpu: u64, pressure: Option<u64>) -> SecondSample {
    SecondSample {
        mono_usec: mono,
        cpu_usage_usec: cpu,
        pressure_some_usec: pressure,
    }
}

/// Asserts the arithmetic: 2.5 cores busy over a second reads 2500 and a quarter of the
/// second waiting reads 250, each interval against the one before, and `take` hands the
/// values over oldest first and leaves the series empty. Catches a rate divided by the
/// wrong span, a first sample that gives a value, and values that are handed over twice.
#[test]
fn a_rate_is_the_counter_step_over_the_interval() {
    let mut seconds = Seconds::new(16);
    seconds.observe(at(10 * S, 1_000, Some(500)));
    assert_eq!(seconds.take(), SecondRates::default());
    seconds.observe(at(11 * S, 1_000 + 2_500_000, Some(500 + 250_000)));
    seconds.observe(at(12 * S, 1_000 + 2_500_000, Some(500 + 250_000 + 1)));
    assert_eq!(
        seconds.take(),
        SecondRates {
            cpu_millis: vec![2500, 0],
            pressure_some_permille: vec![250, 0],
        }
    );
    assert_eq!(seconds.take(), SecondRates::default());
    seconds.observe(at(13 * S, 1_000 + 2_500_000 + 500_000, None));
    assert_eq!(
        seconds.take().cpu_millis,
        vec![500],
        "the baseline outlives a take"
    );
}

/// Asserts that an interval of three seconds gives one value over three seconds, not three
/// values. Catches a rate that assumes a one-second interval.
#[test]
fn an_interval_longer_than_a_second_gives_one_value_over_its_length() {
    let mut seconds = Seconds::new(16);
    seconds.observe(at(0, 0, Some(0)));
    seconds.observe(at(3 * S, 6_000_000, Some(1_500_000)));
    assert_eq!(
        seconds.take(),
        SecondRates {
            cpu_millis: vec![2000],
            pressure_some_permille: vec![500],
        }
    );
}

/// Asserts that a counter that fell (the group was recreated) gives 0 for that interval, for
/// busy and for pressure, and that the interval after it is measured from the lower value.
/// Catches a wrapped subtraction, which would be a huge rate.
#[test]
fn a_counter_that_falls_gives_zero() {
    let mut seconds = Seconds::new(16);
    seconds.observe(at(0, 5_000_000, Some(900_000)));
    seconds.observe(at(S, 100, Some(10)));
    seconds.observe(at(2 * S, 100 + 1_000_000, Some(10 + 100_000)));
    assert_eq!(
        seconds.take(),
        SecondRates {
            cpu_millis: vec![0, 1000],
            pressure_some_permille: vec![0, 100],
        }
    );
}

/// Asserts that pressure never reads above 1000 (a total that grew by more than the
/// interval) and that busy saturates at `u32::MAX`. Catches a share past the whole interval
/// and an overflow in the multiplication by 1000.
#[test]
fn pressure_is_held_to_a_thousand_and_busy_saturates() {
    let mut seconds = Seconds::new(16);
    seconds.observe(at(0, 0, Some(0)));
    seconds.observe(at(S, u64::MAX, Some(3 * S)));
    seconds.observe(at(S + 1, u64::MAX, Some(3 * S)));
    assert_eq!(
        seconds.take(),
        SecondRates {
            cpu_millis: vec![u32::MAX, 0],
            pressure_some_permille: vec![1000, 0],
        }
    );
}

/// Asserts that a sample without pressure, and the one after it (whose interval has no
/// pressure at its start), give busy values and no pressure value, and that pressure comes
/// back with the second reading in a row that has it. Catches a pressure of 0 invented for
/// "not measured" and a pressure delta taken against a missing reading.
#[test]
fn missing_pressure_gives_busy_values_alone() {
    let mut seconds = Seconds::new(16);
    seconds.observe(at(0, 0, None));
    seconds.observe(at(S, 1_000_000, None));
    seconds.observe(at(2 * S, 2_000_000, Some(7)));
    seconds.observe(at(3 * S, 3_000_000, Some(7 + 500_000)));
    assert_eq!(
        seconds.take(),
        SecondRates {
            cpu_millis: vec![1000, 1000, 1000],
            pressure_some_permille: vec![500],
        }
    );
}

/// Asserts that a series of cap 3 keeps the newest three values of each kind in order and
/// that a cap of exactly the values seen drops none. Catches the oldest kept in place of the
/// newest and an off-by-one at the cap.
#[test]
fn the_cap_keeps_the_newest_values() {
    let mut seconds = Seconds::new(3);
    seconds.observe(at(0, 0, Some(0)));
    for i in 1..=5_u64 {
        seconds.observe(at(i * S, i * i * 1_000_000, Some(i * 100_000)));
    }
    assert_eq!(
        seconds.take(),
        SecondRates {
            cpu_millis: vec![5000, 7000, 9000],
            pressure_some_permille: vec![100, 100, 100],
        }
    );
    let mut exact = Seconds::new(2);
    exact.observe(at(0, 0, None));
    exact.observe(at(S, 1_000_000, None));
    exact.observe(at(2 * S, 3_000_000, None));
    assert_eq!(exact.take().cpu_millis, vec![1000, 2000]);
}

/// Asserts that a sample at the time of the last one, or before it, gives no value and
/// becomes the baseline. Catches a division by zero.
#[test]
fn a_sample_that_is_not_later_gives_no_value() {
    let mut seconds = Seconds::new(16);
    seconds.observe(at(5 * S, 0, Some(0)));
    seconds.observe(at(5 * S, 10, Some(10)));
    seconds.observe(at(4 * S, 20, Some(20)));
    assert_eq!(seconds.take(), SecondRates::default());
    seconds.observe(at(5 * S, 1_020, Some(20)));
    assert_eq!(
        seconds.take().cpu_millis,
        vec![1],
        "measured from the last sample"
    );
}

/// Asserts `due`: always before the first sample, not until a whole second after the last
/// one (a microsecond short is not due), and not when the clock reads before it. Catches `>`
/// for `>=` and a gap measured from the wrong sample.
#[test]
fn due_asks_a_whole_second_between_samples() {
    let mut seconds = Seconds::new(16);
    assert!(seconds.due(0));
    seconds.observe(at(7 * S, 0, None));
    assert!(!seconds.due(7 * S));
    assert!(!seconds.due(8 * S - 1));
    assert!(seconds.due(8 * S));
    assert!(!seconds.due(6 * S));
    let _ = seconds.take();
    assert!(!seconds.due(7 * S + 1), "a take does not reset the gap");
}
