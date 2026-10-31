//! The hook record and its lock: the check of the state directory, the exclusive lock, the
//! atomic save, the refusal of records that are not this hook's.

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use js_agent::update_hook::{
    Cordon, DirProblem, DirStat, Held, Invalid, LOCK_FILE, RECORD_FILE, RECORD_MAX, Reason, Record,
    Rollback, SCHEMA, StateError, TEMP_FILE, UpdateInfo, check_dir, system_stat,
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

/// A `stat` that says the directory is as it must be, whoever runs the test.
fn as_root(_: &Path) -> io::Result<DirStat> {
    Ok(ROOT_0700)
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
fn the_system_stat_reports_what_the_directory_is_without_following_a_link() {
    let dir = cell("hook-record-stat");
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    let seen = system_stat(&real).unwrap();
    assert!(seen.is_dir && !seen.is_symlink);
    assert_eq!(seen.mode, 0o700);
    assert_eq!(seen.uid, rustix::process::geteuid().as_raw());
    assert_eq!(seen.gid, rustix::process::getegid().as_raw());

    let link = dir.path().join("link");
    symlink(&real, &link).unwrap();
    let seen = system_stat(&link).unwrap();
    assert!(seen.is_symlink && !seen.is_dir);

    let file = dir.path().join("file");
    fs::write(&file, "x").unwrap();
    assert!(!system_stat(&file).unwrap().is_dir);

    let error = system_stat(&dir.path().join("absent")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
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
        let held = Held::open(dir.path(), &|_: &Path| Ok(stat));
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
fn a_directory_that_cannot_be_looked_at_or_locked_in_is_an_io_error() {
    let dir = cell("hook-record-absent");
    let absent = dir.path().join("absent");
    // The real stat: there is nothing there.
    assert!(matches!(
        Held::open(&absent, &system_stat),
        Err(StateError::Io { .. })
    ));
    // The stat says it is fine, the lock file cannot be made.
    let error = Held::open(&absent, &as_root).unwrap_err();
    match &error {
        StateError::Io { path, .. } => assert_eq!(path, &absent.join(LOCK_FILE)),
        other => panic!("{other:?}"),
    }
    assert_eq!(error.reason(), Reason::StateInvalid);
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

#[test]
fn a_record_that_cannot_be_read_is_an_io_error() {
    let dir = cell("hook-record-unreadable");
    let held = Held::open(dir.path(), &as_root).unwrap();
    // A directory opens and cannot be read.
    fs::create_dir(held.record_path()).unwrap();
    assert!(matches!(held.load(), Err(StateError::Io { .. })));
    fs::remove_dir(held.record_path()).unwrap();
    // A link to itself cannot be opened, and is not "absent".
    symlink(RECORD_FILE, held.record_path()).unwrap();
    assert!(matches!(held.load(), Err(StateError::Io { .. })));
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
