//! The first update of a host the package installed: the crun tmpfiles file makes the release
//! link pointing at `/usr/bin`, the directory of the package's agent, so the previous release
//! the updater names is that directory and not a release of `releases/`. The steps take it as
//! any other previous release: the cutover moves the link to the staged release, and a failed
//! `verify` is rolled back to the package's agent.

use js_agent::update_hook::Outcome;

use super::rollback_entries::{
    assert_restored, link_target, previous, staged, verified_on, verify_failed_on,
};
use super::step_rig::{Rig, Spy, running};

/// The previous release of this rig is the tree's `usr/bin`, outside the releases directory.
fn from_package(name: &str) -> Rig {
    let rig = Rig::from_package(name);
    assert!(previous(&rig).ends_with("usr/bin"), "{:?}", previous(&rig));
    assert_eq!(link_target(&rig), Some(previous(&rig)));
    rig
}

#[test]
fn an_update_from_the_packages_agent_moves_the_link_to_the_staged_release() {
    let rig = verified_on(running(from_package("hook-package-update")));
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    let done = Spy::new(&rig).cleanup(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    assert_eq!(rig.record(), None);
}

#[test]
fn a_failed_verify_after_an_update_from_the_packages_agent_rolls_back_to_it() {
    let rig = verify_failed_on(running(from_package("hook-package-rollback")));
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    let done = Spy::new(&rig).rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    // The link names the package's directory again, and the agent of 1.3.0 runs from it.
    assert_restored(&rig);
}
