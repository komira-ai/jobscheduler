//! The sweep through its public functions, as a library user (the node's daemon) calls
//! them. The unit tests run the crate compiled for testing, which has hooks the library
//! does not; these run the library as it is shipped, off the hooks.

use std::os::unix::fs::PermissionsExt as _;

use js_mac_session::sweep::{clear_flags, clear_flags_within, empty, empty_keeping};
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

/// Catches: an empty that leaves what the uid owns, removes the folder it empties, or
/// removes a kept folder (it must be emptied and left).
#[test]
fn empty_removes_the_contents_and_keeps_the_folder_and_the_kept_folders() {
    let dir = tree("sweep-public-empty");
    let entry = dir.join("entry");
    let swept = empty_keeping(&entry, me(), &["T"]);
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    assert!(entry.join("T").is_dir());
    assert!(!entry.join("T/inner").exists());
    assert!(!entry.join("loose").exists());
    let swept = empty(&entry, me());
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    assert!(entry.is_dir());
    assert!(!entry.join("T").exists());
}
