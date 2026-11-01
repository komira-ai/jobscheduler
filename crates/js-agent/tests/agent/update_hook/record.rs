//! The hook record and its lock: the check of the state directory, the exclusive lock, the
//! atomic save, the refusal of records that are not this hook's.

use std::fs::{self, File};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use js_agent::update_hook::{
    Cordon, DirProblem, DirStat, Held, Invalid, LOCK_FILE, RECORD_FILE, RECORD_MAX, Reason, Record,
    Rollback, SCHEMA, StateError, TEMP_FILE, UpdateInfo, check_ancestor, check_dir, fd_stat,
};
use js_testkit::dir::TestDir;

/// What the directory of a root-run hook looks like.
const ROOT_0700: DirStat = DirStat {
    is_symlink: false,
    is_dir: true,
    uid: 0,
    gid: 0,
    mode: 0o700,
};

/// A `stat` that says every directory is as it must be, whoever runs the test.
fn as_root(_: &Path, _: BorrowedFd<'_>) -> io::Result<DirStat> {
    Ok(ROOT_0700)
}

/// A `stat` that shows what is at and under `tree` as it really is, except that root owns
/// it (the test is not root), and shows everything above it as a root-owned 0755 directory.
fn rooted(tree: &Path) -> impl Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat> {
    let tree = tree.to_owned();
    move |path, fd| {
        if path.starts_with(&tree) {
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
    }
}

/// A state directory (0700) under a parent of `parent_mode`, in a cell that is 0755.
fn tree(name: &str, parent_mode: u32) -> (TestDir, PathBuf, PathBuf) {
    let dir = cell(name);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let parent = dir.path().join("parent");
    let state = parent.join("state");
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(parent_mode)).unwrap();
    (dir, parent, state)
}

