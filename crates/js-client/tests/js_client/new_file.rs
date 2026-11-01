//! `create_new`: a file made whole or not at all, and never over another.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use js_client::create_new;

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

/// Asserts that a staged file left by an interrupted call is replaced and removed, and that a
/// missing directory is an error that names it and makes nothing. It catches a retry blocked
/// by a leftover, and a swallowed failure.
#[test]
fn a_leftover_is_replaced_and_a_missing_directory_is_an_error() {
    let dir = Scratch::new("create_new_leftover");
    dir.write(".f.new", b"old secret", 0o600);
    create_new(&dir.path("f"), b"new", 0o600).unwrap();
    assert_eq!(fs::read(dir.path("f")).unwrap(), b"new");
    assert!(!dir.path(".f.new").exists());

    let err = create_new(&dir.path("absent").join("f"), b"x", 0o600).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}
