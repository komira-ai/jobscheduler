use std::fs;
use std::os::unix::fs::PermissionsExt as _;

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
            [format!("{}: not a directory; not removed", path.display())]
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
    assert_eq!(swept.errors.len(), 1, "{:?}", swept.errors);
    assert!(
        swept.errors[0].starts_with(&format!("{}: ", entry.display())),
        "{:?}",
        swept.errors
    );
    assert!(entry.join("T/tmp").exists());
}
