use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use js_testkit::dir::TestDir;

use super::*;

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kms-entry-{name}"))
}

/// A per-user folders entry with its three folders, a daemon's folder in `C` and a
/// link out of it.
fn populate(entry: &Path, outside: &Path) {
    for dir in ["0", "C/com.apple.ScreenTimeAgent/deep", "T"] {
        fs::create_dir_all(entry.join(dir)).unwrap();
    }
    fs::write(entry.join("C/com.apple.ScreenTimeAgent/deep/db"), "x").unwrap();
    fs::write(entry.join("T/tmp"), "x").unwrap();
    std::os::unix::fs::symlink(outside, entry.join("C/out")).unwrap();
}

/// Asserts that the uid's entry goes whole, counted once, and that what a link in it
/// points at stays. It catches a removal of only the uid's own files or of `0` alone,
/// and one that follows a link out of the entry.
#[test]
fn the_uid_s_entry_goes_whole_and_no_link_is_followed() {
    let dir = scratch("whole");
    let outside = dir.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep"), "x").unwrap();
    let entry = dir.join("ab/cdef");
    populate(&entry, &outside);
    assert_eq!(
        remove_owned(&entry, me()),
        Swept {
            removed: 1,
            ..Swept::default()
        }
    );
    assert!(!entry.exists());
    assert!(dir.join("ab").exists());
    assert!(outside.join("keep").exists());
}

/// Asserts that nothing is removed unless the path is a directory the uid owns: one of
/// another owner, a link (whatever it points at) and a file are each refused by exact
/// message and left as they were. It catches a wrong answer for the uid's entry
/// removing another user's.
#[test]
fn only_a_directory_the_uid_owns_is_removed() {
    let dir = scratch("owner");
    let outside = dir.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let entry = dir.join("ab/cdef");
    populate(&entry, &outside);
    let other = me() + 1;
    let swept = remove_owned(&entry, other);
    assert_eq!(
        swept.errors,
        [format!(
            "{}: owned by uid {}, not uid {other}; not removed",
            entry.display(),
            me()
        )]
    );
    assert_eq!(swept.removed, 0);
    assert!(entry.join("T/tmp").exists());

    let link = dir.join("ab/link");
    std::os::unix::fs::symlink(&entry, &link).unwrap();
    let file = dir.join("ab/file");
    fs::write(&file, "x").unwrap();
    for path in [&link, &file] {
        assert_eq!(
            remove_owned(path, me()).errors,
            [format!(
                "{}: not a directory on its parent's filesystem; not removed",
                path.display()
            )]
        );
        assert!(path.symlink_metadata().is_ok());
    }
    assert!(entry.join("T/tmp").exists());
}

/// Asserts that an entry or a parent that does not exist holds nothing (no error), and
/// that a path naming no entry is an error. It catches an absent entry (the user had
/// none yet) taken for a failure, which would hold its uid for good.
#[test]
fn nothing_there_is_nothing_to_remove() {
    let dir = scratch("absent");
    for path in [dir.join("ab/cdef"), dir.join("ab")] {
        assert_eq!(remove_owned(&path, me()), Swept::default());
    }
    assert_eq!(
        remove_owned(Path::new("/"), me()).errors,
        ["/: not a path to an entry"]
    );
}

/// Asserts that an entry with something in it that cannot be removed is an error, and
/// not counted as removed. It catches a removal reported done while part of the entry
/// stays.
#[test]
fn an_entry_that_cannot_be_emptied_is_an_error() {
    if rustix::process::geteuid().is_root() {
        // Root removes it whatever its mode; the refusal needs a user.
        println!("skipped: root is not refused the removal this test needs");
        return;
    }
    let dir = scratch("stuck");
    let entry = dir.join("ab/cdef");
    fs::create_dir_all(entry.join("T")).unwrap();
    fs::write(entry.join("T/tmp"), "x").unwrap();
    let t = entry.join("T");
    fs::set_permissions(&t, fs::Permissions::from_mode(0o500)).unwrap();
    let swept = remove_owned(&entry, me());
    fs::set_permissions(&t, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(swept.removed, 0);
    assert_eq!(
        swept.errors,
        [format!(
            "{}: {}: Permission denied (os error 13)",
            t.display(),
            t.join("tmp").display()
        )]
    );
    assert!(entry.join("T/tmp").exists());
    // Once nothing stops it, the same entry goes.
    assert_eq!(remove_owned(&entry, me()).removed, 1);
    assert!(!entry.exists());
}

/// `depth` directories, each named `d`, below `top`; the deepest one's path.
fn deep(top: &Path, depth: usize) -> PathBuf {
    let mut path = top.to_path_buf();
    for _ in 0..depth {
        path.push("d");
    }
    fs::create_dir_all(&path).unwrap();
    path
}

/// Gives every directory below `top` its owner's `rwx` back.
fn open_up(top: &Path) {
    fs::set_permissions(top, fs::Permissions::from_mode(0o700)).unwrap();
    for entry in fs::read_dir(top).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            open_up(&entry.path());
        }
    }
}

