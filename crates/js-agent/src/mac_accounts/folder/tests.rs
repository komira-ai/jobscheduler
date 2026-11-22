//! Tests of emptying and looking at the per-user folders, against real directories. Every file
//! a test makes belongs to the test's own uid (`me`), so a folder a test makes is the uid's
//! when the uid is `me` and another owner's when it is `other`. What macOS does to a removal
//! that a test cannot make (user flags, ACLs) is modelled by [`FakeOs`].

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;

use super::*;
use crate::mac_accounts::tests::os::FakeOs;
use crate::mac_accounts::tests::rig::me;
use crate::mac_accounts::tree::System;

const TOPS: [&str; 4] = ["0", "C", "T", "X"];

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-mac-accounts-folder-{name}"),
    )
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A per-user folder `<folders>/<shard>/<hash>` with the system's four top-level directories.
fn folder(folders: &Path, shard: &str, hash: &str) -> PathBuf {
    let folder = folders.join(shard).join(hash);
    for top in TOPS {
        fs::create_dir_all(folder.join(top)).unwrap();
    }
    folder
}

/// Asserts that `folder` is as the system made it: its four directories, all empty.
fn assert_emptied(folder: &Path) {
    assert_eq!(names(folder), TOPS, "{}", folder.display());
    for top in TOPS {
        assert!(names(&folder.join(top)).is_empty(), "{top}");
    }
}

/// Asserts that what a job's uid can leave in a folder is emptied and the folder kept: files,
/// a tree, a mode 000 directory and a mode 500 one with files, a hard link, a link to a directory
/// and a link to a file outside the folder (the targets survive), a non-directory at the top
/// level, and a top-level directory that is closed: the folder and its top-level directories
/// stay.
#[test]
fn a_folder_is_emptied_and_kept_whatever_a_job_left_in_it() {
    let dir = scratch("empty");
    let folders = dir.join("folders");
    let hash = folder(&folders, "ab", "hash");
    fs::create_dir_all(dir.join("outside")).unwrap();
    fs::write(dir.join("outside/precious"), "x").unwrap();
    for top in TOPS {
        let base = hash.join(top);
        fs::write(base.join("file"), "x").unwrap();
        fs::create_dir_all(base.join("tree/a/b")).unwrap();
        fs::write(base.join("tree/a/b/deep"), "x").unwrap();
        fs::create_dir(base.join("closed")).unwrap();
        fs::write(base.join("closed/f"), "x").unwrap();
        chmod(&base.join("closed"), 0o000);
        fs::create_dir(base.join("read-only")).unwrap();
        fs::write(base.join("read-only/f"), "x").unwrap();
        chmod(&base.join("read-only"), 0o500);
        symlink(dir.join("outside"), base.join("to-directory")).unwrap();
        symlink(dir.join("outside/precious"), base.join("to-file")).unwrap();
        fs::hard_link(base.join("file"), base.join("hard-link")).unwrap();
    }
    fs::write(hash.join("stray"), "x").unwrap();
    symlink(dir.join("outside"), hash.join("stray-link")).unwrap();
    chmod(&hash.join("T"), 0o000);

    empty(&folders, me(), &System).unwrap();
    assert_emptied(&hash);
    assert_eq!(names(&dir.join("outside")), ["precious"]);
    assert!(left(&folders, me(), &System).unwrap().is_empty());
    // Nothing is left to do the second time.
    empty(&folders, me(), &System).unwrap();
}

/// Asserts that every folder of the uid is emptied, in whichever shard it is (an older
/// account of the same uid can have left another), that a folder of another owner is not
/// touched, and that a file, a link and a shard that is a link are not folders.
#[test]
fn every_folder_of_the_uid_is_emptied_and_no_other() {
    let dir = scratch("two");
    let folders = dir.join("folders");
    let first = folder(&folders, "ab", "one");
    let second = folder(&folders, "cd", "two");
    for hash in [&first, &second] {
        fs::write(hash.join("T/f"), "x").unwrap();
        fs::create_dir_all(hash.join("0/dir")).unwrap();
    }
    fs::write(folders.join("ab/file"), "x").unwrap();
    fs::create_dir_all(dir.join("elsewhere/hash3/T")).unwrap();
    fs::write(dir.join("elsewhere/hash3/T/f"), "x").unwrap();
    symlink(dir.join("elsewhere"), folders.join("ef")).unwrap();

    // Another uid owns none of them.
    empty(&folders, me() + 1, &System).unwrap();
    assert_eq!(names(&first.join("T")), ["f"]);
    assert_eq!(
        left(&folders, me() + 1, &System).unwrap(),
        Vec::<PathBuf>::new()
    );

    let mut found = left(&folders, me(), &System).unwrap();
    found.sort();
    assert_eq!(
        found,
        [
            first.join("0/dir"),
            first.join("T/f"),
            second.join("0/dir"),
            second.join("T/f")
        ]
    );
    empty(&folders, me(), &System).unwrap();
    assert_emptied(&first);
    assert_emptied(&second);
    assert_eq!(names(&folders.join("ab")), ["file", "one"]);
    assert_eq!(names(&dir.join("elsewhere/hash3/T")), ["f"]);
    assert!(left(&folders, me(), &System).unwrap().is_empty());
}

