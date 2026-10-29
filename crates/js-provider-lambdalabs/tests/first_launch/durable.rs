//! The state file's directory is synced after the file is created and after the rename.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use js_elastic::HostStatus;
use js_provider_lambdalabs::Call;

use crate::rig::{FIRST, Rig};

#[test]
fn the_directory_is_synced_after_the_create_and_after_the_rename() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    let seen = std::cell::RefCell::new(Vec::new());
    let (status, out) = rig.exec_with(&args, &|n| {
        if n == 1 {
            // The first poll pause is after both writes.
            seen.borrow_mut().push(rig.synced.borrow().len());
            crate::rig::activate(&rig.fake);
        }
    });
    assert_eq!(status, 0, "{out}");
    assert_eq!(seen.into_inner(), [2]);
    assert_eq!(*rig.synced.borrow(), [rig.dir.clone(), rig.dir.clone()]);
}

#[test]
fn a_bare_file_name_syncs_the_current_directory() {
    let mut rig = Rig::new();
    rig.state = PathBuf::from("first-launch-durable-test-state");
    let args = rig.go_args(&[]);
    let (status, out) = rig.exec_active(&args, 1);
    let _ = fs::remove_file(&rig.state);
    assert_eq!(status, 0, "{out}");
    assert_eq!(
        *rig.synced.borrow(),
        [PathBuf::from("."), PathBuf::from(".")]
    );
}

#[test]
fn a_failed_sync_after_the_rename_tears_the_instance_down_at_once() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.sync_fails_at.set(2);
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("teardown forced: the state file could not be updated"),
        "{out}"
    );
    assert_eq!(rig.status_of(FIRST), HostStatus::Terminating);
    assert!(rig.sleeps.borrow().is_empty(), "no wait happened");
}

#[test]
fn a_failed_sync_after_the_create_launches_nothing() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.sync_fails_at.set(1);
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 1, "{out}");
    assert!(out.contains("; nothing was launched"), "{out}");
    assert_eq!(rig.fake.count(Call::Launch), 0);
}

#[test]
fn a_stale_staging_file_is_replaced_by_an_owner_only_one() {
    let rig = Rig::new();
    let stale = rig.dir.join("state.new");
    fs::write(&stale, "stale").unwrap();
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o666)).unwrap();
    let args = rig.go_args(&[]);
    let (status, out) = rig.exec_active(&args, 1);
    assert_eq!(status, 0, "{out}");
    assert!(!stale.exists(), "the staging file was renamed away");
    assert_eq!(
        fs::metadata(&rig.state).unwrap().permissions().mode() & 0o777,
        0o600,
        "the renamed file keeps the staging file's mode"
    );
    assert!(rig.state_text().contains(&format!("instances={FIRST}\n")));
}
