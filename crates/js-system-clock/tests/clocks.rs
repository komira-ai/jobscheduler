//! The system clocks read the system's time through the `js-types` traits.

use js_system_clock::{SystemClock, SystemMonotonic};
use js_types::{Clock, Monotonic};

/// Asserts that the system wall clock reads a time after the epoch.
#[test]
fn the_wall_clock_reads_after_the_epoch() {
    assert!(SystemClock.now().millis() > 0);
}

/// Asserts that the monotonic clock never reads less than it read before: a second reading
/// is at least the first. It bounds no elapsed time.
#[test]
fn the_monotonic_clock_never_runs_backwards() {
    let clock = SystemMonotonic::default();
    let first = clock.reading();
    let second = clock.reading();
    assert!(second >= first);
}
