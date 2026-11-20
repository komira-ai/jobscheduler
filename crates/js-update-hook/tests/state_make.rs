//! The making of a missing state directory, and of the missing directories above it, by
//! `Held::open_or_make`: what it makes, with which mode and group, what it refuses to make
//! over, and what `open` and `peek`, which make nothing, do instead.

mod state_support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use js_update_hook::{DirProblem, DirStat, Held, LOCK_FILE, RECORD_FILE, StateError, peek};
use state_support::{Note, TestReason, as_root, cell, dir_with_mode, open, rooted, seeing};

/// Whether a holder other than the step can take the directory's lock now.
fn directory_free(dir: &Path) -> bool {
    fs::File::open(dir).unwrap().try_lock().is_ok()
}

fn stat_of(is_symlink: bool, is_dir: bool, uid: u32, gid: u32, mode: u32) -> DirStat {
    DirStat {
        is_symlink,
        is_dir,
        uid,
        gid,
        mode,
    }
}

/// `Held::open_or_make` with the stat of a root-run hook.
fn open_or_make(dir: &Path) -> Result<Held<Note>, StateError<Note>> {
    Held::open_or_make(dir, &as_root)
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A state directory that is not there is made, a real directory of mode 0700, held to the
/// checks, locked, with no record in it: the first step of a first install. Catches a hook that refuses its first step because nothing made
/// its directory, and one that makes the directory open to others.
#[test]
fn open_or_make_makes_a_missing_state_directory_mode_0700_and_locks_it() {
    let tmp = cell("state-make-missing");
    let state = tmp.path().join("state");
    let held = open_or_make(&state).unwrap();
    let meta = fs::symlink_metadata(&state).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.permissions().mode() & 0o7777, 0o700);
    assert_eq!(names(&state), [LOCK_FILE]);
    assert!(held.load().unwrap().is_none());
    assert!(!directory_free(&state), "the directory it made is locked");
    // A second step meanwhile is refused by the lock, as for a directory that was there.
    let error = open_or_make(&state).unwrap_err();
    assert_eq!(error.reason(), TestReason::Locked);
}

/// A state directory that is there is opened as `open` opens it: its record kept, its mode and
/// its contents unchanged, nothing made.
#[test]
fn open_or_make_opens_a_state_directory_that_is_there_and_changes_nothing() {
    let tmp = cell("state-make-there");
    let state = tmp.path().join("state");
    dir_with_mode(&state, 0o700);
    state_support::write_record(&state, &state_support::record_of(3));
    let held = open_or_make(&state).unwrap();
    assert_eq!(
        held.load().unwrap(),
        Some(Note {
            schema: 3,
            name: "kept".to_owned()
        })
    );
    assert_eq!(names(&state), [LOCK_FILE, RECORD_FILE]);
    assert_eq!(
        fs::metadata(&state).unwrap().permissions().mode() & 0o7777,
        0o700
    );
}

/// The group the test runs in, which a directory the hook makes is given.
fn own_group() -> u32 {
    rustix::process::getegid().as_raw()
}

/// A scratch directory of its own of mode 0755, as a root-owned directory a hook's path passes
/// through is, so that [`rooted`] shows the checks what is made under it as it really is.
fn tree(name: &str) -> js_testkit::dir::TestDir {
    let tmp = cell(name);
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    tmp
}

/// The permission bits and the group of what is at `path`.
fn mode_and_group(path: &Path) -> (u32, u32) {
    let meta = fs::symlink_metadata(path).unwrap();
    assert!(meta.is_dir(), "{} is a directory", path.display());
    (
        meta.permissions().mode() & 0o7777,
        std::os::unix::fs::MetadataExt::gid(&meta),
    )
}

/// A first install into a root where no directory of the state directory's path is there (a
/// Mac's `/Library/Application Support/js-agent/update` where `js-agent` is not there either):
/// every one is made, those above mode 0755 and the state directory mode 0700, each in the
/// hook's group, and they pass the checks as they really are. Catches a hook that makes only the
/// last directory, and so halts its first step on a missing parent, and one that leaves a made
/// directory open to others or in a group the checks refuse.
#[test]
fn open_or_make_makes_every_missing_directory_of_the_path() {
    let tmp = tree("state-make-path");
    let top = tmp.path().join("js-agent");
    let state = top.join("deeper").join("update");
    let held = Held::<Note>::open_or_make(&state, &rooted(tmp.path())).unwrap();
    assert!(held.load().unwrap().is_none());
    assert_eq!(mode_and_group(&top), (0o755, own_group()));
    assert_eq!(mode_and_group(&top.join("deeper")), (0o755, own_group()));
    assert_eq!(mode_and_group(&state), (0o700, own_group()));
    assert_eq!(names(tmp.path()), ["js-agent"]);
    assert_eq!(names(&top), ["deeper"]);
    assert_eq!(names(&top.join("deeper")), ["update"]);
    assert_eq!(names(&state), [LOCK_FILE]);
    assert!(!directory_free(&state), "the directory it made is locked");
}

