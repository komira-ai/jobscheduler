//! A machine whose host may have said hello is not released by the boot timeout when the
//! registry's view cannot be vouched for.

mod common;

use common::*;
use js_scaler::Phase;

/// Asserts that a booting machine is not revoked and released at `boot_timeout` when the
/// registry's view is incomplete: the host may have said hello and be holding work.
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
