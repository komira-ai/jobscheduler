//! The sweep through its public functions, as a library user (the node's daemon) calls
//! them. The unit tests run the crate compiled for testing, which has hooks the library
//! does not; these run the library as it is shipped, off the hooks.

use std::os::unix::fs::PermissionsExt as _;

use js_mac_session::sweep::{SweepPlan, clear_flags, clear_flags_within, sweep};
use js_testkit::dir::TestDir;

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn tree(name: &str) -> TestDir {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    std::fs::create_dir_all(dir.join("entry/T/inner")).unwrap();
    std::fs::write(dir.join("entry/T/inner/file"), "x").unwrap();
    std::fs::write(dir.join("entry/loose"), "x").unwrap();
    dir
}

/// Catches: a clear of flags that removes what it was only to unlock, reports an error
/// for a tree it can walk, does not reach below a kept folder, or leaves a folder below
/// it that the owner took every permission from closed (its contents would be skipped).
#[test]
fn clearing_flags_changes_nothing_that_is_not_a_flag() {
    let dir = tree("sweep-public-clear");
    let entry = dir.join("entry");
    // A folder the owner closed: given back its `rwx` (not for root, which needs none).
    let closed = entry.join("T/closed");
    std::fs::create_dir(&closed).unwrap();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
    let swept = clear_flags_within(&entry, me(), &["T"]);
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    let swept = clear_flags(&entry, me());
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    assert!(entry.join("T/inner/file").exists());
    assert!(entry.join("loose").exists());
    if me() != 0 {
        let mode = std::fs::metadata(&closed).unwrap().permissions().mode();
        assert_eq!(mode & 0o700, 0o700, "the closed folder was left closed");
    }
}

/// Catches: a sweep that leaves what the uid owns in a place of its plan (a named entry or
/// an owned place), or removes the owned place itself.
#[test]
fn a_sweep_removes_what_the_uid_owns_and_keeps_the_place() {
    let dir = tree("sweep-public-sweep");
    let entry = dir.join("entry");
    let plan = SweepPlan {
        named: vec![format!("{}/loose", entry.display())],
        owned: vec![entry.clone()],
    };
    let swept = sweep(&plan, "u", me());
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    assert!(entry.is_dir());
    assert!(!entry.join("T").exists());
    assert!(!entry.join("loose").exists());
}