/// A make that was killed before its rename left its directory at the making name: the next one
/// takes it, gives it the state directory's mode and puts it at its name, so a crash never stops
/// the first install for good. Catches a hook that refuses forever after a crash in the make.
#[test]
fn open_or_make_finishes_a_directory_a_killed_make_left() {
    let tmp = tree("state-make-left");
    let state = tmp.path().join("state");
    dir_with_mode(&tmp.path().join(".state.making"), 0o777);
    Held::<Note>::open_or_make(&state, &rooted(tmp.path())).unwrap();
    assert_eq!(mode_and_group(&state), (0o700, own_group()));
    assert_eq!(names(tmp.path()), ["state"]);
}

/// Something other than a directory at the making name is not taken: the make is the file error
/// naming the state directory, and nothing appears at its name.
#[test]
fn open_or_make_refuses_a_file_at_the_making_name() {
    let tmp = cell("state-make-left-file");
    let state = tmp.path().join("state");
    fs::write(tmp.path().join(".state.making"), "x").unwrap();
    let error = open_or_make(&state).unwrap_err();
    assert!(
        matches!(&error, StateError::Io { path, .. } if *path == state),
        "{error:?}"
    );
    assert_eq!(error.reason(), TestReason::StateInvalid);
    assert_eq!(names(tmp.path()), [".state.making"]);
}

/// A made directory is in the hook's group even where its parent would give it another: a
/// Mac gives every new directory its parent's group (`admin` under `/Library/Application
/// Support`), as Linux does under a set-group-id parent, and the state directory's `root:root`
/// check would refuse it. The parent here is set-group-id and in another group the test runs
/// in. Catches a make that leaves the group a new directory inherits.
#[test]
fn a_made_directory_is_in_the_hooks_group_not_its_parents() {
    let Some(other) = rustix::process::getgroups()
        .unwrap()
        .into_iter()
        .find(|gid| *gid != rustix::process::getegid())
    else {
        eprintln!(
            "skipped: the test process is in no group but its own, so no parent can have another"
        );
        return;
    };
    let tmp = tree("state-make-group");
    let parent = tmp.path().join("parent");
    fs::create_dir(&parent).unwrap();
    rustix::fs::chown(&parent, None, Some(other)).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o2755)).unwrap();
    let state = parent.join("state");
    Held::<Note>::open_or_make(&state, &rooted(tmp.path())).unwrap();
    assert_eq!(mode_and_group(&state), (0o700, own_group()));
}

/// A link at the making name is not followed: the make is the file error naming the state
/// directory, nothing appears at its name, and the directory the link points to keeps its mode.
#[test]
fn open_or_make_refuses_a_link_at_the_making_name_and_follows_nothing() {
    let tmp = tree("state-make-left-link");
    let elsewhere = tmp.path().join("elsewhere");
    dir_with_mode(&elsewhere, 0o777);
    symlink(&elsewhere, tmp.path().join(".state.making")).unwrap();
    let state = tmp.path().join("state");
    let error = Held::<Note>::open_or_make(&state, &rooted(tmp.path())).unwrap_err();
    assert!(
        matches!(&error, StateError::Io { path, .. } if *path == state),
        "{error:?}"
    );
    assert!(!state.exists());
    assert_eq!(mode_and_group(&elsewhere).0, 0o777);
}

