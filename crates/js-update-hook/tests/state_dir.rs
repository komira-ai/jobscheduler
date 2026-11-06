//! The state directory of a hook: the check of the directory and of every directory above it,
//! the walk down from `/`, the two locks and their release, and the messages and reasons of
//! the errors.

mod state_support;

use std::error::Error as _;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use js_update_hook::{
    DirProblem, DirStat, Held, LOCK_FILE, RECORD_FILE, StateError, check_ancestor, check_dir,
    fd_stat, peek,
};
use state_support::{
    Note, NoteInvalid, ROOT_0700, TestReason, as_root, cell, dir_with_mode, failing_at, fifo, open,
    rooted, seeing,
};

/// Whether a holder other than the step can take the directory's lock now.
fn directory_free(dir: &Path) -> bool {
    File::open(dir).unwrap().try_lock().is_ok()
}

/// Whether a holder other than the step can take the lock file's lock now.
fn lock_file_free(dir: &Path) -> bool {
    File::options()
        .write(true)
        .open(dir.join(LOCK_FILE))
        .unwrap()
        .try_lock()
        .is_ok()
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

/// The descriptor of a directory is looked at as the directory it is: a directory, never a
/// link, with the owner and the permission bits only (not the file type bits).
#[test]
fn fd_stat_reports_a_directory_with_its_owner_and_permission_bits() {
    let tmp = cell("state-fd-stat-dir");
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o750)).unwrap();
    let meta = fs::metadata(tmp.path()).unwrap();
    let dir = File::open(tmp.path()).unwrap();
    let seen = fd_stat(dir.as_fd()).unwrap();
    assert_eq!(
        seen,
        stat_of(
            false,
            true,
            std::os::unix::fs::MetadataExt::uid(&meta),
            std::os::unix::fs::MetadataExt::gid(&meta),
            0o750
        )
    );
}

/// A descriptor of a regular file is reported as not a directory.
#[test]
fn fd_stat_reports_a_regular_file_as_not_a_directory() {
    let tmp = cell("state-fd-stat-file");
    let path = tmp.path().join("plain");
    fs::write(&path, "x").unwrap();
    let file = File::open(&path).unwrap();
    let seen = fd_stat(file.as_fd()).unwrap();
    assert!(!seen.is_dir);
    assert!(!seen.is_symlink);
}

/// An ancestor is refused for the first thing wrong with it, in the order link, not a
/// directory, not root's, writable by group or others; a root-owned directory nobody else can
/// write passes, including one that is not 0700.
#[test]
fn an_ancestor_is_refused_for_the_first_thing_wrong_with_it() {
    let cases = [
        (stat_of(true, true, 0, 0, 0o755), Err(DirProblem::Symlink)),
        (stat_of(true, false, 5, 5, 0o777), Err(DirProblem::Symlink)),
        (
            stat_of(false, false, 5, 5, 0o777),
            Err(DirProblem::NotDirectory),
        ),
        (
            stat_of(false, true, 1000, 0, 0o777),
            Err(DirProblem::NotRootOwned { uid: 1000 }),
        ),
        (
            stat_of(false, true, 0, 0, 0o775),
            Err(DirProblem::Writable { mode: 0o775 }),
        ),
        (
            stat_of(false, true, 0, 0, 0o757),
            Err(DirProblem::Writable { mode: 0o757 }),
        ),
        (stat_of(false, true, 0, 0, 0o755), Ok(())),
        (stat_of(false, true, 0, 1000, 0o700), Ok(())),
    ];
    for (seen, want) in cases {
        assert_eq!(check_ancestor(&seen), want, "{seen:?}");
    }
}

/// The state directory itself is refused for the first thing wrong with it, in the order link,
/// not a directory, owner, mode; only root:root 0700 passes.
#[test]
fn the_state_directory_is_refused_for_the_first_thing_wrong_with_it() {
    let cases = [
        (stat_of(true, false, 5, 5, 0o755), Err(DirProblem::Symlink)),
        (
            stat_of(false, false, 5, 5, 0o755),
            Err(DirProblem::NotDirectory),
        ),
        (
            stat_of(false, true, 1000, 0, 0o755),
            Err(DirProblem::Owner { uid: 1000, gid: 0 }),
        ),
        (
            stat_of(false, true, 0, 1000, 0o700),
            Err(DirProblem::Owner { uid: 0, gid: 1000 }),
        ),
        (
            stat_of(false, true, 0, 0, 0o755),
            Err(DirProblem::Mode { mode: 0o755 }),
        ),
        (
            stat_of(false, true, 0, 0, 0o70),
            Err(DirProblem::Mode { mode: 0o70 }),
        ),
        (stat_of(false, true, 0, 0, 0o700), Ok(())),
    ];
    for (seen, want) in cases {
        assert_eq!(check_dir(&seen), want, "{seen:?}");
    }
}

