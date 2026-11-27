//! The scaler against the real server: what a panic in its tick does, and what a leap of
//! the clock does to the registry's view. Time is the test's clock and the ticks are
//! stepped; nothing sleeps and no network is touched.

use std::time::Duration;

use crate::scaler_support::{account, panic_on_list, start, text};
use crate::support::{Harness, request};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Asserts what a panic in the scaler's tick does: its task ends, `/healthz` answers 503
/// so that the supervisor of the process restarts the server, and the table goes on
/// ticking, each tick finished, until then. It catches a scaler that dies without anyone
/// being told, and a table held up by a scaler that is gone.
#[tokio::test]
async fn a_panic_in_the_scaler_ends_its_ticks_and_health_says_so() {
    let scratch = crate::support::Scratch::new("scaler_panics");
    account(&scratch);
    panic_on_list(&scratch);
    let source = text(&scratch, "live", "initialise");
    let h = start(scratch, &source).await;
    let before = request(h.addr(), "GET", "/healthz", None, None).await;
    assert_eq!(before.status, 200, "{}", before.body);
    h.ticks.step().await;
    h.ticks.step().await;
    h.ticks.step().await;
    let after = request(h.addr(), "GET", "/healthz", None, None).await;
    assert_eq!(after.status, 503, "{}", after.body);
}

/// Asserts that the server, and not only the scaler, caps what a leap of the wall clock
/// adds to the time the registry's view has been quiet: the clock jumps an hour right
/// after the listeners are bound, the scaler ticks, and the view is still not complete,
/// because one observation counts at most twice the tick interval. The scaler has no host
/// to wait for, so only the floor holds the view open. It catches the server handing the
/// scaler no cap (or none at all), which lets one tick after an NTP or RTC correction at
/// boot vouch for a registry that is empty because the agents have not yet said hello.
#[tokio::test]
async fn a_leap_of_the_clock_after_start_does_not_complete_the_registry_view() {
    let scratch = crate::support::Scratch::new("scaler_leap");
    account(&scratch);
    let source = text(&scratch, "live", "initialise");
    let h = start(scratch, &source).await;
    let complete = |h: &Harness| h.running.scaler().unwrap().registry_complete();
    h.clock.advance(secs(3600));
    h.ticks.step().await;
    assert!(!complete(&h));
    assert_eq!(h.running.scaler().unwrap().save_errors(), 0);
    // The ticks do observe the clock: with an hour between ticks the view completes once
    // enough ticks have each counted the cap, which is many more than the leap's one.
    let mut ticks = 1;
    while !complete(&h) {
        assert!(ticks < 500, "the view never completed");
        h.clock.advance(secs(3600));
        h.ticks.step().await;
        ticks += 1;
    }
    assert!(ticks > 2, "completed after {ticks} ticks");
}
