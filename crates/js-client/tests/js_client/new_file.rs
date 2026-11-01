//! `create_new`: a file made whole or not at all, and never over another.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use js_client::{create_new, probe_dir};

use crate::support::Scratch;

/// Asserts that the file is made with the bytes and exactly the mode asked for, whatever the
/// process's umask takes off (0664 is more than the usual 022 allows), that it is a file of
/// its own (one link), and that nothing else is left in the directory. It catches a mode
/// left to the umask, a file that is a second name of the staged one, and a leftover.
#[test]
fn the_file_has_the_bytes_and_the_mode_and_nothing_is_left() {
    let dir = Scratch::new("create_new_ok");
    for (name, mode) in [("private", 0o600), ("shared", 0o664)] {
        let path = dir.path(name);
        create_new(&path, b"content\n", mode).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"content\n");
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, mode);
        assert_eq!(meta.nlink(), 1);
    }
    let mut names: Vec<_> = fs::read_dir(dir.path(""))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["private", "shared"]);
}

/// Asserts that anything already at the path (a file, a link to nothing, a directory) is left
/// as it was and the error is `AlreadyExists` without the content in its text, and that the
/// staged file is gone. It catches an overwrite and a link followed.
#[test]
fn what_is_there_is_never_replaced() {
    let dir = Scratch::new("create_new_exists");
    let file = dir.write("file", b"keep", 0o600);
    symlink(dir.path("nothing"), dir.path("dangling")).unwrap();
    fs::create_dir(dir.path("dir")).unwrap();
    for name in ["file", "dangling", "dir"] {
        let err = create_new(&dir.path(name), b"SECRET-CONTENT", 0o600).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists, "{name}");
        assert!(!err.to_string().contains("SECRET"), "{name}: {err}");
        assert!(
            !dir.path(&format!(".{name}.new")).exists(),
            "{name}: staged file left"
        );
    }
    assert_eq!(fs::read(file).unwrap(), b"keep");
    assert!(!dir.path("nothing").exists());
    assert!(fs::metadata(dir.path("dir")).unwrap().is_dir());
}

/// Asserts that a name an earlier version staged under (`.f.new`), as a directory or a file,
/// does not stop the call and is not touched, and that a missing directory is an error that
/// makes nothing. It catches a staging name that another user can guess and plant.
#[test]
fn the_staging_name_cannot_be_planted() {
    let dir = Scratch::new("create_new_planted");
    fs::create_dir(dir.path(".f.new")).unwrap();
    dir.write(".g.new", b"theirs", 0o644);
    create_new(&dir.path("f"), b"new", 0o600).unwrap();
    create_new(&dir.path("g"), b"new", 0o600).unwrap();
    assert_eq!(fs::read(dir.path("f")).unwrap(), b"new");
    assert_eq!(fs::read(dir.path(".g.new")).unwrap(), b"theirs");
    assert!(dir.path(".f.new").is_dir());

    let err = create_new(&dir.path("absent").join("f"), b"x", 0o600).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

/// Asserts that when the link is made and only the sync of the directory fails (a directory
/// that can be written and searched but not read, 0300) the call succeeds: the file is in
/// place and complete. It catches an error reported for a file that exists, which tells the
/// caller a good secret is lost.
#[test]
fn a_directory_that_cannot_be_synced_does_not_fail_a_file_that_is_in_place() {
    let dir = Scratch::new("create_new_unsynced");
    fs::create_dir(dir.path("d")).unwrap();
    fs::set_permissions(dir.path("d"), fs::Permissions::from_mode(0o300)).unwrap();
    let made = create_new(&dir.path("d").join("f"), b"kept", 0o600);
    fs::set_permissions(dir.path("d"), fs::Permissions::from_mode(0o700)).unwrap();
    made.unwrap();
    assert_eq!(fs::read(dir.path("d").join("f")).unwrap(), b"kept");
}

/// Asserts that `probe_dir` says whether a file can be made in a directory and linked, and
/// leaves nothing behind either way: it is Ok for an ordinary directory and an error for one
/// that is read-only, one that is not there, and one that is a file. It catches a probe that
/// leaves its files and one that passes a directory the real call would fail in.
#[test]
fn the_probe_tells_a_usable_directory_and_leaves_nothing() {
    let dir = Scratch::new("probe_dir");
    fs::create_dir(dir.path("ok")).unwrap();
    fs::create_dir(dir.path("ro")).unwrap();
    fs::set_permissions(dir.path("ro"), fs::Permissions::from_mode(0o500)).unwrap();
    dir.write("file", b"x", 0o600);
    probe_dir(&dir.path("ok")).unwrap();
    assert_eq!(fs::read_dir(dir.path("ok")).unwrap().count(), 0);
    let read_only = probe_dir(&dir.path("ro"));
    fs::set_permissions(dir.path("ro"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(read_only.is_err());
    assert_eq!(fs::read_dir(dir.path("ro")).unwrap().count(), 0);
    assert!(probe_dir(&dir.path("absent")).is_err());
    assert!(probe_dir(&dir.path("file")).is_err());
}