/// Every reason a directory is refused reads as the sentence the step's last line carries.
#[test]
fn every_directory_problem_reads_as_its_own_sentence() {
    let cases = [
        (DirProblem::Symlink, "is a symbolic link"),
        (DirProblem::NotDirectory, "is not a directory"),
        (
            DirProblem::Owner { uid: 7, gid: 8 },
            "is owned by 7:8, not 0:0",
        ),
        (DirProblem::Mode { mode: 0o755 }, "has mode 0755, not 0700"),
        (
            DirProblem::NotPlain,
            "is not an absolute path of plain components",
        ),
        (
            DirProblem::NotRootOwned { uid: 1000 },
            "is owned by 1000, not by root",
        ),
        (
            DirProblem::Writable { mode: 0o775 },
            "has mode 0775, writable by its group or by others",
        ),
    ];
    for (problem, text) in cases {
        assert_eq!(problem.to_string(), text);
    }
}

/// Every error of the state reads as one line naming what and where, and says which of the two
/// reasons the step gives: the lock's for a second holder, the state's for everything else.
#[test]
fn every_state_error_reads_as_one_line_and_gives_its_reason() {
    let lock: StateError<Note> = StateError::Lock {
        path: "/s/lock".into(),
        source: io::Error::from(io::ErrorKind::WouldBlock),
    };
    assert_eq!(
        lock.to_string(),
        "cannot lock /s/lock: operation would block"
    );
    assert_eq!(lock.reason(), TestReason::Locked);

    let others: Vec<(StateError<Note>, &str)> = vec![
        (
            StateError::Dir {
                path: "/s".into(),
                problem: DirProblem::Mode { mode: 0o755 },
            },
            "state directory /s: has mode 0755, not 0700",
        ),
        (
            StateError::Ancestor {
                path: "/a".into(),
                problem: DirProblem::NotRootOwned { uid: 9 },
            },
            "above the state directory, /a: is owned by 9, not by root",
        ),
        (
            StateError::Io {
                path: "/s/x".into(),
                source: io::Error::other("boom"),
            },
            "/s/x: boom",
        ),
        (
            StateError::Corrupt("bad bytes".to_owned()),
            "the record is not readable: bad bytes",
        ),
        (
            StateError::Newer { found: 4 },
            "the record has schema 4, newer than 3: read-only to this hook",
        ),
        (
            StateError::Invalid(NoteInvalid("name is empty")),
            "the record is not valid: name is empty",
        ),
    ];
    for (error, text) in others {
        assert_eq!(error.to_string(), text);
        assert_eq!(error.reason(), TestReason::StateInvalid, "{text}");
    }
}

/// The cause of an error is its underlying one: the hook's own error for an invalid record,
/// the system's for a lock and for a file operation, and none for the rest.
#[test]
fn the_source_of_an_error_is_the_error_underneath() {
    let invalid: StateError<Note> = StateError::Invalid(NoteInvalid("name is empty"));
    let inner = invalid.source().unwrap().downcast_ref::<NoteInvalid>();
    assert_eq!(inner, Some(&NoteInvalid("name is empty")));

    let lock: StateError<Note> = StateError::Lock {
        path: "/s".into(),
        source: io::Error::from(io::ErrorKind::WouldBlock),
    };
    let cause = lock.source().unwrap().downcast_ref::<io::Error>().unwrap();
    assert_eq!(cause.kind(), io::ErrorKind::WouldBlock);

    let io_error: StateError<Note> = StateError::Io {
        path: "/s".into(),
        source: io::Error::from(io::ErrorKind::NotFound),
    };
    let cause = io_error
        .source()
        .unwrap()
        .downcast_ref::<io::Error>()
        .unwrap();
    assert_eq!(cause.kind(), io::ErrorKind::NotFound);

    let corrupt: StateError<Note> = StateError::Corrupt("x".to_owned());
    assert!(corrupt.source().is_none());
    let newer: StateError<Note> = StateError::Newer { found: 4 };
    assert!(newer.source().is_none());
}