/// Asserts that a user flag is cleared before the entry is removed, on every entry whoever
/// owns it and on the top-level directory too (a directory with a flag blocks removing what is
/// in it), that an entry the system still refuses is stripped of its ACL and removed on the
/// second try, and that nothing is stripped that was not refused.
#[test]
fn flags_are_cleared_first_and_a_refused_entry_has_its_acl_stripped() {
    let dir = scratch("lift");
    let folders = dir.join("folders");
    let hash = folder(&folders, "ab", "hash");
    let base = hash.join("T");
    fs::create_dir(base.join("lockdir")).unwrap();
    fs::write(base.join("lockdir/inside"), "x").unwrap();
    fs::write(base.join("immutable"), "x").unwrap();
    fs::write(base.join("append-only"), "x").unwrap();
    fs::write(base.join("acl-file"), "x").unwrap();
    fs::create_dir(base.join("acl-dir")).unwrap();
    fs::write(base.join("acl-dir/f"), "x").unwrap();
    fs::write(base.join("plain"), "x").unwrap();
    fs::write(hash.join("stray"), "x").unwrap();
    let os = FakeOs::new();
    for name in ["T", "lockdir", "immutable", "append-only", "stray"] {
        os.flag(name);
    }
    os.deny("acl-file");
    os.deny("acl-dir");

    empty(&folders, me(), &os).unwrap();
    assert_emptied(&hash);
    let calls = os.calls();
    let at = |call: &str| {
        calls
            .iter()
            .position(|made| made == call)
            .unwrap_or_else(|| panic!("{call} was not made: {calls:?}"))
    };
    for name in [
        "lockdir",
        "immutable",
        "append-only",
        "stray",
        "plain",
        "inside",
    ] {
        assert!(
            at(&format!("clear_flags {name}")) < at(&format!("unlink {name}")),
            "{name}: {calls:?}"
        );
    }
    // The top-level directory's flags are cleared before anything in it is touched, and the
    // directory is not removed.
    assert!(at("clear_flags T") < at("unlink lockdir"), "{calls:?}");
    assert!(!calls.contains(&"unlink T".to_owned()), "{calls:?}");
    // An ACL is stripped after the refusal, then the removal is tried again; a directory's
    // removal is refused after it was emptied.
    for name in ["acl-file", "acl-dir"] {
        let first = at(&format!("unlink {name}"));
        assert_eq!(calls[first + 1], format!("strip_acl {name}"), "{calls:?}");
        assert_eq!(calls[first + 2], format!("unlink {name}"), "{calls:?}");
    }
    assert!(at("unlink f") < at("unlink acl-dir"), "{calls:?}");
    assert!(
        calls
            .iter()
            .all(|call| !call.starts_with("strip_acl plain")
                && !call.starts_with("strip_acl immutable")),
        "{calls:?}"
    );
}

/// Asserts that an entry whose flags cannot be cleared, and one whose ACL cannot be stripped,
/// are each named with why, that the other entries are removed all the same, and that the
/// folder is kept.
#[test]
fn an_entry_that_cannot_be_unlocked_is_named_and_the_rest_goes() {
    let dir = scratch("stuck");
    let folders = dir.join("folders");
    let hash = folder(&folders, "ab", "hash");
    fs::write(hash.join("C/frozen"), "x").unwrap();
    fs::write(hash.join("C/denied"), "x").unwrap();
    fs::write(hash.join("C/loose"), "x").unwrap();
    fs::write(hash.join("0/loose"), "x").unwrap();
    let os = FakeOs::new();
    os.flag_for_good("frozen");
    os.deny_for_good("denied");

    let why = empty(&folders, me(), &os).unwrap_err().to_string();
    assert!(
        why.contains(&format!(
            "{}: the flags cannot be cleared",
            hash.join("C/frozen").display()
        )),
        "{why}"
    );
    assert!(
        why.contains(&format!(
            "{}: Permission denied (os error 13); stripping its ACL: the ACL cannot be stripped",
            hash.join("C/denied").display()
        )),
        "{why}"
    );
    assert_eq!(names(&hash.join("C")), ["denied", "frozen"]);
    assert!(names(&hash.join("0")).is_empty());
    assert_eq!(names(&hash), TOPS);
    let mut found = left(&folders, me(), &System).unwrap();
    found.sort();
    assert_eq!(found, [hash.join("C/denied"), hash.join("C/frozen")]);
}

