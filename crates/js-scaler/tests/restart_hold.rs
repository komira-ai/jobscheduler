//! What a scaler does with a request a restored machine would serve, before the machine's
//! agent has said hello to the registry of the new process. The registry has no live session
//! for the machine then, so the host does not cover the request; the machine's stored offer
//! does, until the host says hello or `boot_timeout` has run from the restart.

mod common;

use common::*;
use js_scaler::{HostFacts, HostState, Phase, Records, State};
use js_types::Resources;

/// A scaler that has just started from records holding one registered machine, `js-made-0-60`
/// (an a10 machine), with one request waiting that its offer fits and no host shown.
fn restarted() -> Rig {
    let mut rig = Rig::new(settings("gpu"));
    rig.state = State::new(Records {
        instances: vec![record(Phase::Registered, 60, 0)],
        ..Records::default()
    });
    rig.input.waiting = vec![request("r1")];
    rig
}

/// The host of the machine of [`restarted`], live.
fn hello() -> HostFacts {
    host("js-made-0-60", "gpu")
}

/// Asserts that a request the restored machine's offer fits launches nothing while the
/// machine's host is not shown, however many steps pass inside the window. It catches a view
/// that counts the machine as covering nothing until its agent says hello, which launches a
/// second machine for the same request.
#[test]
fn a_request_a_restored_machine_fits_launches_nothing_before_its_hello() {
    let mut rig = restarted();
    assert!(acquires(&rig.tick(at(0))).is_empty());
    assert!(acquires(&rig.tick(at(5 * MINUTE))).is_empty());
    assert!(acquires(&rig.tick(at(15 * MINUTE - 1))).is_empty());
    assert_eq!(rig.state.records.instances.len(), 1);
}

/// Asserts that the same holds for a host the registry shows but does not hear from
/// (waiting to say hello again, with no shape): the stored offer covers the request.
#[test]
fn an_awaiting_host_does_not_make_the_stored_offer_stop_covering() {
    let mut rig = restarted();
    let mut silent = hello();
    silent.state = HostState::Awaiting;
    silent.shape = None;
    rig.input.hosts = vec![silent];
    assert!(acquires(&rig.tick(at(0))).is_empty());
    assert!(acquires(&rig.tick(at(15 * MINUTE - 1))).is_empty());
}

/// Asserts that the hold ends at `boot_timeout` from the first step after the restart, to
/// the millisecond: one before it nothing is launched, at it the request is launched for
/// and the restored machine is still held (the lost-machine rules wait for `unknown_hold`).
#[test]
fn the_hold_ends_at_boot_timeout_from_the_restart() {
    let mut rig = restarted();
    let back = 40 * MINUTE;
    assert!(acquires(&rig.tick(at(back))).is_empty());
    assert!(acquires(&rig.tick(at(back + 15 * MINUTE - 1))).is_empty());
    let launched = acquires(&rig.tick(at(back + 15 * MINUTE)));
    assert_eq!(launched.len(), 1);
    assert_eq!(rig.state.records.instances.len(), 2);
    assert_eq!(rig.phase(0), Phase::Registered);
    // The launched machine now covers the request: later steps launch no more.
    assert!(acquires(&rig.tick(at(back + 16 * MINUTE))).is_empty());
}

/// Asserts that the machine's hello ends the hold: the live host covers the request itself
/// and nothing is launched; if the host then goes silent again the stored offer does not
/// cover once more (the hold is for the first hello only), so a request is launched for.
#[test]
fn a_hello_ends_the_hold_for_good() {
    let mut rig = restarted();
    assert!(acquires(&rig.tick(at(0))).is_empty());
    rig.input.hosts = vec![hello()];
    assert!(acquires(&rig.tick(at(MINUTE))).is_empty());

    rig.input.hosts[0].state = HostState::Lost;
    assert_eq!(acquires(&rig.tick(at(2 * MINUTE))).len(), 1);
}

/// Asserts that a request the restored machine's offer does not fit is launched for at
/// once: the hold is scoped by fit. It catches a hold that stops the whole pool.
#[test]
fn a_request_the_restored_machine_does_not_fit_is_launched_for_at_once() {
    let mut rig = restarted();
    let stored = &mut rig.state.records.instances[0].offer;
    stored.gpus = 0;
    stored.gpu_class = None;
    assert_eq!(acquires(&rig.tick(at(0))).len(), 1);
}

/// Asserts that one restored machine covers one request: with two requests it fits, the
/// second is launched for. It catches a machine that covers every request.
#[test]
fn a_restored_machine_covers_one_request() {
    let mut rig = restarted();
    rig.input.waiting = vec![request("r1"), request("r2")];
    assert_eq!(acquires(&rig.tick(at(0))).len(), 1);
}