/// A good directory opens; the lock file is made in it, the record is not, and the record's
/// path is the directory's `record.json`.
#[test]
fn a_good_directory_opens_and_makes_only_its_lock_file() {
    let tmp = cell("state-open-ok");
    let held = open(tmp.path()).unwrap();
    assert_eq!(held.record_path(), tmp.path().join(RECORD_FILE));
    assert!(tmp.path().join(LOCK_FILE).is_file());
    assert!(!tmp.path().join(RECORD_FILE).exists());
}

/// A directory with the real mode 0700, owned by root as the fake says, under a real parent
/// that is 0755, opens when the walk looks at what is really there.
#[test]
fn a_real_0700_directory_under_a_0755_parent_opens() {
    let tmp = cell("state-open-real");
    dir_with_mode(tmp.path(), 0o755);
    let state = tmp.path().join("parent").join("state");
    dir_with_mode(&state, 0o700);
    dir_with_mode(&tmp.path().join("parent"), 0o755);
    let stat = rooted(tmp.path());
    let held = Held::<Note>::open(&state, &stat).unwrap();
    assert_eq!(held.record_path(), state.join(RECORD_FILE));
}

/// The real mode is what the check sees: a state directory that is 0755 is refused with the
/// mode, naming the directory.
#[test]
fn a_real_0755_state_directory_is_refused_for_its_mode() {
    let tmp = cell("state-open-mode");
    dir_with_mode(tmp.path(), 0o755);
    let state = tmp.path().join("state");
    dir_with_mode(&state, 0o755);
    let stat = rooted(tmp.path());
    let error = Held::<Note>::open(&state, &stat).unwrap_err();
    assert!(
        matches!(&error, StateError::Dir { path, problem: DirProblem::Mode { mode: 0o755 } } if *path == state),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "state directory {}: has mode 0755, not 0700",
            state.display()
        )
    );
    assert_eq!(error.reason(), TestReason::StateInvalid);
}

/// A real parent that its group can write is refused as an ancestor, naming the parent.
#[test]
fn a_real_parent_writable_by_its_group_is_refused() {
    let tmp = cell("state-open-writable-parent");
    let parent = tmp.path().join("parent");
    let state = parent.join("state");
    dir_with_mode(&state, 0o700);
    dir_with_mode(&parent, 0o775);
    let stat = |path: &Path, fd: std::os::fd::BorrowedFd<'_>| {
        // Everything above the parent as root's 0755; the parent and below as they are.
        if path.starts_with(&parent) {
            Ok(DirStat {
                uid: 0,
                gid: 0,
                ..fd_stat(fd)?
            })
        } else {
            Ok(DirStat {
                mode: 0o755,
                ..ROOT_0700
            })
        }
    };
    let error = Held::<Note>::open(&state, &stat).unwrap_err();
    assert!(
        matches!(&error, StateError::Ancestor { path, problem: DirProblem::Writable { mode: 0o775 } } if *path == parent),
        "{error:?}"
    );
}

/// A path that is not absolute and plain is refused before anything is opened: relative, with
/// `..` or `.`, with a doubled or a trailing slash. The stat is never asked.
#[test]
fn a_path_that_is_not_plain_is_refused_before_anything_is_opened() {
    let never = |_: &Path, _: std::os::fd::BorrowedFd<'_>| -> io::Result<DirStat> {
        panic!("the walk must not start");
    };
    for bad in [
        "state",
        "/var/../state",
        "/var/./state",
        "/var//state",
        "/var/state/",
    ] {
        let error = Held::<Note>::open(Path::new(bad), &never).unwrap_err();
        assert!(
            matches!(&error, StateError::Dir { path, problem: DirProblem::NotPlain } if path == Path::new(bad)),
            "{bad}: {error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("state directory {bad}: is not an absolute path of plain components")
        );
    }
}

