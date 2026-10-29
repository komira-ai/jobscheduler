//! The scaler against the real registry: a host the registry would not remove is out once
//! its machine is gone. Time is the test's clock and the ticks are
//! stepped; nothing sleeps and no network is touched.

use std::time::Duration;

use js_provider::Provider;
use js_scaler::Phase;

use crate::scaler_support::{
    account, host_id_in, join, panic_on_list, spec, start, text, token_in,
};
use crate::support::{Harness, request};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Asserts that the host of a machine the provider has lost is out of the registry, which
/// refuses to remove a host that is not marked releasing: a registered machine is deleted
/// at the provider, its host stops beating and passes the registry's grace, and the scaler
/// reads the machine gone by id. The record is gone and the registry holds no host, so its
/// slot is free for the next launch. It catches a removal sent once and refused, which
/// keeps the dead host's slot for a day and makes every replacement launch fail.
#[tokio::test]
async fn the_host_of_a_machine_that_is_gone_leaves_the_registry() {
    let scratch = crate::support::Scratch::new("scaler_gone_host");
    let account = account(&scratch);
    let source = text(&scratch, "live", "initialise");
    let h = start(scratch, &source).await;
    h.ticks.step().await;
    let registry = h.running.registry().unwrap();
    registry.provider().start(spec(1, "r1"));
    h.ticks.step().await;
    let script = account.launches()[0].user_data().unwrap().to_owned();
    let name = host_id_in(&script);
    h.ticks.step().await;
    join(&h, &token_in(&script), &name).await;
    h.ticks.step().await;
    let phase = |h: &Harness| h.running.scaler().unwrap().state().records.instances[0].phase;
    assert_eq!(phase(&h), Phase::Registered);
    assert_eq!(registry.hosts().len(), 1);

    let machine = account.instances()[0].id.clone();
    account.remove_instance(&machine);
    for _ in 0..6 {
        h.clock.advance(secs(20));
        h.ticks.step().await;
    }
    assert_eq!(phase(&h), Phase::Gone);
    assert!(registry.hosts().is_empty(), "{:?}", registry.hosts());
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
