//! Which directories are a job's put directory, on a scratch put root.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::symlink;
use std::path::Path;

use js_testkit::dir::TestDir;
use rustix::io::Errno;

use super::{PutTarget, Refusal, target_of};

/// The refusal of `path` saying `why`.
fn refusal(path: &Path, why: &str) -> Refusal {
    Refusal {
        path: path.to_owned(),
        why: why.to_owned(),
    }
}

const SHAPE: &str = "not of the shape cas/b/<build>/<nonce> below the put root";

/// Proves only `cas/b/<build>/<nonce>` strictly beneath the put root is a put directory, with
/// that path as its key prefix and the root joined with it as its directory, whether it
/// exists yet or not (a `.` in the path names the same directory); and that everything else
/// is refused by its path and why: the root itself, `cas/b/<build>`, five components, other
/// first or second components, a dot-leading build or nonce, `..`, an absolute path outside
/// the root, a relative path, and a name that is not UTF-8. Catches a commit made outside
/// the put root or under a key the build service did not name.
#[test]
fn target_of_accepts_only_cas_b_build_nonce_below_the_root() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-target");
    let root = scratch.path().join("put");
    fs::create_dir_all(&root).unwrap();
    let want = |dir: &Path| PutTarget {
        dir: dir.to_owned(),
        key_prefix: "cas/b/r1/n1".to_owned(),
    };
    let dir = root.join("cas/b/r1/n1");
    assert_eq!(target_of(&root, &dir), Ok(want(&dir)), "not made yet");
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(target_of(&root, &dir), Ok(want(&dir)), "made");
    let dotted = root.join("cas/b/./r1/n1");
    assert_eq!(target_of(&root, &dotted), Ok(want(&dir)), "a `.`");

    for below in [
        "",
        "cas/b/r1",
        "cas/b/r1/n1/more",
        "kas/b/r1/n1",
        "cas/x/r1/n1",
        "cas/b/.r1/n1",
        "cas/b/r1/.n1",
        "cas/b/r1/..n1",
    ] {
        let dir = root.join(below);
        assert_eq!(target_of(&root, &dir), Err(refusal(&dir, SHAPE)), "{below}");
    }

    let dotted = root.join("cas/b/r1/../r1/n1");
    let names = refusal(&dotted, "names `..`");
    assert_eq!(target_of(&root, &dotted), Err(names));
    let outside = scratch.path().join("elsewhere/cas/b/r1/n1");
    let beneath = refusal(&outside, "not beneath the put root");
    assert_eq!(target_of(&root, &outside), Err(beneath));
    let relative = Path::new("cas/b/r1/n1");
    let beneath = refusal(relative, "not beneath the put root");
    assert_eq!(target_of(&root, relative), Err(beneath));
    let unnamed = root
        .join("cas/b")
        .join(OsStr::from_bytes(b"r\xff"))
        .join("n1");
    let utf8 = refusal(&unnamed, "not valid UTF-8");
    assert_eq!(target_of(&root, &unnamed), Err(utf8));
}

/// Proves a put directory whose existing path below the root passes a link, or something
/// that is not a directory, is refused naming that part, at each of the four places: a
/// `cas` linked to a directory outside the root, a `b` that is a file, a build linked
/// outside, and a nonce that is a link or a file. Catches a commit that a link sends outside
/// the put root, and a check that stops before the build or the nonce.
#[test]
fn a_link_or_a_file_on_the_way_is_refused() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-target-link");
    let away = scratch.path().join("away");
    fs::create_dir_all(away.join("b/r1/n1")).unwrap();
    let refused = |root: &Path, wrong: &str| {
        let dir = root.join("cas/b/r1/n1");
        let at = root.join(wrong);
        assert_eq!(
            target_of(root, &dir),
            Err(refusal(&at, "a link or not a directory")),
            "{wrong}"
        );
    };

    let root = scratch.path().join("linked-cas");
    fs::create_dir_all(&root).unwrap();
    symlink(&away, root.join("cas")).unwrap();
    refused(&root, "cas");

    let root = scratch.path().join("file-b");
    fs::create_dir_all(root.join("cas")).unwrap();
    fs::write(root.join("cas/b"), "").unwrap();
    refused(&root, "cas/b");

    let root = scratch.path().join("linked-build");
    fs::create_dir_all(root.join("cas/b")).unwrap();
    symlink(away.join("b/r1"), root.join("cas/b/r1")).unwrap();
    refused(&root, "cas/b/r1");

    let root = scratch.path().join("linked-nonce");
    fs::create_dir_all(root.join("cas/b/r1")).unwrap();
    symlink(away.join("b/r1/n1"), root.join("cas/b/r1/n1")).unwrap();
    refused(&root, "cas/b/r1/n1");

    let root = scratch.path().join("file-nonce");
    fs::create_dir_all(root.join("cas/b/r1")).unwrap();
    fs::write(root.join("cas/b/r1/n1"), "").unwrap();
    refused(&root, "cas/b/r1/n1");
}

/// Proves a part of the path that cannot be looked at (a nonce longer than a file name may
/// be, which no file system takes) is refused naming it, with the system's error. Catches a
/// failed look taken for a part that is not there yet.
#[test]
fn a_part_that_cannot_be_looked_at_is_refused() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-target-long");
    let root = scratch.path().join("put");
    fs::create_dir_all(root.join("cas/b/r1")).unwrap();
    let dir = root.join("cas/b/r1").join("n".repeat(300));
    let why = io::Error::from(Errno::NAMETOOLONG).to_string();
    assert_eq!(target_of(&root, &dir), Err(refusal(&dir, &why)));
}

/// Proves a refusal reads as its path and why. Catches a refusal that loses the path.
#[test]
fn a_refusal_names_its_path() {
    let said = refusal(Path::new("/put/cas"), "a link or not a directory").to_string();
    assert_eq!(said, "/put/cas: a link or not a directory");
}