/// A link in place of a directory above the state directory is refused as an ancestor
/// (the link's own path is named), not followed.
#[test]
fn a_symbolic_link_above_the_state_directory_is_refused_as_an_ancestor() {
    let tmp = cell("state-open-ancestor-link");
    fs::create_dir_all(tmp.path().join("real").join("state")).unwrap();
    let link = tmp.path().join("link");
    symlink(tmp.path().join("real"), &link).unwrap();
    let error = open(&link.join("state")).unwrap_err();
    assert!(
        matches!(&error, StateError::Ancestor { path, problem: DirProblem::Symlink } if *path == link),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "above the state directory, {}: is a symbolic link",
            link.display()
        )
    );
}

/// A regular file in place of a directory above the state directory is refused as an
/// ancestor that is not a directory.
#[test]
fn a_file_above_the_state_directory_is_refused_as_an_ancestor() {
    let tmp = cell("state-open-ancestor-file");
    let file = tmp.path().join("file");
    fs::write(&file, "x").unwrap();
    let error = open(&file.join("state")).unwrap_err();
    assert!(
        matches!(&error, StateError::Ancestor { path, problem: DirProblem::NotDirectory } if *path == file),
        "{error:?}"
    );
}

/// A link as the state directory itself is refused as the state directory, not followed.
#[test]
fn a_symbolic_link_as_the_state_directory_is_refused() {
    let tmp = cell("state-open-last-link");
    fs::create_dir(tmp.path().join("real")).unwrap();
    let link = tmp.path().join("state");
    symlink(tmp.path().join("real"), &link).unwrap();
    let error = open(&link).unwrap_err();
    assert!(
        matches!(&error, StateError::Dir { path, problem: DirProblem::Symlink } if *path == link),
        "{error:?}"
    );
    assert!(!tmp.path().join("real").join(LOCK_FILE).exists());
}

/// A dangling link as the state directory is still a link, not a missing directory.
#[test]
fn a_dangling_link_as_the_state_directory_is_refused_as_a_link() {
    let tmp = cell("state-open-last-dangling");
    let link = tmp.path().join("state");
    symlink(tmp.path().join("nowhere"), &link).unwrap();
    let error = open(&link).unwrap_err();
    assert!(
        matches!(
            &error,
            StateError::Dir {
                problem: DirProblem::Symlink,
                ..
            }
        ),
        "{error:?}"
    );
}

/// A regular file as the state directory is refused as not a directory.
#[test]
fn a_file_as_the_state_directory_is_refused() {
    let tmp = cell("state-open-last-file");
    let file = tmp.path().join("state");
    fs::write(&file, "x").unwrap();
    let error = open(&file).unwrap_err();
    assert!(
        matches!(&error, StateError::Dir { path, problem: DirProblem::NotDirectory } if *path == file),
        "{error:?}"
    );
}

/// A directory that is not there is a file error naming it (not a verdict on the directory),
/// whether it is the state directory or a directory above it; the walk stops there.
#[test]
fn a_directory_that_does_not_exist_is_an_io_error_naming_it() {
    let tmp = cell("state-open-missing");
    let last = tmp.path().join("state");
    let error = open(&last).unwrap_err();
    assert!(
        matches!(&error, StateError::Io { path, source } if *path == last && source.kind() == io::ErrorKind::NotFound),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!("{}: No such file or directory (os error 2)", last.display())
    );
    assert_eq!(error.reason(), TestReason::StateInvalid);

    let above = tmp.path().join("gone");
    let error = open(&above.join("state")).unwrap_err();
    assert!(
        matches!(&error, StateError::Io { path, source } if *path == above && source.kind() == io::ErrorKind::NotFound),
        "{error:?}"
    );
}

/// An ancestor the stat finds wrong is refused with the check's reason, naming that ancestor
/// and not the state directory: not root's, writable, not a directory, a link.
#[test]
fn an_ancestor_the_stat_finds_wrong_is_refused_naming_it() {
    let tmp = cell("state-open-stat-ancestor");
    let state = tmp.path().join("state");
    fs::create_dir(&state).unwrap();
    let cases = [
        (
            stat_of(false, true, 1000, 0, 0o755),
            DirProblem::NotRootOwned { uid: 1000 },
        ),
        (
            stat_of(false, true, 0, 0, 0o777),
            DirProblem::Writable { mode: 0o777 },
        ),
        (stat_of(false, false, 0, 0, 0o755), DirProblem::NotDirectory),
        (stat_of(true, true, 0, 0, 0o755), DirProblem::Symlink),
    ];
    for (seen, want) in cases {
        let stat = seeing(tmp.path(), seen);
        let error = Held::<Note>::open(&state, &stat).unwrap_err();
        assert!(
            matches!(&error, StateError::Ancestor { path, problem } if path == tmp.path() && *problem == want),
            "{seen:?}: {error:?}"
        );
    }
}

