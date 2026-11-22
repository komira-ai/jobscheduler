//! The hand-over of a put directory to the node, on a scratch directory, to this test's own
//! user and group (which needs no root; `root_tests` hands it to another user as root).

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use js_testkit::dir::TestDir;
use rustix::process::{getegid, geteuid};

use super::{DIR_MODE, Node, PutTarget, hand_over};

/// The put directory `cas/b/r1/n1` below `root`.
fn target(root: &Path) -> PutTarget {
    PutTarget {
        dir: root.join("cas/b/r1/n1"),
        key_prefix: "cas/b/r1/n1".to_owned(),
    }
}

fn me() -> Node {
    Node {
        uid: geteuid().as_raw(),
        gid: getegid().as_raw(),
    }
}

/// Proves the hand-over gives the three folders of the key and the job's directory their
/// owners and leaves the job's directory mode 0770 (it was made 0755), the root itself
/// untouched. Catches a job directory left 0755 (the node's group could not remove the
/// file) or one another user could enter.
#[test]
fn the_put_directory_is_handed_over_with_its_mode() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-node");
    let root = scratch.path().join("put");
    let to = target(&root);
    fs::create_dir_all(&to.dir).unwrap();
    fs::set_permissions(&to.dir, fs::Permissions::from_mode(0o755)).unwrap();
    let root_mode = fs::metadata(&root).unwrap().mode();
    hand_over(&root, &to, me().uid, me()).unwrap();
    for part in ["cas", "cas/b", "cas/b/r1", "cas/b/r1/n1"] {
        let meta = fs::symlink_metadata(root.join(part)).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (me().uid, me().gid), "{part}");
    }
    let mode = fs::metadata(&to.dir).unwrap().mode() & 0o7777;
    assert_eq!(mode, DIR_MODE);
    assert_eq!(fs::metadata(&root).unwrap().mode(), root_mode, "the root");
}

/// Proves a hand-over of a folder or a job directory that is not there fails naming it.
/// Catches a hand-over that reports success with the node left unable to remove the file.
#[test]
fn a_missing_folder_fails_the_hand_over_by_name() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-node-missing");
    let root = scratch.path().join("put");
    fs::create_dir_all(&root).unwrap();
    let to = target(&root);
    let why = hand_over(&root, &to, me().uid, me()).unwrap_err();
    assert_eq!(why.kind(), io::ErrorKind::NotFound);
    assert!(
        why.to_string()
            .starts_with(&format!("{}: ", root.join("cas").display())),
        "{why}"
    );

    fs::create_dir_all(root.join("cas/b/r1")).unwrap();
    let why = hand_over(&root, &to, me().uid, me()).unwrap_err();
    assert!(
        why.to_string()
            .starts_with(&format!("{}: ", to.dir.display())),
        "{why}"
    );
}
