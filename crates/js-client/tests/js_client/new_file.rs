//! `create_new` and `probe_dir`: a file made whole or not at all, and never over another.

use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;
use std::sync::Barrier;
use std::thread;

use js_client::{NewFileStep, check_file_path, create_new, probe_dir};

use crate::support::Scratch;

/// The sorted names in `dir`.
fn listing(dir: &Path) -> Vec<OsString> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    names
}

fn names(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

/// Makes the directory `path` that only its owner can write, whatever the umask.
fn mkdir(path: &Path) {
    fs::create_dir(path).unwrap();
    chmod(path, 0o700);
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Asserts that the file is made with the bytes and exactly the mode asked for, whatever the
/// process's umask takes off (0664 is more than the usual 022 allows), that it is a file of
/// its own (one link), and that the directory then holds exactly the two files and nothing
/// else. It catches a mode left to the umask, a file that is a second name of the staged
/// one, and a leftover.
#[test]
fn the_file_has_the_bytes_and_the_mode_and_nothing_is_left() {
    let dir = Scratch::new("create_new_ok");
    assert!(listing(&dir.path("")).is_empty());
    for (name, mode) in [("private", 0o600), ("shared", 0o664)] {
        let path = dir.path(name);
        create_new(&path, b"content\n", mode).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"content\n");
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, mode);
        assert_eq!(meta.nlink(), 1);
    }
    assert_eq!(listing(&dir.path("")), names(&["private", "shared"]));
}

/// Asserts that anything already at the path (a file, a link to nothing, a directory) is left
/// as it was and the error is `AlreadyExists` at the link step, with the path and not the
/// content in its text, and that the whole directory listing is the same after the call as
/// before it. It catches an overwrite, a link followed, and a staged file left behind.
#[test]
fn what_is_there_is_never_replaced() {
    let dir = Scratch::new("create_new_exists");
    let file = dir.write("file", b"keep", 0o600);
    symlink(dir.path("nothing"), dir.path("dangling")).unwrap();
    fs::create_dir(dir.path("dir")).unwrap();
    let before = listing(&dir.path(""));
    for name in ["file", "dangling", "dir"] {
        let err = create_new(&dir.path(name), b"SECRET-CONTENT", 0o600).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists, "{name}");
        assert_eq!(err.step(), NewFileStep::Link, "{name}");
        assert_eq!(err.path(), dir.path(name), "{name}");
        let text = err.to_string();
        assert!(text.contains(dir.path(name).to_str().unwrap()), "{text}");
        assert!(!text.contains("SECRET"), "{name}: {text}");
        assert_eq!(listing(&dir.path("")), before, "{name}");
    }
    assert_eq!(fs::read(file).unwrap(), b"keep");
    assert!(!dir.path("nothing").exists());
    assert!(fs::metadata(dir.path("dir")).unwrap().is_dir());
}

/// Asserts that names an earlier version staged under (`.f.new`), as a directory or a file,
/// do not stop the call and are not touched, and that the listing afterwards is those and
/// the new files only. It catches a staging name that another user can guess and plant.
#[test]
fn the_staging_name_cannot_be_guessed() {
    let dir = Scratch::new("create_new_guessed");
    fs::create_dir(dir.path(".f.new")).unwrap();
    dir.write(".g.new", b"theirs", 0o644);
    create_new(&dir.path("f"), b"new", 0o600).unwrap();
    create_new(&dir.path("g"), b"new", 0o600).unwrap();
    assert_eq!(fs::read(dir.path("f")).unwrap(), b"new");
    assert_eq!(fs::read(dir.path(".g.new")).unwrap(), b"theirs");
    assert!(dir.path(".f.new").is_dir());
    assert_eq!(
        listing(&dir.path("")),
        names(&[".f.new", ".g.new", "f", "g"])
    );
}