/// The state directory the stat finds wrong is refused as the state directory: the wrong
/// owner, the wrong mode, a link, not a directory.
#[test]
fn a_state_directory_the_stat_finds_wrong_is_refused_naming_it() {
    let tmp = cell("state-open-stat-last");
    let cases = [
        (
            stat_of(false, true, 1000, 0, 0o700),
            DirProblem::Owner { uid: 1000, gid: 0 },
        ),
        (
            stat_of(false, true, 0, 0, 0o777),
            DirProblem::Mode { mode: 0o777 },
        ),
        (stat_of(true, true, 0, 0, 0o700), DirProblem::Symlink),
        (stat_of(false, false, 0, 0, 0o700), DirProblem::NotDirectory),
    ];
    for (seen, want) in cases {
        let stat = seeing(tmp.path(), seen);
        let error = Held::<Note>::open(tmp.path(), &stat).unwrap_err();
        assert!(
            matches!(&error, StateError::Dir { path, problem } if path == tmp.path() && *problem == want),
            "{seen:?}: {error:?}"
        );
    }
    assert!(!tmp.path().join(LOCK_FILE).exists());
}

/// A stat that fails is a file error naming the path it failed at: at `/` when the first look
/// fails, and further down when a later one does.
#[test]
fn a_stat_that_fails_is_an_io_error_naming_where_it_failed() {
    let tmp = cell("state-open-stat-error");
    let error = Held::<Note>::open(tmp.path(), &failing_at(Path::new("/"))).unwrap_err();
    assert!(
        matches!(&error, StateError::Io { path, .. } if path == Path::new("/")),
        "{error:?}"
    );
    assert_eq!(error.to_string(), "/: stat refused");

    let error = Held::<Note>::open(tmp.path(), &failing_at(tmp.path())).unwrap_err();
    assert!(
        matches!(&error, StateError::Io { path, .. } if path == tmp.path()),
        "{error:?}"
    );
}

/// The directory held by someone else refuses the step with the lock error naming the
/// directory, before anything is made in it; once the holder lets go, the step opens.
#[test]
fn a_directory_held_elsewhere_refuses_the_step_before_anything_is_made_in_it() {
    let tmp = cell("state-lock-dir-held");
    let other = File::open(tmp.path()).unwrap();
    other.try_lock().unwrap();
    let error = open(tmp.path()).unwrap_err();
    assert!(
        matches!(&error, StateError::Lock { path, source } if path == tmp.path() && source.kind() == io::ErrorKind::WouldBlock),
        "{error:?}"
    );
    assert_eq!(error.reason(), TestReason::Locked);
    assert_eq!(
        error.to_string(),
        format!(
            "cannot lock {}: Resource temporarily unavailable (os error 11)",
            tmp.path().display()
        )
    );
    assert!(!tmp.path().join(LOCK_FILE).exists());
    drop(other);
    open(tmp.path()).unwrap();
}

/// The lock file held by someone else (a hook built before the directory lock) refuses the
/// step with the lock error naming the lock file, and the directory's lock taken first is
/// released again.
#[test]
fn a_lock_file_held_elsewhere_refuses_the_step_and_releases_the_directory() {
    let tmp = cell("state-lock-file-held");
    let old = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(tmp.path().join(LOCK_FILE))
        .unwrap();
    old.try_lock().unwrap();
    let error = open(tmp.path()).unwrap_err();
    let lock_path = tmp.path().join(LOCK_FILE);
    assert!(
        matches!(&error, StateError::Lock { path, source } if *path == lock_path && source.kind() == io::ErrorKind::WouldBlock),
        "{error:?}"
    );
    assert_eq!(error.reason(), TestReason::Locked);
    assert!(directory_free(tmp.path()), "the directory is released");
    drop(old);
    open(tmp.path()).unwrap();
}

/// While the step holds the directory, a hook that takes only the lock file is kept out, and
/// a holder of the directory is kept out.
#[test]
fn a_step_holds_both_locks_while_it_lives() {
    let tmp = cell("state-lock-both");
    let held = open(tmp.path()).unwrap();
    assert!(!directory_free(tmp.path()));
    assert!(!lock_file_free(tmp.path()));
    drop(held);
}

