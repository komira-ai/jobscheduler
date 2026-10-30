//! A restarted shell over records that hold a machine, with a request waiting that the
//! machine would serve. The registry has no live session for the machine until its agent
//! says hello again, so the view does not count it; the step still counts it as covering a
//! request its stored offer fits, for `boot_timeout` from the restart or its hello.

use std::time::Duration;

use js_registry::HostState;
use js_scaler::Phase;

use crate::fakes::{Rig, live, record, records, request, rig};

/// A rig just restarted over the records of one registered machine, `js-restored`, with
/// the registry showing no host and one request waiting that the machine's offer fits.
fn restarted() -> Rig {
    let rig = rig(
        records(vec![record("js-restored", Phase::Registered)]),
        live(),
    );
    rig.world().waiting = vec![request("r1")];
    rig
}

/// Whether the log holds a launch (a mint or an acquire).
fn launched(rig: &Rig) -> bool {
    rig.log()
        .iter()
        .any(|entry| entry.starts_with("mint:") || entry.starts_with("acquire:"))
}

/// Asserts that no machine is launched for a request the restored machine fits, on the
/// first tick or the ticks after, while the registry has not heard it; and that when it
/// says hello the live host covers the request and still nothing is launched. It catches a
/// view that counts a restored machine as covering nothing until its hello, which launches
/// a second machine for the same request.
#[tokio::test]
async fn nothing_is_launched_for_a_request_a_restored_machine_fits_until_it_says_hello() {
    let rig = restarted();
    rig.scaler.tick().await;
    rig.advance(Duration::from_secs(60));
    rig.scaler.tick().await;
    assert!(!launched(&rig), "{:?}", rig.log());
    assert_eq!(rig.scaler.state().records.instances.len(), 1);

    rig.world().hosts = vec![crate::fakes::view("js-restored")];
    rig.advance(Duration::from_secs(60));
    rig.scaler.tick().await;
    assert!(!launched(&rig), "{:?}", rig.log());
    assert_eq!(rig.scaler.state().records.instances.len(), 1);
}

/// Asserts that a machine that never says hello holds the launch for `boot_timeout` from
/// the restart, to the millisecond: nothing is launched 1 ms before, a machine is launched
/// at it, and the restored machine is still held as registered (the lost-machine rules
/// apply to it, and they wait for `unknown_hold`). It catches a hold with no end, and one
/// that ends early.
#[tokio::test]
async fn a_machine_that_never_says_hello_holds_the_launch_until_boot_timeout() {
    let rig = restarted();
    let boot_timeout = live().boot_timeout;
    rig.scaler.tick().await;
    rig.advance(boot_timeout - Duration::from_millis(1));
    rig.scaler.tick().await;
    assert!(!launched(&rig), "{:?}", rig.log());

    rig.advance(Duration::from_millis(1));
    rig.scaler.tick().await;
    assert!(launched(&rig), "{:?}", rig.log());
    let state = rig.scaler.state();
    assert_eq!(state.records.instances.len(), 2);
    assert_eq!(state.records.instances[0].phase, Phase::Registered);
}

/// Asserts that the hold is scoped by fit: a request the restored machine's offer does not
/// fit (it has no gpu) is launched for on the first tick, while a request it fits waits.
/// It catches a hold that stops the whole pool.
#[tokio::test]
async fn a_request_the_restored_machine_does_not_fit_is_launched_for_at_once() {
    let mut machine = record("js-restored", Phase::Registered);
    machine.offer.gpus = 0;
    machine.offer.gpu_class = None;
    let rig = rig(records(vec![machine]), live());
    rig.world().waiting = vec![request("r1")];
    rig.scaler.tick().await;
    assert!(launched(&rig), "{:?}", rig.log());
    assert_eq!(rig.scaler.state().records.instances.len(), 2);
}

/// Asserts that a restored machine covers one request, not all: with two requests it fits,
/// one machine is launched for the other. It catches a pool-wide hold.
#[tokio::test]
async fn a_restored_machine_holds_the_launch_for_one_request_only() {
    let rig = restarted();
    rig.world().waiting = vec![request("r1"), request("r2")];
    rig.scaler.tick().await;
    let acquires = rig
        .log()
        .iter()
        .filter(|entry| entry.starts_with("acquire:"))
        .count();
    assert_eq!(acquires, 1, "{:?}", rig.log());
}

/// Asserts that a machine whose host the registry shows silent (waiting to say hello
/// again, holding a lease) covers a request its offer fits the same way. It catches a shell
/// that holds only for a machine the registry does not show.
#[tokio::test]
async fn an_awaiting_host_holds_the_launch_the_same_way() {
    let rig = restarted();
    let mut awaiting = crate::fakes::view("js-restored");
    awaiting.state = HostState::Awaiting;
    awaiting.claim = None;
    awaiting.lease = Some(js_types::LeaseId::new(1, 1));
    rig.world().hosts = vec![awaiting];
    rig.scaler.tick().await;
    assert!(!launched(&rig), "{:?}", rig.log());
}

/// Asserts that with no restored records a waiting request is launched for on the first
/// tick, as before the hold. It catches a hold that fires without a restored machine.
#[tokio::test]
async fn without_restored_records_a_waiting_request_is_launched_for() {
    let rig = rig(js_scaler::Records::default(), live());
    rig.world().waiting = vec![request("r1")];
    rig.scaler.tick().await;
    assert!(launched(&rig), "{:?}", rig.log());
    assert_eq!(rig.scaler.state().records.instances.len(), 1);
}