fn cell(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

fn update(target: &str) -> UpdateInfo {
    UpdateInfo {
        target: target.to_owned(),
        previous: Some("1.3.2".to_owned()),
        staged: PathBuf::from("/opt/js-agent/releases/new"),
        previous_target: Some(PathBuf::from("/opt/js-agent/releases/old")),
        unit: "js-agent-crun.service".to_owned(),
        artifact_digest: format!("sha256:{}", "a".repeat(64)),
    }
}

fn record(target: &str) -> Record {
    Record::new(update(target))
}

#[test]
fn a_new_record_has_the_current_schema_and_nothing_done() {
    let record = record("1.4.0");
    assert_eq!(record.schema, SCHEMA);
    assert_eq!(record.cordon, Cordon::default());
    assert_eq!(record.rb, Rollback::default());
    assert!(!record.stop_begun && !record.link_moved && !record.start_issued && !record.verify_ok);
}

#[test]
fn the_state_directory_must_be_a_root_owned_0700_directory_and_not_a_link() {
    assert_eq!(check_dir(&ROOT_0700), Ok(()));
    let cases = [
        (
            DirStat {
                is_symlink: true,
                is_dir: false,
                ..ROOT_0700
            },
            DirProblem::Symlink,
        ),
        (
            DirStat {
                is_symlink: true,
                ..ROOT_0700
            },
            DirProblem::Symlink,
        ),
        (
            DirStat {
                is_dir: false,
                ..ROOT_0700
            },
            DirProblem::NotDirectory,
        ),
        (
            DirStat {
                uid: 1000,
                ..ROOT_0700
            },
            DirProblem::Owner { uid: 1000, gid: 0 },
        ),
        (
            DirStat {
                gid: 4,
                ..ROOT_0700
            },
            DirProblem::Owner { uid: 0, gid: 4 },
        ),
        (
            DirStat {
                mode: 0o755,
                ..ROOT_0700
            },
            DirProblem::Mode { mode: 0o755 },
        ),
        (
            DirStat {
                mode: 0o770,
                ..ROOT_0700
            },
            DirProblem::Mode { mode: 0o770 },
        ),
        (
            DirStat {
                mode: 0o600,
                ..ROOT_0700
            },
            DirProblem::Mode { mode: 0o600 },
        ),
        (
            DirStat {
                mode: 0o4700,
                ..ROOT_0700
            },
            DirProblem::Mode { mode: 0o4700 },
        ),
    ];
    for (stat, problem) in cases {
        assert_eq!(check_dir(&stat), Err(problem), "{stat:?}");
    }
    assert_eq!(DirProblem::Symlink.to_string(), "is a symbolic link");
    assert_eq!(DirProblem::NotDirectory.to_string(), "is not a directory");
    assert_eq!(
        DirProblem::Owner { uid: 1, gid: 2 }.to_string(),
        "is owned by 1:2, not 0:0"
    );
    assert_eq!(
        DirProblem::Mode { mode: 0o755 }.to_string(),
        "has mode 0755, not 0700"
    );
}

#[test]
fn the_fd_stat_reports_what_the_open_directory_is() {
    let dir = cell("hook-record-stat");
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    let open = |path: &Path| File::open(path).unwrap();
    let seen = fd_stat(open(&real).as_fd()).unwrap();
    assert!(seen.is_dir && !seen.is_symlink);
    assert_eq!(seen.mode, 0o700);
    assert_eq!(seen.uid, rustix::process::geteuid().as_raw());
    assert_eq!(seen.gid, rustix::process::getegid().as_raw());

    // A link is followed by this open, so what is seen is the directory it names; the state
    // directory is never opened this way (see the symlink tests below).
    let file = dir.path().join("file");
    fs::write(&file, "x").unwrap();
    assert!(!fd_stat(open(&file).as_fd()).unwrap().is_dir);
}

#[test]
fn an_ancestor_must_be_a_root_owned_directory_nobody_else_can_write() {
    assert_eq!(check_ancestor(&ROOT_0700), Ok(()));
    let root_755 = DirStat {
        mode: 0o755,
        ..ROOT_0700
    };
    assert_eq!(check_ancestor(&root_755), Ok(()));
    let cases = [
        (
            DirStat {
                is_symlink: true,
                ..root_755
            },
            DirProblem::Symlink,
        ),
        (
            DirStat {
                is_dir: false,
                ..root_755
            },
            DirProblem::NotDirectory,
        ),
        (
            DirStat {
                uid: 1000,
                ..root_755
            },
            DirProblem::NotRootOwned { uid: 1000 },
        ),
        (
            DirStat {
                mode: 0o775,
                ..root_755
            },
            DirProblem::Writable { mode: 0o775 },
        ),
        (
            DirStat {
                mode: 0o757,
                ..root_755
            },
            DirProblem::Writable { mode: 0o757 },
        ),
        (
            DirStat {
                mode: 0o1777,
                ..root_755
            },
            DirProblem::Writable { mode: 0o1777 },
        ),
        (
            DirStat {
                mode: 0o720,
                ..root_755
            },
            DirProblem::Writable { mode: 0o720 },
        ),
    ];
    for (stat, problem) in cases {
        assert_eq!(check_ancestor(&stat), Err(problem), "{stat:?}");
    }
    assert_eq!(
        DirProblem::NotRootOwned { uid: 7 }.to_string(),
        "is owned by 7, not by root"
    );
    assert_eq!(
        DirProblem::Writable { mode: 0o775 }.to_string(),
        "has mode 0775, writable by its group or by others"
    );
    assert_eq!(
        DirProblem::NotPlain.to_string(),
        "is not an absolute path of plain components"
    );
}

#[test]
fn a_directory_that_is_not_as_required_is_refused_before_anything_is_made_in_it() {
    let cases = [
        (
            DirStat {
                is_symlink: true,
                ..ROOT_0700
            },
            DirProblem::Symlink,
        ),
        (
            DirStat {
                uid: 1000,
                ..ROOT_0700
            },
            DirProblem::Owner { uid: 1000, gid: 0 },
        ),
        (
            DirStat {
                mode: 0o755,
                ..ROOT_0700
            },
            DirProblem::Mode { mode: 0o755 },
        ),
    ];
    for (stat, problem) in cases {
        let dir = cell("hook-record-refused");
        let held = Held::open(dir.path(), &|path: &Path, _: BorrowedFd<'_>| {
            Ok(if path == dir.path() { stat } else { ROOT_0700 })
        });
        match held {
            Err(StateError::Dir {
                problem: got, path, ..
            }) => {
                assert_eq!(got, problem);
                assert_eq!(path, dir.path());
            }
            other => panic!("{problem:?}: {other:?}"),
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0, "{problem:?}");
    }
}

