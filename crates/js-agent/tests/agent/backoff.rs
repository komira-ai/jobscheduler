//! The backoff and the system's jitter.

use std::time::Duration;

use js_agent::{BASE, Backoff, CAP, Jitter, RETRY_AFTER_CAP, SystemJitter};

use crate::support::{Highest, Lowest};

/// Asserts the waits after a run of failures: the ceiling doubles from one second to the
/// cap, the wait is half the ceiling plus the jitter (so between half and all of it), the
/// cap holds however long the run, and a success starts the run over. It catches a
/// backoff that grows without bound, one with no jitter (a fleet that failed together
/// returns together) and one that never recovers.
#[test]
fn the_wait_doubles_to_the_cap_and_a_success_resets_it() {
    let secs = |n: f64| Duration::from_secs_f64(n);
    let mut backoff = Backoff::default();
    let longest: Vec<Duration> = (0..9).map(|_| backoff.next(&mut Highest, None)).collect();
    let expected = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 60.0, 60.0, 60.0].map(secs);
    assert_eq!(longest, expected);

    let mut backoff = Backoff::default();
    let shortest: Vec<Duration> = (0..9).map(|_| backoff.next(&mut Lowest, None)).collect();
    let expected = [0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 30.0, 30.0, 30.0].map(secs);
    assert_eq!(shortest, expected);

    backoff.reset();
    assert_eq!(backoff.next(&mut Lowest, None), BASE / 2);
    for _ in 0..100 {
        assert!(backoff.next(&mut Highest, None) <= CAP);
    }
}

/// A wait the server asked for raises the wait to that but never past the bound, and never
/// lowers a wait that is already longer.
#[test]
fn a_requested_wait_raises_the_backoff_within_a_bound() {
    let mut backoff = Backoff::default();
    let asked = Some(Duration::from_secs(10));
    assert_eq!(backoff.next(&mut Lowest, asked), Duration::from_secs(10));
    let huge = Some(Duration::from_secs(86_400));
    assert_eq!(backoff.next(&mut Lowest, huge), RETRY_AFTER_CAP);
    let tiny = Some(Duration::from_millis(1));
    assert_eq!(backoff.next(&mut Lowest, tiny), Duration::from_secs(2));
}

/// The system's jitter stays inside its range, including the ends of what a number can
/// be.
#[test]
fn the_system_jitter_stays_in_range() {
    let mut jitter = SystemJitter;
    assert_eq!(jitter.up_to(0), 0);
    for _ in 0..200 {
        assert!(jitter.up_to(9) <= 9);
    }
    let _ = jitter.up_to(u64::MAX);
}
