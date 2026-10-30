//! The self-test as the pool user, with root-owned canaries: what the runner tests cannot
//! show, because there the canaries belong to the user who runs the probes.
//!
//! Every test is `#[ignore]`d: they run only on a provisioned Mac, as root, through the
//! `js-mac-pool-privileged-tests` workflow's binary. They read these variables (a
//! flag-free test binary has no other way to take them):
//!
//! - `JS_MAC_POOL_PRIV_ROOT`: a directory to work in, owned by root, not writable by
//!   others, not a link, on the boot volume's data volume (not under `/Volumes`: the
//!   data-volume canary is spelled through `/System/Volumes/Data`);
//! - `JS_MAC_POOL_PRIV_UID` and `JS_MAC_POOL_PRIV_GID`: an unprivileged user and group
//!   (neither 0).
//!
//! Run as root: `./privileged --ignored --test-threads=1`.

#![cfg(target_os = "macos")]

mod common;

use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;

use common::mac::{Mac, describe, outcomes, run_steps};
use common::{Scratch, run};
use js_mac_pool::seatbelt::{Class, MacosMajor, Role, SelfTest};

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn ids() -> (u32, u32) {
    let uid: u32 = var("JS_MAC_POOL_PRIV_UID").parse().expect("a uid");
    let gid: u32 = var("JS_MAC_POOL_PRIV_GID").parse().expect("a gid");
    assert!(uid != 0, "the pool user is not root");
    assert!(gid != 0, "the pool group is not root");
    (uid, gid)
}

/// The fixture below the work root, with the run directory and the user's folder entry
/// handed to the pool user and everything else left to root.
fn fixture(tag: &str) -> Mac {
    let (uid, gid) = ids();
    let root = PathBuf::from(var("JS_MAC_POOL_PRIV_ROOT"));
    let meta = std::fs::symlink_metadata(&root).expect("the work root");
    assert!(meta.is_dir(), "the work root is a directory, not a link");
    assert_eq!(meta.uid(), 0, "the work root belongs to root");
    assert_eq!(meta.mode() & 0o022, 0, "nobody else writes the work root");
    let scratch = Scratch::under(&root, tag);
    let entry = scratch.dir("uid-entry");
    let mac = Mac::build(scratch, entry.clone(), tag, 0);
    for dir in [&mac.run, &entry] {
        std::os::unix::fs::chown(dir, Some(uid), Some(gid)).expect("chown");
    }
    mac
}

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
