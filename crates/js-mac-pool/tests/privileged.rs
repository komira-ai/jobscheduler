//! The self-test as the pool user, with root-owned canaries: what the runner tests cannot
//! show, because there the canaries belong to the user who runs the probes.
//!
//! Every test is `#[ignore]`d: they run only on a provisioned Mac, as root, through the
//! `js-mac-pool-privileged-tests` workflow's binary. They read these variables (a
//! flag-free test binary has no other way to take them):
//!
//! - the variables of [`common::priv_root`] (the work root, the invoking user's uid, the
//!   pool user and group).
//!
//! Run as root: `./privileged --ignored --test-threads=1`.

#![cfg(target_os = "macos")]

mod common;

use common::mac::{describe, outcomes, run_steps};
use common::priv_root::{fixture, ids};
use common::run;
use js_mac_pool::seatbelt::{Class, MacosMajor, Role, SelfTest};

#[test]
#[ignore = "needs root on a provisioned Mac (js-mac-pool-privileged-tests)"]
fn the_self_test_passes_as_the_pool_user_against_root_owned_canaries() {
    let ids = ids();
    let mac = fixture("priv-selftest");
    let test =
        SelfTest::new(MacosMajor::Macos14, mac.fixture(MacosMajor::Macos14.scan())).expect("steps");
    let ran = run_steps(&test, Some(ids));
    let results = outcomes(&ran);
    if let Err(failures) = test.judge(&results) {
        let shown: Vec<String> = failures.iter().map(ToString::to_string).collect();
        panic!("{}\n{}", shown.join("\n"), describe(&test, &ran));
    }
}

/// The pool user cannot read the canaries by permission alone being absent: the control
/// (the same read without the profile, as the same user) must work, and it does only if
/// the canaries are world-readable. This pins that the self-test's denials come from the
/// profile and not from the files' modes.
#[test]
#[ignore = "needs root on a provisioned Mac (js-mac-pool-privileged-tests)"]
fn the_canaries_are_readable_by_the_pool_user_without_the_profile() {
    let ids = ids();
    let mac = fixture("priv-controls");
    let test =
        SelfTest::new(MacosMajor::Macos14, mac.fixture(MacosMajor::Macos14.scan())).expect("steps");
    let controls: Vec<Class> = test
        .steps()
        .iter()
        .filter(|step| step.role() == Role::Control)
        .map(|step| step.class())
        .collect();
    assert_eq!(controls.len(), 15);
    for step in test
        .steps()
        .iter()
        .filter(|step| step.role() == Role::Control)
    {
        let ran = run(&step.argv(), step.env(), Some(step.cwd()), Some(ids));
        assert!(ran.ok, "{}: {}", step.class(), ran.stderr);
    }
}
