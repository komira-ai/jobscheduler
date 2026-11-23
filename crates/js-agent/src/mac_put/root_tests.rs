//! The hand-over of a put directory and the commit in it for real, as root: the folders of
//! the key are given to another user (a uid and gid no account needs to hold), the job's
//! directory to root with that group, and what the system then shows is read back. Each test
//! needs root, so a plain `cargo test` skips it by name (`#[ignore]`, with the reason); the
//! macOS beta stage runs exactly these under `sudo` (`beta.yml`, `mac_put::root_tests`).

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use js_testkit::dir::TestDir;
use rustix::process::geteuid;

use super::{DIR_MODE, Node, PutTarget, commit, hand_over};

/// The node of these tests: a uid and gid no account needs to hold, outside the agent's and
/// the other root tests' ranges.
const NODE: Node = Node {
    uid: 4321,
    gid: 4321,
};

/// Proves, as root, that a commit whose `ready` is the real hand-over leaves the folders of
/// the key owned by the node, the job's directory root's with the node's group and mode
/// 0770, and `pack` root's, of the group the directory gives it, mode 0640, holding the
/// bytes: what the node needs to read the file and remove it and the directory. Catches a
/// hand-over that changes nothing as root (a `chown` that fails silently, a wrong id) and a
/// pack of another group than the directory's.
#[test]
#[ignore = "needs root: the macOS beta stage's root step runs it (beta.yml)"]
fn as_root_the_put_directory_is_the_nodes_and_the_pack_its_groups() {
    assert_eq!(geteuid().as_raw(), 0, "this test needs root");
    let scratch = TestDir::under(std::env::temp_dir(), "put-root");
    let root = scratch.path().join("put");
    fs::create_dir_all(&root).unwrap();
    let out = scratch.path().join("outputs");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("pack"), b"the pack").unwrap();
    let to = PutTarget {
        dir: root.join("cas/b/r1/n1"),
        key_prefix: "cas/b/r1/n1".to_owned(),
    };
    let ready = |_: &Path| hand_over(&root, &to, 0, NODE);
    commit(&out.join("pack"), &to, &ready).unwrap();
    for part in ["cas", "cas/b", "cas/b/r1"] {
        let meta = fs::symlink_metadata(root.join(part)).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (NODE.uid, NODE.gid), "{part}");
    }
    let dir = fs::symlink_metadata(&to.dir).unwrap();
    assert_eq!((dir.uid(), dir.gid()), (0, NODE.gid), "the job's directory");
    assert_eq!(dir.permissions().mode() & 0o7777, DIR_MODE);
    let pack = fs::symlink_metadata(to.dir.join("pack")).unwrap();
    // A new file takes its directory's group on a Mac (BSD semantics) and on Linux takes the
    // creator's; the node reads it through the group, so it must be the directory's.
    assert_eq!(pack.uid(), 0, "pack");
    assert_eq!(pack.permissions().mode() & 0o7777, 0o640, "pack");
    assert_eq!(fs::read(to.dir.join("pack")).unwrap(), b"the pack");
    if cfg!(target_os = "macos") {
        assert_eq!(pack.gid(), NODE.gid, "pack takes the directory's group");
    }
}

/// Proves, as root, that a link in the job directory's place gives the node nothing outside
/// the put root: the directory the link names keeps its owner, group and mode (root's, 0755),
/// and the hand-over fails. Catches a hand-over by `chown` or `chmod` of a path, which follows
/// the link and hands the node's user a directory the agent never made.
#[test]
#[ignore = "needs root: the macOS beta stage's root step runs it (beta.yml)"]
fn as_root_a_link_in_the_job_directorys_place_is_not_followed() {
    assert_eq!(geteuid().as_raw(), 0, "this test needs root");
    let scratch = TestDir::under(std::env::temp_dir(), "put-root-link");
    let root = scratch.path().join("put");
    fs::create_dir_all(root.join("cas/b/r1")).unwrap();
    let outside = scratch.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
    let to = PutTarget {
        dir: root.join("cas/b/r1/n1"),
        key_prefix: "cas/b/r1/n1".to_owned(),
    };
    std::os::unix::fs::symlink(&outside, &to.dir).unwrap();
    assert!(hand_over(&root, &to, 0, NODE).is_err());
    let meta = fs::metadata(&outside).unwrap();
    assert_eq!(
        (meta.uid(), meta.gid()),
        (0, 0),
        "the outside directory's owners"
    );
    assert_eq!(meta.permissions().mode() & 0o7777, 0o755);
}