/// Asserts that a condemned machine covers nothing, and that a machine that has left the
/// registered phases (released because the provider no longer has it) stops covering.
#[test]
fn a_condemned_or_released_machine_covers_nothing() {
    let mut rig = restarted();
    rig.state.records.instances[0].condemned = true;
    assert_eq!(acquires(&rig.tick(at(0))).len(), 1);

    let mut rig = restarted();
    assert!(acquires(&rig.tick(at(0))).is_empty());
    // The provider says the machine is gone: it is gone.
    rig.input.not_found = vec!["js-made-0-60".to_owned()];
    let actions = rig.tick(at(MINUTE));
    assert_eq!(rig.phase(0), Phase::Gone);
    assert_eq!(acquires(&actions).len(), 1);
}

/// Asserts that a scaler that did not start from records is unchanged: a registered
/// machine whose host is not shown covers no request, so one is launched for.
#[test]
fn without_a_restart_a_machine_with_no_host_covers_nothing() {
    let mut rig = Rig::new(settings("gpu"));
    rig.state.records.instances = vec![record(Phase::Registered, 60, 0)];
    rig.input.waiting = vec![request("r1")];
    assert_eq!(acquires(&rig.tick(at(0))).len(), 1);
}

/// Asserts that a restored machine of another pool covers nothing in this one.
#[test]
fn a_restored_machine_of_another_pool_does_not_hold_this_one() {
    let mut rig = restarted();
    rig.state.records.instances[0].pool = "other".to_owned();
    assert_eq!(acquires(&rig.tick(at(0))).len(), 1);
}

/// Asserts that a restored machine that is draining holds nothing: the step had decided to
/// take it out and the registry places no new lease on it, so the request is launched for
/// on the first step rather than after the window. It catches a hold that awaits draining
/// machines, which delays the launch by `boot_timeout` for no saving.
#[test]
fn a_restored_draining_machine_does_not_hold_a_request_it_fits() {
    let mut rig = restarted();
    rig.state.records.instances[0].phase = Phase::Draining;
    assert_eq!(acquires(&rig.tick(at(0))).len(), 1);
}

/// Asserts that a restored machine whose offer fits only part of the request covers
/// nothing: enough cpu but too little memory, and enough of everything but too few gpus.
/// It catches a cover that checks one dimension of the offer.
#[test]
fn a_restored_machine_that_fits_part_of_a_request_does_not_hold_it() {
    let mut rig = restarted();
    // The request wants 16 GiB; the machine's offer has 8000 millis of cpu (enough, the
    // request wants 4000) and 8 GiB.
    rig.state.records.instances[0].offer.memory_bytes = 8 << 30;
    assert_eq!(acquires(&rig.tick(at(0))).len(), 1);

    let mut rig = restarted();
    // The request wants two a10s; the machine's offer has one. The catalog has a two-gpu
    // offer, which is what the launch asks for.
    let mut pair = offer("a10x2", 120);
    pair.gpus = 2;
    rig.pool().offers.push(pair);
    rig.input.waiting[0].demand.resources =
        Resources::new(4000, 16 << 30, 2, Some("nvidia-a10".to_owned())).unwrap();
    assert_eq!(acquires(&rig.tick(at(0))).len(), 1);
}

/// Asserts that a machine that has said hello and is then marked condemned is not counted
/// twice: the hello took it out of the awaited machines, the condemned host covers nothing,
/// so the request is launched for once, and the launched machine covers it after. It
/// catches an awaited entry that outlives the hello and counts the machine again beside
/// the host.
#[test]
fn a_machine_that_said_hello_and_is_then_condemned_is_not_counted_twice() {
    let mut rig = restarted();
    assert!(acquires(&rig.tick(at(0))).is_empty());
    rig.input.hosts = vec![hello()];
    assert!(acquires(&rig.tick(at(MINUTE))).is_empty());

    rig.state.records.instances[0].condemned = true;
    assert_eq!(acquires(&rig.tick(at(2 * MINUTE))).len(), 1);
    assert!(acquires(&rig.tick(at(3 * MINUTE))).is_empty());
}

/// Asserts that a pool missing from one step's input does not end the hold: the window is
/// not run out by the absence, nothing is covered while the pool is absent, and when the
/// pool is back the machine still holds the request until `boot_timeout` from the restart.
/// It catches an entry dropped because its pool had no settings to read the window from.
#[test]
fn a_pool_absent_for_a_step_does_not_end_the_hold() {
    let mut rig = restarted();
    assert!(acquires(&rig.tick(at(0))).is_empty());

    let pool = rig.input.pools.remove(0);
    assert!(acquires(&rig.tick(at(MINUTE))).is_empty());

    rig.input.pools = vec![pool];
    assert!(acquires(&rig.tick(at(2 * MINUTE))).is_empty());
    assert!(acquires(&rig.tick(at(15 * MINUTE - 1))).is_empty());
    assert_eq!(acquires(&rig.tick(at(15 * MINUTE))).len(), 1);
}
