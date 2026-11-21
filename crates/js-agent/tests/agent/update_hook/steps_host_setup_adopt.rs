//! A Mac that an earlier release's host setup left half done, and calls the system refuses: the
//! group, the roots and the unmounted scratch volume that are there are used and not made
//! again, the leftover copy of the file system table is removed, and a call the system refuses
//! (`EPERM`) fails the step (1, `host_setup_refused`) naming the path and the error, where any
//! other error defers it.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use js_agent::update_hook::step::{DISKUTIL, DSEDITGROUP};
use js_agent::update_hook::{Class, Done, Outcome, Reason};

use super::fake_mac::{DEVICE, EPERM, FakeMac, GROUP, Volume, mac_rig, scratch_of, spy_on};
use super::step_rig::MAC_UNIT;
use super::steps_host_setup::{assert_missing, assert_no_table, leave_leftover};

/// Asserts the failure of a call the system refused: exit 1, `host_setup_refused`, an
/// environment matter, no retry hint, and exactly `detail`.
fn assert_refused(done: &Done, detail: &str) {
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::HostSetupRefused));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(done.report.retry_after_secs, None);
    assert_eq!(done.report.detail.as_deref(), Some(detail));
}

/// Asserts how the setup ends for a path whose call failed with `why`: failed when the system
/// refused it (`EPERM`), deferred otherwise, naming the path and the error either way.
fn assert_ending(done: &Done, path: &Path, why: &io::Error) {
    let detail = format!("{}: {why}", path.display());
    if why.raw_os_error() == Some(EPERM) {
        assert_refused(done, &detail);
    } else {
        assert_missing(done, &detail);
    }
}

/// Asserts that a Mac as a release whose table write macOS refused left it (the group there with
/// a gid of macOS's, both roots there and private, the scratch volume made, unmounted and
/// ignoring ownership, and the copy of the table beside it) ends set up: the volume is mounted
/// on the scratch root and given ownership, nothing else is made, no table is written and the
/// leftover is gone. Catches a setup that makes a second volume or group, or that writes the
/// table again.
#[test]
fn a_mac_an_earlier_setup_left_half_done_is_adopted_and_its_leftover_removed() {
    let rig = mac_rig("hook-setup-adopt", false);
    let mac = FakeMac::new();
    mac.with_group(GROUP, 502);
    mac.with_volume(Volume {
        mount: None,
        owners: false,
    });
    let scratch = scratch_of(&rig);
    for root in [&rig.params.state_root, &scratch] {
        fs::create_dir_all(root).unwrap();
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let spy = spy_on(&rig, &mac);
    leave_leftover(&spy);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let shown = scratch.display();
    assert_eq!(
        mac.changes(),
        [
            format!("{DISKUTIL} mount nobrowse -mountPoint {shown} {DEVICE}"),
            format!("{DISKUTIL} enableOwnership {shown}"),
        ]
    );
    assert_eq!(mac.groups.borrow().get(GROUP), Some(&502));
    assert_eq!(
        *mac.volume.borrow(),
        Some(Volume {
            mount: Some(scratch.clone()),
            owners: true,
        })
    );
    assert!(!spy.fstab_leftover.exists());
    assert_eq!(
        fs::read_dir(spy.fstab_leftover.parent().unwrap())
            .unwrap()
            .count(),
        0
    );
    assert_no_table(&rig);
    assert!(rig.record().is_some());
}

/// Asserts that a look the system refuses (`EPERM`) fails the first install naming the path and
/// the error, with no record written and nothing made after it; and that on an update it fails
/// before the host is cordoned. Catches a refusal that is deferred and retried for ever.
#[test]
fn a_call_the_system_refuses_fails_naming_the_path_and_the_error() {
    let rig = mac_rig("hook-setup-refused", false);
    let mac = FakeMac::new();
    let scratch = scratch_of(&rig);
    mac.look_refused.borrow_mut().insert(scratch.clone());
    let done = spy_on(&rig, &mac).prepare(&rig);
    let detail = format!(
        "{}: Operation not permitted (os error 1)",
        scratch.display()
    );
    assert_refused(&done, &detail);
    assert!(rig.record().is_none());
    assert_eq!(mac.changes(), [format!("{DSEDITGROUP} -o create {GROUP}")]);

    let rig = mac_rig("hook-setup-refused-update", true);
    rig.world.seed_unit(MAC_UNIT, "active", "running");
    let mac = FakeMac::new();
    let state = rig.params.state_root.clone();
    mac.look_refused.borrow_mut().insert(state.clone());
    let spy = spy_on(&rig, &mac);
    let done = spy.prepare(&rig);
    let detail = format!("{}: Operation not permitted (os error 1)", state.display());
    assert_refused(&done, &detail);
    assert!(spy.log().iter().all(|call| !call.starts_with("cordon(")));
    assert_eq!(rig.cordon_expiry(), None);
    assert!(rig.record().is_none());
}

/// Asserts that a leftover that cannot be looked at (a file where its directory should be)
/// defers naming it, and that one that cannot be removed (a directory with an entry) ends as
/// its error says: failed for a refusal, deferred otherwise. Either way the volume is mounted
/// first and no record is written.
#[test]
fn a_leftover_that_cannot_be_looked_at_or_removed_ends_naming_it() {
    let rig = mac_rig("hook-setup-leftover-unlooked", false);
    let mac = FakeMac::new();
    let spy = spy_on(&rig, &mac);
    let parent = spy.fstab_leftover.parent().unwrap();
    fs::write(parent, "not a directory").unwrap();
    let why = fs::symlink_metadata(&spy.fstab_leftover).unwrap_err();
    assert_ne!(why.kind(), io::ErrorKind::NotFound);
    let done = spy.prepare(&rig);
    assert_ending(&done, &spy.fstab_leftover, &why);
    assert_eq!(
        mac.volume.borrow().as_ref().unwrap().mount,
        Some(scratch_of(&rig))
    );
    assert!(rig.record().is_none());

    let rig = mac_rig("hook-setup-leftover-kept", false);
    let mac = FakeMac::new();
    let spy = spy_on(&rig, &mac);
    let twin = rig.tmp.path().join("twin");
    for dir in [&spy.fstab_leftover, &twin] {
        fs::create_dir_all(dir.join("entry")).unwrap();
    }
    let why = fs::remove_file(&twin).unwrap_err();
    let done = spy.prepare(&rig);
    assert_ending(&done, &spy.fstab_leftover, &why);
    assert!(spy.fstab_leftover.is_dir());
    assert!(rig.record().is_none());
}