/// Asserts that a tree of any depth below a top-level directory is emptied by a removal that
/// lifts, as it is by the others: the deep part is moved up and removed from there.
#[test]
fn a_deep_tree_in_a_folder_is_emptied() {
    let dir = scratch("deep");
    let folders = dir.join("folders");
    let hash = folder(&folders, "ab", "hash");
    let flags = rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY;
    let mut level = rustix::fs::openat(
        rustix::fs::CWD,
        hash.join("0"),
        flags,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    for _ in 0..3000 {
        rustix::fs::mkdirat(&level, "d", rustix::fs::Mode::from_raw_mode(0o755)).unwrap();
        level = rustix::fs::openat(&level, "d", flags, rustix::fs::Mode::empty()).unwrap();
    }
    drop(level);
    assert_eq!(names(&hash.join("0")), ["d"]);
    empty(&folders, me(), &System).unwrap();
    assert_emptied(&hash);
}

/// Asserts that folders that cannot be searched fail the emptying and the look, naming them,
/// and that a failure to empty one folder does not stop the next.
#[test]
fn folders_that_cannot_be_searched_fail_naming_them() {
    let dir = scratch("search");
    let folders = dir.join("folders");
    fs::write(&folders, "").unwrap();
    for why in [
        empty(&folders, me(), &System).unwrap_err().to_string(),
        left(&folders, me(), &System).unwrap_err().to_string(),
    ] {
        assert!(
            why.starts_with(&format!("{}: ", folders.display())),
            "{why}"
        );
    }
    fs::remove_file(&folders).unwrap();
    // No folders at all holds nothing.
    empty(&folders, me(), &System).unwrap();
    assert!(left(&folders, me(), &System).unwrap().is_empty());

    let first = folder(&folders, "ab", "one");
    let second = folder(&folders, "cd", "two");
    fs::write(first.join("T/frozen"), "x").unwrap();
    fs::write(second.join("T/loose"), "x").unwrap();
    let os = FakeOs::new();
    os.flag_for_good("frozen");
    let why = empty(&folders, me(), &os).unwrap_err().to_string();
    assert!(
        why.contains("T/frozen: the flags cannot be cleared"),
        "{why}"
    );
    assert_emptied(&second);
}

/// Asserts what is kept of the items macOS's own daemons seal in a per-user folder: Launch
/// Services' data vault in `0`, sealed, stays with what is in it, and a look does not count
/// it; the trust daemon's folder in `T`, not sealed, is removed like anything else; a sealed
/// item that is not one of the system's (a name of the list in another top-level directory,
/// another name) is refused and named, and a look counts it. Without the seal (`System` off
/// macOS) the vault is an ordinary leftover.
#[test]
fn only_a_sealed_system_item_is_kept_in_a_folder() {
    let dir = scratch("sealed");
    let folders = dir.join("folders");
    let hash = folder(&folders, "ab", "hash");
    fs::create_dir_all(hash.join("0/com.apple.LaunchServices.dv")).unwrap();
    fs::write(hash.join("0/com.apple.LaunchServices.dv/db"), "x").unwrap();
    fs::create_dir_all(hash.join("T/com.apple.trustd")).unwrap();
    fs::write(hash.join("T/com.apple.trustd/cache"), "x").unwrap();
    fs::write(hash.join("X/com.apple.LaunchServices.dv"), "x").unwrap();
    fs::write(hash.join("C/vault"), "x").unwrap();
    let os = FakeOs::new();
    os.seal("com.apple.LaunchServices.dv");
    os.seal("vault");

    let why = empty(&folders, me(), &os).unwrap_err().to_string();
    for refused in ["X/com.apple.LaunchServices.dv", "C/vault"] {
        assert!(
            why.contains(&hash.join(refused).display().to_string()),
            "{why}"
        );
    }
    assert!(!why.contains("0/com.apple"), "{why}");
    assert_eq!(names(&hash.join("0/com.apple.LaunchServices.dv")), ["db"]);
    assert!(names(&hash.join("T")).is_empty());
    let mut found = left(&folders, me(), &os).unwrap();
    found.sort();
    assert_eq!(
        found,
        [
            hash.join("C/vault"),
            hash.join("X/com.apple.LaunchServices.dv")
        ]
    );
    assert!(os.calls().contains(&"sealed com.apple.trustd".to_owned()));
    let unsealed = left(&folders, me(), &System).unwrap();
    assert!(unsealed.contains(&hash.join("0/com.apple.LaunchServices.dv")));
}