/// Asserts that a directory where the staged file cannot be created (read-only) and one that
/// is not there are errors (the staging step with `PermissionDenied`; the examine step with
/// `NotFound`, since the directory is opened first) with the listing unchanged, and that a
/// missing directory makes nothing. It catches an error of
/// another step, a half-made file and a directory made on the way.
#[test]
fn a_directory_that_cannot_take_the_file_is_an_error_and_makes_nothing() {
    let dir = Scratch::new("create_new_unwritable");
    mkdir(&dir.path("ro"));
    chmod(&dir.path("ro"), 0o500);
    let outer = listing(&dir.path(""));
    let denied = create_new(&dir.path("ro").join("f"), b"x", 0o600);
    chmod(&dir.path("ro"), 0o700);
    let err = denied.unwrap_err();
    assert_eq!(
        (err.step(), err.kind()),
        (NewFileStep::Stage, ErrorKind::PermissionDenied)
    );
    assert!(listing(&dir.path("ro")).is_empty());

    let err = create_new(&dir.path("absent").join("f"), b"x", 0o600).unwrap_err();
    assert_eq!(
        (err.step(), err.kind()),
        (NewFileStep::Inspect, ErrorKind::NotFound)
    );
    assert_eq!(listing(&dir.path("")), outer);
}

/// Asserts that when the link is made and only the sync of the directory fails (a directory
/// that can be written and searched but not read, 0300) the call succeeds: the file is in
/// place and complete. It catches an error reported for a file that exists, which tells the
/// caller a good secret is lost.
#[test]
fn a_directory_that_cannot_be_synced_does_not_fail_a_file_that_is_in_place() {
    let dir = Scratch::new("create_new_unsynced");
    mkdir(&dir.path("d"));
    chmod(&dir.path("d"), 0o300);
    let made = create_new(&dir.path("d").join("f"), b"kept", 0o600);
    chmod(&dir.path("d"), 0o700);
    made.unwrap();
    assert_eq!(fs::read(dir.path("d").join("f")).unwrap(), b"kept");
    assert_eq!(listing(&dir.path("d")), names(&["f"]));
}

/// Asserts that a destination whose name is 255 bytes (the most a name can be) is made, and
/// that one of 256 bytes is an error at the link step that leaves the directory as it was. It
/// catches a staging name built from the destination's, which would be too long for the
/// system when the destination is not.
#[test]
fn the_staging_name_does_not_depend_on_the_length_of_the_destination() {
    let dir = Scratch::new("create_new_long");
    let longest = "n".repeat(255);
    create_new(&dir.path(&longest), b"long", 0o600).unwrap();
    assert_eq!(listing(&dir.path("")), names(&[&longest]));
    assert_eq!(fs::read(dir.path(&longest)).unwrap(), b"long");

    let too_long = "m".repeat(256);
    let err = create_new(&dir.path(&too_long), b"x", 0o600).unwrap_err();
    assert_eq!(err.step(), NewFileStep::Link);
    assert_eq!(listing(&dir.path("")), names(&[&longest]));
}

/// Asserts that a path that names no file (empty, ending in a separator, `.` or `..`) is
/// refused at the path step as invalid input by `check_file_path` and by `create_new`, which
/// makes nothing, and that an ordinary relative name is fine. It catches a directory name
/// taken for a file name and an empty path taken for the current directory.
#[test]
fn a_path_that_names_no_file_is_refused() {
    let dir = Scratch::new("create_new_paths");
    mkdir(&dir.path("sub"));
    let before = listing(&dir.path(""));
    let trailing = format!("{}/", dir.path("sub").display());
    let dotted = format!("{}/.", dir.path("sub").display());
    for bad in ["", "..", "/", &trailing, &dotted] {
        let err = check_file_path(Path::new(bad)).unwrap_err();
        assert_eq!(
            (err.step(), err.kind()),
            (NewFileStep::Path, ErrorKind::InvalidInput),
            "{bad:?}"
        );
        let err = create_new(Path::new(bad), b"x", 0o600).unwrap_err();
        assert_eq!(err.step(), NewFileStep::Path, "{bad:?}");
        assert_eq!(listing(&dir.path("")), before, "{bad:?}");
        assert!(listing(&dir.path("sub")).is_empty(), "{bad:?}");
    }
    check_file_path(Path::new("name")).unwrap();
    check_file_path(&dir.path("name")).unwrap();
}