/// Dropping the step releases both locks, so a second open succeeds and another holder can
/// take either lock.
#[test]
fn dropping_the_step_releases_both_locks() {
    let tmp = cell("state-lock-drop");
    let held = open(tmp.path()).unwrap();
    let second = open(tmp.path()).unwrap_err();
    assert!(matches!(second, StateError::Lock { .. }), "{second:?}");
    drop(held);
    assert!(directory_free(tmp.path()));
    assert!(lock_file_free(tmp.path()));
    open(tmp.path()).unwrap();
}

/// A lock file that cannot be opened as one fails the step as a file error naming the lock
/// file (not as a second holder) and releases the directory: a directory, a link, a pipe nobody
/// reads, and a pipe somebody reads (which opens, and is then not a regular file).
#[test]
fn a_lock_file_that_is_not_a_regular_file_fails_the_step_and_releases_the_directory() {
    let kinds = ["dir", "link", "fifo-unread", "fifo-read"];
    for kind in kinds {
        let tmp = cell(&format!("state-lock-odd-{kind}"));
        let lock = tmp.path().join(LOCK_FILE);
        let mut reader = None;
        match kind {
            "dir" => fs::create_dir(&lock).unwrap(),
            "link" => symlink("/nonexistent-target", &lock).unwrap(),
            "fifo-unread" => {
                fifo(&lock);
            }
            _ => {
                fifo(&lock);
                reader = Some(
                    File::options()
                        .read(true)
                        .custom_flags(0o4000)
                        .open(&lock)
                        .unwrap(),
                );
            }
        }
        let error = open(tmp.path()).unwrap_err();
        assert!(
            matches!(&error, StateError::Io { path, .. } if *path == lock),
            "{kind}: {error:?}"
        );
        if kind == "fifo-read" {
            assert_eq!(
                error.to_string(),
                format!("{}: not a regular file", lock.display())
            );
        }
        assert_eq!(error.reason(), TestReason::StateInvalid, "{kind}");
        assert!(directory_free(tmp.path()), "{kind}: directory released");
        drop(reader);
    }
}

/// Reading the record without the lock refuses a directory as `open` does: not plain, a link
/// above, a wrong owner at the end.
#[test]
fn peek_refuses_a_directory_as_open_does() {
    let tmp = cell("state-peek-dir-errors");
    let never = |_: &Path, _: std::os::fd::BorrowedFd<'_>| -> io::Result<DirStat> {
        panic!("the walk must not start");
    };
    let error = peek::<Note>(Path::new("relative"), &never).unwrap_err();
    assert!(
        matches!(
            &error,
            StateError::Dir {
                problem: DirProblem::NotPlain,
                ..
            }
        ),
        "{error:?}"
    );

    fs::create_dir_all(tmp.path().join("real").join("state")).unwrap();
    let link = tmp.path().join("link");
    symlink(tmp.path().join("real"), &link).unwrap();
    let error = peek::<Note>(&link.join("state"), &as_root).unwrap_err();
    assert!(
        matches!(&error, StateError::Ancestor { path, problem: DirProblem::Symlink } if *path == link),
        "{error:?}"
    );

    let stat = seeing(tmp.path(), stat_of(false, true, 1000, 0, 0o700));
    let error = peek::<Note>(tmp.path(), &stat).unwrap_err();
    assert!(
        matches!(
            &error,
            StateError::Dir {
                problem: DirProblem::Owner { uid: 1000, gid: 0 },
                ..
            }
        ),
        "{error:?}"
    );

    let error = peek::<Note>(&tmp.path().join("missing"), &as_root).unwrap_err();
    assert!(matches!(&error, StateError::Io { .. }), "{error:?}");

    let error = peek::<Note>(tmp.path(), &failing_at(Path::new("/"))).unwrap_err();
    assert!(matches!(&error, StateError::Io { .. }), "{error:?}");
}

/// Reading the record without the lock takes no lock and makes nothing: no lock file appears.
#[test]
fn peek_makes_nothing_in_the_directory() {
    let tmp = cell("state-peek-no-lock-file");
    assert!(peek::<Note>(tmp.path(), &as_root).unwrap().is_none());
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
}
