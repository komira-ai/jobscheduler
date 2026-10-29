//! The release of a silent machine: not while its host shows work, and not left for good
//! when the scaler had already marked the host for release.

mod common;

use common::*;
use js_scaler::{HostState, Mark};

/// A rig whose registered host is silent, with the registry's lease flag set, and which
/// is then changed by `change`.
fn silent_given_up(change: impl FnOnce(&mut js_scaler::HostFacts)) -> Rig {
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

/// Asserts that a silent host the registry shows marked for release, which books nothing,
/// is released once silent for `unknown_hold` although no lease was ever given up.
#[test]
fn a_silent_host_marked_releasing_is_released() {
    let mut rig = silent_given_up(|host| {
        host.mark = Some(Mark::Releasing);
        host.lease_given_up = false;
    });
    let mut released = 0;
    for later in [3 * SECOND, 3 * SECOND + 30 * MINUTE, 2 * HOUR, 2 * DAY] {
        released += count(&rig.tick(at(later)), "Release");
    }
    assert_eq!(released, 1);
}
