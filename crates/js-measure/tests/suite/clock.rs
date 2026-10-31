//! The clocks.

use std::time::Instant;

use js_measure::{Clock, FakeClock, SystemClock};

/// Asserts that the fake clock moves only when something waits on it, and by the wait.
#[test]
fn the_fake_clock_moves_by_the_wait() {
    let clock = FakeClock::at(1_000);
    assert_eq!(clock.now_ms(), 1_000);
    clock.sleep_ms(250);
    clock.sleep_ms(250);
    assert_eq!(clock.now_ms(), 1_500);
}

/// Asserts that the system clock reads a time after 2020 in milliseconds and that a wait
/// takes at least as long as asked.
#[test]
fn the_system_clock_reads_epoch_milliseconds_and_waits() {
    let clock = SystemClock;
    assert!(clock.now_ms() > 1_577_836_800_000);
    let start = Instant::now();
    clock.sleep_ms(5);
    assert!(start.elapsed().as_millis() >= 5);
}