/// A directory of the path that is there and is not private is still refused, not made over or
/// changed: the state directory open to others, and a directory above it writable by others
/// with a missing state directory under it, which is not made.
#[test]
fn open_or_make_still_refuses_a_directory_that_is_there_and_not_private() {
    let tmp = tree("state-make-open-there");
    let state = tmp.path().join("state");
    dir_with_mode(&state, 0o755);
    let error = Held::<Note>::open_or_make(&state, &rooted(tmp.path())).unwrap_err();
    assert!(
        matches!(&error, StateError::Dir { path, problem: DirProblem::Mode { mode: 0o755 } } if *path == state),
        "{error:?}"
    );
    assert_eq!(error.reason(), TestReason::StateInvalid);
    assert_eq!(mode_and_group(&state).0, 0o755);
    assert!(names(&state).is_empty());

    let open = tmp.path().join("open");
    dir_with_mode(&open, 0o777);
    let error = Held::<Note>::open_or_make(&open.join("below").join("state"), &rooted(tmp.path()))
        .unwrap_err();
    assert!(
        matches!(&error, StateError::Ancestor { path, problem: DirProblem::Writable { mode: 0o777 } } if *path == open),
        "{error:?}"
    );
    assert!(names(&open).is_empty());
}

/// A directory above the state directory that cannot be made (its making name is longer than a
/// name may be) is the file error naming it, and nothing is made under it.
#[test]
fn a_directory_above_that_cannot_be_made_is_an_io_error_naming_it() {
    let tmp = cell("state-make-parent-refused");
    let parent = tmp.path().join("p".repeat(250));
    let error = open_or_make(&parent.join("state")).unwrap_err();
    assert!(
        matches!(&error, StateError::Io { path, source } if *path == parent && source.raw_os_error() == Some(rustix::io::Errno::NAMETOOLONG.raw_os_error())),
        "{error:?}"
    );
    assert!(names(tmp.path()).is_empty());
}

/// A parent the check refuses is refused before anything is made in it, naming the parent.
#[test]
fn open_or_make_makes_nothing_in_a_parent_the_check_refuses() {
    let tmp = cell("state-make-bad-parent");
    let parent = tmp.path().join("parent");
    fs::create_dir(&parent).unwrap();
    let stat = seeing(&parent, stat_of(false, true, 0, 0, 0o777));
    let error = Held::<Note>::open_or_make(&parent.join("state"), &stat).unwrap_err();
    assert!(
        matches!(&error, StateError::Ancestor { path, problem: DirProblem::Writable { mode: 0o777 } } if *path == parent),
        "{error:?}"
    );
    assert!(names(&parent).is_empty());
}

/// A link at the state directory's name, dangling or not, is refused as a link and not
/// followed: nothing is made where it points.
#[test]
fn open_or_make_refuses_a_link_in_the_place_of_the_state_directory_and_makes_nothing() {
    let tmp = cell("state-make-link");
    let link = tmp.path().join("state");
    let target = tmp.path().join("nowhere");
    symlink(&target, &link).unwrap();
    let error = open_or_make(&link).unwrap_err();
    assert!(
        matches!(&error, StateError::Dir { path, problem: DirProblem::Symlink } if *path == link),
        "{error:?}"
    );
    assert!(!target.exists());
    assert_eq!(names(tmp.path()), ["state"]);
}

/// A file at the state directory's name is refused as not a directory, and left as it was.
#[test]
fn open_or_make_refuses_a_file_in_the_place_of_the_state_directory() {
    let tmp = cell("state-make-file");
    let file = tmp.path().join("state");
    fs::write(&file, "x").unwrap();
    let error = open_or_make(&file).unwrap_err();
    assert!(
        matches!(&error, StateError::Dir { path, problem: DirProblem::NotDirectory } if *path == file),
        "{error:?}"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "x");
}

/// A state directory that cannot be made (its name is longer than a name may be) is the file
/// error naming it, with the system's words.
#[test]
fn a_state_directory_that_cannot_be_made_is_an_io_error_naming_it() {
    let tmp = cell("state-make-refused");
    let state = tmp.path().join("s".repeat(300));
    let error = open_or_make(&state).unwrap_err();
    assert!(
        matches!(&error, StateError::Io { path, source } if *path == state && source.raw_os_error() == Some(rustix::io::Errno::NAMETOOLONG.raw_os_error())),
        "{error:?}"
    );
    assert_eq!(error.reason(), TestReason::StateInvalid);
    assert!(names(tmp.path()).is_empty());
}

/// `open` and `peek` make no directory: a state directory that is not there stays so.
#[test]
fn open_and_peek_make_no_missing_state_directory() {
    let tmp = cell("state-open-makes-nothing");
    let state = tmp.path().join("state");
    assert!(matches!(open(&state), Err(StateError::Io { .. })));
    assert!(matches!(
        peek::<Note>(&state, &as_root),
        Err(StateError::Io { .. })
    ));
    assert!(names(tmp.path()).is_empty());
}