#[test]
fn a_directory_that_cannot_be_opened_is_an_io_error() {
    let dir = cell("hook-record-absent");
    let absent = dir.path().join("absent");
    let error = Held::open(&absent, &rooted(dir.path())).unwrap_err();
    match &error {
        StateError::Io { path, source } => {
            assert_eq!(path, &absent);
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(error.reason(), Reason::StateInvalid);
    // An ancestor that is not there is the same, said of that ancestor.
    let deep = absent.join("deeper");
    match Held::open(&deep, &rooted(dir.path())) {
        Err(StateError::Io { path, .. }) => assert_eq!(path, absent),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_state_directory_that_is_a_link_or_a_file_is_refused_by_the_open_itself() {
    let dir = cell("hook-record-link-state");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    let link = dir.path().join("link");
    symlink(&real, &link).unwrap();
    let file = dir.path().join("file");
    fs::write(&file, "x").unwrap();
    for (path, want) in [
        (link, DirProblem::Symlink),
        (file, DirProblem::NotDirectory),
    ] {
        match Held::open(&path, &rooted(dir.path())) {
            Err(StateError::Dir { path: got, problem }) => {
                assert_eq!(got, path);
                assert_eq!(problem, want);
            }
            other => panic!("{want:?}: {other:?}"),
        }
    }
    assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
}

#[test]
fn a_path_that_is_not_plain_is_refused_before_anything_is_opened() {
    for path in ["state", "/a/../b", "/a/./b", "/a//b", "/a/b/"] {
        match Held::open(Path::new(path), &as_root) {
            Err(StateError::Dir {
                problem: DirProblem::NotPlain,
                ..
            }) => {}
            other => panic!("{path}: {other:?}"),
        }
    }
}

#[test]
fn a_parent_that_others_can_write_is_refused_and_nothing_is_made() {
    for mode in [0o770, 0o707, 0o777, 0o720, 0o1777] {
        let (dir, parent, state) = tree("hook-record-writable-parent", mode);
        match Held::open(&state, &rooted(dir.path())) {
            Err(StateError::Ancestor { path, problem }) => {
                assert_eq!(path, parent, "{mode:o}");
                assert_eq!(problem, DirProblem::Writable { mode }, "{mode:o}");
            }
            other => panic!("{mode:o}: {other:?}"),
        }
        assert_eq!(fs::read_dir(&state).unwrap().count(), 0, "{mode:o}");
    }
    let (dir, _, state) = tree("hook-record-writable-parent-ok", 0o755);
    assert!(Held::open(&state, &rooted(dir.path())).is_ok());
}

#[test]
fn a_writable_grandparent_is_as_bad_as_a_writable_parent() {
    let (dir, parent, state) = tree("hook-record-writable-grand", 0o755);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o775)).unwrap();
    match Held::open(&state, &rooted(dir.path())) {
        Err(StateError::Ancestor { path, problem }) => {
            assert_eq!(path, dir.path());
            assert_eq!(problem, DirProblem::Writable { mode: 0o775 });
            assert_ne!(path, parent);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_parent_owned_by_someone_else_and_the_root_itself_are_checked() {
    let (dir, parent, state) = tree("hook-record-owner-parent", 0o755);
    let by_parent = |path: &Path, fd: BorrowedFd<'_>| -> io::Result<DirStat> {
        let mut seen = rooted(dir.path())(path, fd)?;
        if path == parent {
            seen.uid = 1000;
        }
        Ok(seen)
    };
    match Held::open(&state, &by_parent) {
        Err(StateError::Ancestor { path, problem }) => {
            assert_eq!(path, parent);
            assert_eq!(problem, DirProblem::NotRootOwned { uid: 1000 });
        }
        other => panic!("{other:?}"),
    }
    // Every directory from `/` down is shown to the check, in order, the state directory last.
    let seen = std::cell::RefCell::new(Vec::new());
    let record_paths = |path: &Path, fd: BorrowedFd<'_>| -> io::Result<DirStat> {
        seen.borrow_mut().push(path.to_owned());
        rooted(dir.path())(path, fd)
    };
    Held::open(&state, &record_paths).unwrap();
    let want: Vec<PathBuf> = state.ancestors().map(Path::to_owned).collect();
    let want: Vec<PathBuf> = want.into_iter().rev().collect();
    assert_eq!(*seen.borrow(), want);
    assert_eq!(want[0], Path::new("/"));
}

#[test]
fn a_link_or_a_file_among_the_ancestors_is_refused() {
    let dir = cell("hook-record-link-ancestor");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    symlink(&real, &link).unwrap();
    let file = dir.path().join("file");
    fs::write(&file, "x").unwrap();
    for (ancestor, want) in [
        (link, DirProblem::Symlink),
        (file, DirProblem::NotDirectory),
    ] {
        match Held::open(&ancestor.join("state"), &rooted(dir.path())) {
            Err(StateError::Ancestor { path, problem }) => {
                assert_eq!(path, ancestor);
                assert_eq!(problem, want);
            }
            other => panic!("{want:?}: {other:?}"),
        }
    }
    let error = StateError::Ancestor {
        path: PathBuf::from("/a"),
        problem: DirProblem::Symlink,
    };
    assert_eq!(error.reason(), Reason::StateInvalid);
    assert_eq!(
        error.to_string(),
        "above the state directory, /a: is a symbolic link"
    );
}

#[test]
fn a_path_swapped_for_a_link_after_the_open_does_not_redirect_a_write() {
    let (dir, _, state) = tree("hook-record-swap", 0o755);
    let held = Held::open(&state, &rooted(dir.path())).unwrap();
    // Another hand moves the directory away and puts a link to a directory of its own where
    // the path was.
    let moved = dir.path().join("moved");
    let other = dir.path().join("other");
    fs::create_dir(&other).unwrap();
    fs::rename(&state, &moved).unwrap();
    symlink(&other, &state).unwrap();

    held.save(&record("1.4.0")).unwrap();
    assert!(moved.join(RECORD_FILE).is_file());
    assert!(!moved.join(TEMP_FILE).exists());
    assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
    assert_eq!(held.load().unwrap(), Some(record("1.4.0")));
    // A second step, coming by the path now, is refused as the link it is.
    assert!(matches!(
        Held::open(&state, &rooted(dir.path())),
        Err(StateError::Dir {
            problem: DirProblem::Symlink,
            ..
        })
    ));
}

#[test]
fn the_lock_is_exclusive_and_a_second_holder_changes_nothing() {
    let dir = cell("hook-record-lock");
    let first = Held::open(dir.path(), &as_root).unwrap();
    first.save(&record("1.4.0")).unwrap();
    let before = fs::read(first.record_path()).unwrap();

    let second = Held::open(dir.path(), &as_root).unwrap_err();
    match &second {
        StateError::Lock { path, source } => {
            assert_eq!(path, &dir.path().join(LOCK_FILE));
            assert_eq!(source.kind(), io::ErrorKind::WouldBlock);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(second.reason(), Reason::Locked);
    assert!(second.to_string().starts_with("cannot lock "));
    assert_eq!(fs::read(first.record_path()).unwrap(), before);
    assert!(!dir.path().join(TEMP_FILE).exists());

    drop(first);
    let third = Held::open(dir.path(), &as_root).unwrap();
    assert_eq!(third.load().unwrap(), Some(record("1.4.0")));
}

#[test]
fn a_record_is_saved_and_loaded_and_the_next_save_replaces_it() {
    let dir = cell("hook-record-save");
    let held = Held::open(dir.path(), &as_root).unwrap();
    assert_eq!(held.load().unwrap(), None);

    let mut first = record("1.4.0");
    first.cordon = Cordon {
        placed: true,
        provisional: false,
        first_ms: 1_000,
        expires_ms: 301_000,
    };
    held.save(&first).unwrap();
    assert_eq!(held.load().unwrap(), Some(first.clone()));
    assert_eq!(held.record_path(), dir.path().join(RECORD_FILE));
    let mode = fs::metadata(held.record_path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(!dir.path().join(TEMP_FILE).exists());

    let mut second = first;
    second.stop_begun = true;
    held.save(&second).unwrap();
    assert_eq!(held.load().unwrap(), Some(second));
}

#[test]
fn a_crash_between_the_write_and_the_rename_leaves_the_old_record() {
    let dir = cell("hook-record-crash");
    let old = record("1.4.0");
    let new = Record {
        stop_begun: true,
        ..record("1.4.0")
    };
    {
        let held = Held::open(dir.path(), &as_root).unwrap();
        held.save(&old).unwrap();
        // The process dies here: the temporary file is written and synced, not renamed.
        held.write_temp(&new).unwrap();
        assert!(dir.path().join(TEMP_FILE).exists());
        assert_eq!(held.load().unwrap(), Some(old.clone()));
    }
    // The next step finds the old record, and a leftover it overwrites.
    let held = Held::open(dir.path(), &as_root).unwrap();
    assert_eq!(held.load().unwrap(), Some(old));
    held.save(&new).unwrap();
    assert_eq!(held.load().unwrap(), Some(new));
    assert!(!dir.path().join(TEMP_FILE).exists());
}

#[test]
fn a_commit_with_nothing_written_and_a_temporary_file_in_the_way_are_io_errors() {
    let dir = cell("hook-record-io");
    let held = Held::open(dir.path(), &as_root).unwrap();
    assert!(matches!(held.commit(), Err(StateError::Io { .. })));
    fs::create_dir(dir.path().join(TEMP_FILE)).unwrap();
    assert!(matches!(
        held.write_temp(&record("1.4.0")),
        Err(StateError::Io { .. })
    ));
    assert_eq!(held.load().unwrap(), None);
}

#[test]
fn a_record_of_a_newer_schema_is_not_loaded_and_not_overwritten() {
    let dir = cell("hook-record-newer");
    let held = Held::open(dir.path(), &as_root).unwrap();
    let future = br#"{"schema":2,"update":{"something":"new"}}"#;
    fs::write(held.record_path(), future).unwrap();

    let error = held.load().unwrap_err();
    assert!(matches!(error, StateError::Newer { found: 2 }));
    assert_eq!(error.reason(), Reason::StateInvalid);
    assert_eq!(
        error.to_string(),
        "the record has schema 2, newer than 1: read-only to this hook"
    );
    assert!(matches!(
        held.save(&record("1.4.0")),
        Err(StateError::Newer { found: 2 })
    ));
    assert_eq!(fs::read(held.record_path()).unwrap(), future);
    assert!(!dir.path().join(TEMP_FILE).exists());
}

#[test]
fn what_is_not_a_record_of_this_schema_is_corrupt() {
    let dir = cell("hook-record-corrupt");
    let held = Held::open(dir.path(), &as_root).unwrap();
    let good = serde_json::to_string(&record("1.4.0")).unwrap();
    let unknown_field = good.replacen("\"schema\":1", "\"schema\":1,\"extra\":true", 1);
    let wrong_type = good.replacen("\"stop_begun\":false", "\"stop_begun\":\"no\"", 1);
    let mut oversize = good.clone();
    oversize.push_str(&" ".repeat(usize::try_from(RECORD_MAX).unwrap()));
    for (name, body) in [
        ("empty", String::new()),
        ("not json", "record".to_owned()),
        ("no schema", r#"{"update":{}}"#.to_owned()),
        ("schema 0", r#"{"schema":0}"#.to_owned()),
        ("schema as text", r#"{"schema":"1"}"#.to_owned()),
        ("unknown field", unknown_field),
        ("wrong type", wrong_type),
        ("fields missing", r#"{"schema":1}"#.to_owned()),
        ("too large", oversize),
    ] {
        fs::write(held.record_path(), &body).unwrap();
        let error = held.load().unwrap_err();
        assert!(matches!(error, StateError::Corrupt(_)), "{name}: {error}");
        assert_eq!(error.reason(), Reason::StateInvalid, "{name}");
        // A record that cannot be read is not replaced by a save either.
        assert!(held.save(&record("1.4.0")).is_err(), "{name}");
        assert_eq!(
            fs::read_to_string(held.record_path()).unwrap(),
            body,
            "{name}"
        );
    }
    // The same text within the limit loads.
    fs::write(held.record_path(), &good).unwrap();
    assert_eq!(held.load().unwrap(), Some(record("1.4.0")));
}

/// Runs `work` on a thread and fails the test, instead of hanging it, if it does not return.
fn within<T: Send + 'static>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(work());
    });
    receive
        .recv_timeout(std::time::Duration::from_secs(60))
        .unwrap_or_else(|_| panic!("{what} blocked"))
}

fn fifo(path: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
}

#[test]
fn a_directory_in_place_of_the_record_is_not_a_record() {
    let dir = cell("hook-record-irregular");
    let held = Held::open(dir.path(), &as_root).unwrap();
    fs::create_dir(held.record_path()).unwrap();
    assert!(matches!(held.load(), Err(StateError::Corrupt(_))));
}

#[test]
fn a_fifo_in_place_of_the_record_is_refused_without_blocking() {
    let dir = cell("hook-record-fifo");
    let record = dir.path().join(RECORD_FILE);
    fifo(&record);
    let path = dir.path().to_owned();
    let loaded = within("loading a FIFO record", move || {
        Held::open(&path, &as_root).unwrap().load()
    });
    assert!(matches!(loaded, Err(StateError::Corrupt(_))), "{loaded:?}");
}

#[test]
fn a_symbolic_link_in_place_of_the_record_or_its_temporary_file_is_not_followed() {
    let dir = cell("hook-record-links");
    let held = Held::open(dir.path(), &as_root).unwrap();
    let elsewhere = dir.path().join("elsewhere");
    fs::write(&elsewhere, serde_json::to_string(&record("1.4.0")).unwrap()).unwrap();
    // A link to a good record is not that record.
    symlink(&elsewhere, held.record_path()).unwrap();
    assert!(matches!(held.load(), Err(StateError::Io { .. })));
    fs::remove_file(held.record_path()).unwrap();
    // A link to itself, and a dangling one, are not "absent".
    symlink(RECORD_FILE, held.record_path()).unwrap();
    assert!(matches!(held.load(), Err(StateError::Io { .. })));
    fs::remove_file(held.record_path()).unwrap();
    symlink(dir.path().join("nothing"), held.record_path()).unwrap();
    assert!(matches!(held.load(), Err(StateError::Io { .. })));
    fs::remove_file(held.record_path()).unwrap();

    // The temporary file is not written through a link.
    let before = fs::read(&elsewhere).unwrap();
    symlink(&elsewhere, dir.path().join(TEMP_FILE)).unwrap();
    assert!(matches!(
        held.write_temp(&record("1.5.0")),
        Err(StateError::Io { .. })
    ));
    assert_eq!(fs::read(&elsewhere).unwrap(), before);
}

#[test]
fn a_fifo_in_place_of_the_temporary_file_does_not_block_the_save() {
    let dir = cell("hook-record-fifo-temp");
    let path = dir.path().to_owned();
    fifo(&path.join(TEMP_FILE));
    let saved = within("saving over a FIFO", move || {
        Held::open(&path, &as_root).unwrap().save(&record("1.4.0"))
    });
    assert!(matches!(saved, Err(StateError::Io { .. })), "{saved:?}");
}

#[test]
fn the_size_limit_is_exact() {
    let dir = cell("hook-record-limit");
    let held = Held::open(dir.path(), &as_root).unwrap();
    let good = serde_json::to_string(&record("1.4.0")).unwrap();
    let limit = usize::try_from(RECORD_MAX).unwrap();
    let padded = |len: usize| format!("{good}{}", " ".repeat(len - good.len()));
    fs::write(held.record_path(), padded(limit)).unwrap();
    assert_eq!(held.load().unwrap(), Some(record("1.4.0")));
    fs::write(held.record_path(), padded(limit + 1)).unwrap();
    assert!(matches!(held.load(), Err(StateError::Corrupt(_))));
}

#[test]
fn both_halves_of_a_save_refuse_a_record_of_a_newer_schema() {
    let dir = cell("hook-record-newer-halves");
    let held = Held::open(dir.path(), &as_root).unwrap();
    let future = br#"{"schema":2}"#;
    fs::write(held.record_path(), future).unwrap();
    assert!(matches!(
        held.write_temp(&record("1.4.0")),
        Err(StateError::Newer { found: 2 })
    ));
    assert!(!dir.path().join(TEMP_FILE).exists());
    // A temporary file that is there (a crash, or another hand) is not committed over it.
    fs::write(dir.path().join(TEMP_FILE), "{}").unwrap();
    assert!(matches!(held.commit(), Err(StateError::Newer { found: 2 })));
    assert_eq!(fs::read(held.record_path()).unwrap(), future);
}

#[test]
fn only_a_second_holder_is_locked_and_every_other_error_is_state_invalid() {
    let io_error = || io::Error::other("x");
    let errors = [
        (
            StateError::Lock {
                path: PathBuf::from("/s/lock"),
                source: io_error(),
            },
            Reason::Locked,
        ),
        (
            StateError::Dir {
                path: PathBuf::from("/s"),
                problem: DirProblem::Symlink,
            },
            Reason::StateInvalid,
        ),
        (
            StateError::Io {
                path: PathBuf::from("/s/record.json"),
                source: io_error(),
            },
            Reason::StateInvalid,
        ),
        (StateError::Corrupt("x".to_owned()), Reason::StateInvalid),
        (StateError::Newer { found: 9 }, Reason::StateInvalid),
        (
            StateError::Invalid(Invalid::SameVersion),
            Reason::StateInvalid,
        ),
    ];
    for (error, reason) in errors {
        assert_eq!(error.reason(), reason, "{error}");
    }
    let dir = StateError::Dir {
        path: PathBuf::from("/s"),
        problem: DirProblem::Symlink,
    };
    assert_eq!(dir.to_string(), "state directory /s: is a symbolic link");
}

#[test]
fn delete_removes_the_record_and_a_leftover_temporary_file_and_nothing_else() {
    let dir = cell("hook-record-delete");
    let held = Held::open(dir.path(), &as_root).unwrap();
    held.save(&record("1.4.0")).unwrap();
    held.write_temp(&record("1.4.1")).unwrap();
    held.delete().unwrap();
    assert_eq!(held.load().unwrap(), None);
    assert!(!held.record_path().exists());
    assert!(!dir.path().join(TEMP_FILE).exists());
    assert!(dir.path().join(LOCK_FILE).exists(), "the lock stays");
    // The next step starts from nothing, and a delete of nothing is a success.
    held.delete().unwrap();
    held.save(&record("1.4.2")).unwrap();
    assert_eq!(held.load().unwrap(), Some(record("1.4.2")));
}

#[test]
fn delete_refuses_a_record_it_cannot_load_and_reports_what_it_cannot_remove() {
    let dir = cell("hook-record-delete-refused");
    let held = Held::open(dir.path(), &as_root).unwrap();
    let future = br#"{"schema":2}"#;
    fs::write(held.record_path(), future).unwrap();
    assert!(matches!(held.delete(), Err(StateError::Newer { found: 2 })));
    assert_eq!(fs::read(held.record_path()).unwrap(), future);

    let other = cell("hook-record-delete-io");
    let held = Held::open(other.path(), &as_root).unwrap();
    held.save(&record("1.4.0")).unwrap();
    // A directory where the temporary file belongs cannot be unlinked as a file.
    fs::create_dir(other.path().join(TEMP_FILE)).unwrap();
    assert!(matches!(held.delete(), Err(StateError::Io { .. })));
}