/// Asserts that two threads calling `create_new` on one path at the same moment (released
/// together by a barrier) give exactly one `Ok` and one `AlreadyExists` at the link step, that
/// the file holds the winner's bytes with the winner's mode, and that nothing else is left in
/// the directory, over many rounds. It catches a check-then-create that lets both succeed or
/// mixes their bytes, and a loser that leaves its staged file.
#[test]
fn two_calls_at_once_make_one_file_and_one_loses() {
    let dir = Scratch::new("create_new_race");
    for round in 0..50 {
        let path = dir.path(&format!("target-{round}"));
        let barrier = Barrier::new(2);
        let results: Vec<_> = thread::scope(|scope| {
            let handles = [
                (b"first".as_slice(), 0o600),
                (b"second-longer".as_slice(), 0o640),
            ]
            .map(|(bytes, mode)| {
                let (path, barrier) = (&path, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    (bytes, mode, create_new(path, bytes, mode))
                })
            });
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        let wins: Vec<_> = results.iter().filter(|r| r.2.is_ok()).collect();
        let losses: Vec<_> = results.iter().filter(|r| r.2.is_err()).collect();
        assert_eq!((wins.len(), losses.len()), (1, 1), "round {round}");
        let err = losses[0].2.as_ref().unwrap_err();
        assert_eq!(
            (err.kind(), err.step()),
            (ErrorKind::AlreadyExists, NewFileStep::Link)
        );
        let (bytes, mode, _) = wins[0];
        assert_eq!(&fs::read(&path).unwrap(), bytes, "round {round}");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            *mode
        );
        fs::remove_file(&path).unwrap();
        assert!(listing(&dir.path("")).is_empty(), "round {round}");
    }
}

/// Asserts that `probe_dir` says whether a file can be made in a directory and linked, and
/// leaves nothing behind either way: it is Ok for an ordinary directory and an error for one
/// that is read-only (the probe step), one that is not there (the inspect step), and one that
/// is a file. It catches a probe that leaves its files and one that passes a directory the
/// real call would fail in.
#[test]
fn the_probe_tells_a_usable_directory_and_leaves_nothing() {
    let dir = Scratch::new("probe_dir");
    mkdir(&dir.path("ok"));
    mkdir(&dir.path("ro"));
    chmod(&dir.path("ro"), 0o500);
    dir.write("file", b"x", 0o600);
    probe_dir(&dir.path("ok")).unwrap();
    assert!(listing(&dir.path("ok")).is_empty());
    let read_only = probe_dir(&dir.path("ro"));
    chmod(&dir.path("ro"), 0o700);
    assert_eq!(read_only.unwrap_err().step(), NewFileStep::Probe);
    assert!(listing(&dir.path("ro")).is_empty());
    assert_eq!(
        probe_dir(&dir.path("absent")).unwrap_err().step(),
        NewFileStep::Inspect
    );
    assert!(probe_dir(&dir.path("file")).is_err());
    assert_eq!(listing(&dir.path("")), names(&["file", "ok", "ro"]));
}

/// Asserts that a directory group or others can write is refused by `probe_dir` at the
/// unsafe step as `PermissionDenied`, with the directory and the mode in the text, whether
/// it is 0777, 0775, 0757 or 0770, and that nothing is made in it; and that the same with the
/// sticky bit (01777), or only the owner writing (0755), is accepted. It catches a directory
/// where another user could swap the staged file before it is linked, and a refusal of
/// `/tmp`-style directories.
#[test]
fn a_directory_others_can_write_is_refused_unless_it_is_sticky() {
    let dir = Scratch::new("probe_unsafe");
    mkdir(&dir.path("d"));
    for mode in [0o777, 0o775, 0o757, 0o770] {
        chmod(&dir.path("d"), mode);
        let err = probe_dir(&dir.path("d")).unwrap_err();
        assert_eq!(
            (err.step(), err.kind()),
            (NewFileStep::Unsafe, ErrorKind::PermissionDenied),
            "{mode:o}"
        );
        let text = err.to_string();
        assert!(text.contains(dir.path("d").to_str().unwrap()), "{text}");
        assert!(text.contains(&format!("mode {mode:04o}")), "{text}");
        assert!(listing(&dir.path("d")).is_empty(), "{mode:o}");
    }
    for mode in [0o1777, 0o755, 0o700] {
        chmod(&dir.path("d"), mode);
        probe_dir(&dir.path("d")).unwrap();
        assert!(listing(&dir.path("d")).is_empty(), "{mode:o}");
    }
}
