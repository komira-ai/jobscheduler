//! What the scaler never releases: a machine whose host may be holding work, and, among the
//! silent ones, only the machines whose host books nothing.

mod common;

use common::*;
use js_scaler::{HostFacts, HostState, Mark, Phase};

/// Asserts that a booting machine is not revoked and released at `boot_timeout` when the
/// registry's view is incomplete: its host may have said hello and be holding work.
#[test]
fn a_booting_machine_is_held_by_the_boot_timeout_on_an_incomplete_view() {
    let (mut rig, _plan, _) = booting(settings("gpu"), "i-1");
    rig.input.registry_complete = false;
    let actions = rig.tick(at(15 * MINUTE));
    assert_eq!(
        count(&actions, "Release") + count(&actions, "Revoke"),
        0,
        "{:?}",
        kinds(&actions)
    );
    assert_eq!(rig.phase(0), Phase::Booting);
}

/// Asserts that the boot timeout does give up on a machine that has said nothing once the
/// registry's view is complete again, at the next step: it is revoked and released, and the
/// pool cools off.
#[test]
fn a_booting_machine_is_given_up_once_the_view_is_complete() {
    let (mut rig, _plan, _) = booting(settings("gpu"), "i-1");
    rig.input.registry_complete = false;
    rig.tick(at(15 * MINUTE));
    rig.input.registry_complete = true;
    let actions = rig.tick(at(15 * MINUTE + SECOND));
    assert_eq!(count(&actions, "Revoke"), 1);
    assert_eq!(count(&actions, "Release"), 1);
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert_eq!(rig.state.records.cooldowns.len(), 1);
}

/// A rig whose registered host is silent, with the registry's lease flag set, and which is
/// then changed by `change`.
fn silent_given_up(change: impl FnOnce(&mut HostFacts)) -> Rig {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].state = HostState::Lost;
    rig.input.hosts[0].lease_given_up = true;
    change(&mut rig.input.hosts[0]);
    rig
}

/// Asserts that a silent host whose snapshot shows a lease, a run or a stream is not
/// released, whatever the lease flag says.
#[test]
fn a_silent_host_that_shows_work_is_not_released() {
    let mut rig = silent_given_up(|host| host.busy = true);
    for later in [3 * SECOND, 3 * SECOND + 30 * MINUTE, 2 * HOUR] {
        let actions = rig.tick(at(later));
        assert_eq!(count(&actions, "Release") + count(&actions, "Revoke"), 0);
    }
}

/// Asserts that a silent host with a whole-node lease committed on it is not released.
#[test]
fn a_silent_host_marked_held_is_not_released() {
    let mut rig = silent_given_up(|host| host.mark = Some(Mark::Held));
    for later in [3 * SECOND, 3 * SECOND + 30 * MINUTE, 2 * HOUR] {
        let actions = rig.tick(at(later));
        assert_eq!(count(&actions, "Release") + count(&actions, "Revoke"), 0);
    }
}

/// Asserts that a silent host set aside for a waiting request is not released, drained or
/// marked either.
#[test]
fn a_silent_host_marked_reserved_is_left_alone() {
    let mut rig = silent_given_up(|host| host.mark = Some(Mark::Reserved));
    for later in [3 * SECOND, 3 * SECOND + 30 * MINUTE, 2 * HOUR] {
        assert!(rig.tick(at(later)).is_empty());
    }
    assert_eq!(rig.phase(0), Phase::Registered);
}

/// Asserts that a silent host the registry shows marked for release, which books nothing,
/// is released once silent for `unknown_hold` although no lease was ever given up.
#[test]
fn a_silent_host_marked_releasing_is_released() {
    let mut rig = silent_given_up(|host| {
        host.mark = Some(Mark::Releasing);
        host.lease_given_up = false;
    });
    assert_eq!(count(&rig.tick(at(3 * SECOND)), "Release"), 0);
    assert_eq!(
        count(&rig.tick(at(3 * SECOND + 30 * MINUTE - 1)), "Release"),
        0
    );
    let actions = rig.tick(at(3 * SECOND + 30 * MINUTE));
    assert_eq!(count(&actions, "Revoke"), 1);
    assert_eq!(count(&actions, "Release"), 1);
    assert_eq!(rig.phase(0), Phase::Releasing);
}