/// Asserts that a tree too deep to remove in place is moved up inside the entry and
/// never beside it: when part of it cannot be removed, the entry's parent holds only
/// the entry, and the removal repeated once nothing stops it takes everything. It
/// catches a deep subtree parked in the per-user folders beside the entry, which a
/// later removal of the entry (the only one the uid's `DARWIN_USER_DIR` names) would
/// never find, so the uid would be freed with its data left.
#[test]
fn a_deep_tree_is_moved_up_inside_the_entry_and_found_again() {
    if rustix::process::geteuid().is_root() {
        // Root removes it whatever its mode; the refusal needs a user.
        println!("skipped: root is not refused the removal this test needs");
        return;
    }
    let dir = scratch("deep");
    let entry = dir.join("ab/cdef");
    let bottom = deep(&entry.join("C"), 80);
    fs::write(bottom.join("db"), "x").unwrap();
    fs::set_permissions(&bottom, fs::Permissions::from_mode(0o500)).unwrap();
    let swept = remove_owned(&entry, me());
    assert_eq!(swept.removed, 0);
    assert_eq!(swept.errors.len(), 1, "{:?}", swept.errors);
    let beside: Vec<_> = fs::read_dir(dir.join("ab"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(beside, ["cdef"]);
    open_up(&entry);
    assert_eq!(remove_owned(&entry, me()).removed, 1);
    assert_eq!(fs::read_dir(dir.join("ab")).unwrap().count(), 0);

    // A deep tree with nothing to stop it goes in one removal.
    deep(&entry.join("T"), 80);
    assert_eq!(remove_owned(&entry, me()).removed, 1);
    assert!(!entry.exists());
}

/// Asserts that an entry whose parent is a link or a file is an error and nothing is
/// followed: what the link points at stays. It catches a removal that follows a link
/// planted for the per-user folders' `<xx>` out of them.
#[test]
fn an_entry_whose_parent_is_no_directory_is_an_error() {
    let dir = scratch("parent");
    let outside = dir.join("outside");
    fs::create_dir_all(outside.join("cdef")).unwrap();
    fs::write(outside.join("cdef/keep"), "x").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("ab")).unwrap();
    fs::write(dir.join("zz"), "x").unwrap();
    for parent in ["ab", "zz"] {
        let path = dir.join(parent).join("cdef");
        let swept = remove_owned(&path, me());
        assert_eq!(swept.removed, 0);
        assert_eq!(swept.errors.len(), 1, "{:?}", swept.errors);
        assert!(
            swept.errors[0].starts_with(&format!("{}: ", path.display())),
            "{:?}",
            swept.errors
        );
    }
    assert!(outside.join("cdef/keep").exists());
}

/// Asserts which entries count as the uid's when there is no `DARWIN_USER_DIR` to name
/// one: the directories two levels down that it owns, in any `<xx>`; not a file, not a
/// link, nothing for another uid, and none when the folders do not exist. It catches a
/// search that follows a link out of the folders or takes another user's entry.
#[test]
fn the_uid_s_entries_are_its_own_directories_two_levels_down() {
    let dir = scratch("owned");
    let folders = dir.join("folders");
    assert_eq!(
        owned_entries(&folders, me()).unwrap(),
        Vec::<PathBuf>::new()
    );
    fs::create_dir_all(folders.join("ab/one/C")).unwrap();
    fs::create_dir_all(folders.join("cd/two")).unwrap();
    fs::write(folders.join("ab/file"), "x").unwrap();
    fs::write(folders.join("loose"), "x").unwrap();
    fs::create_dir_all(dir.join("elsewhere/three")).unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere"), folders.join("ef")).unwrap();
    std::os::unix::fs::symlink(folders.join("cd/two"), folders.join("ab/link")).unwrap();
    let mut found = owned_entries(&folders, me()).unwrap();
    found.sort();
    assert_eq!(found, [folders.join("ab/one"), folders.join("cd/two")]);
    assert!(owned_entries(&folders, me() + 1).unwrap().is_empty());

    let not_a_dir = dir.join("file-folders");
    fs::write(&not_a_dir, "x").unwrap();
    assert!(owned_entries(&not_a_dir, me()).is_err());
}
