//! The scan of a host's setuid and setgid files.

mod common;

use std::os::unix::fs::PermissionsExt as _;

use common::Scratch;
use js_mac_pool::seatbelt::{MacosMajor, scan_privileged};

fn set(path: &std::path::Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("mode");
}

fn text(path: &std::path::Path) -> String {
    path.to_str().expect("text").to_owned()
}

#[test]
fn setuid_and_setgid_files_are_found_and_plain_ones_are_not() {
    let scratch = Scratch::new("scan-find");
    let uid = scratch.file("bin/uid", "x");
    let gid = scratch.file("bin/deep/gid", "x");
    let both = scratch.file("bin/both", "x");
    let plain = scratch.file("bin/plain", "x");
    let sticky = scratch.file("bin/sticky", "x");
    set(&uid, 0o4755);
    set(&gid, 0o2755);
    set(&both, 0o6755);
    set(&plain, 0o755);
    set(&sticky, 0o1755);
    let root = text(&scratch.root.join("bin"));
    let mut expected = vec![text(&uid), text(&gid), text(&both)];
    expected.sort();
    assert_eq!(
        scan_privileged(&[root.as_str()]).paths(),
        expected.as_slice()
    );
    assert_eq!(
        scan_privileged(&[root.as_str(), root.as_str()]).paths(),
        expected.as_slice()
    );
}

#[test]
fn links_are_not_followed_and_missing_roots_hold_nothing() {
    let scratch = Scratch::new("scan-links");
    let target = scratch.file("real/uid", "x");
    set(&target, 0o4755);
    let linked_file = scratch.root.join("tree/file-link");
    let linked_dir = scratch.root.join("tree/dir-link");
    scratch.dir("tree");
    std::os::unix::fs::symlink(&target, &linked_file).expect("link");
    std::os::unix::fs::symlink(scratch.root.join("real"), &linked_dir).expect("link");
    let tree = text(&scratch.root.join("tree"));
    let missing = text(&scratch.root.join("missing"));
    assert!(
        scan_privileged(&[tree.as_str(), missing.as_str()])
            .paths()
            .is_empty()
    );
}

#[test]
fn a_directory_that_cannot_be_read_holds_nothing() {
    let scratch = Scratch::new("scan-unreadable");
    let hidden = scratch.file("tree/hidden/uid", "x");
    set(&hidden, 0o4755);
    let visible = scratch.file("tree/uid", "x");
    set(&visible, 0o4755);
    let dir = hidden.parent().expect("parent").to_owned();
    set(&dir, 0o000);
    let tree = text(&scratch.root.join("tree"));
    let found = scan_privileged(&[tree.as_str()]);
    set(&dir, 0o755);
    assert_eq!(found.paths(), [text(&visible)]);
}

#[test]
fn the_scan_of_a_version_walks_the_directories_its_base_set_grants() {
    let found = MacosMajor::Macos14.scan();
    assert!(found.paths().iter().all(|path| path.starts_with('/')));
}
