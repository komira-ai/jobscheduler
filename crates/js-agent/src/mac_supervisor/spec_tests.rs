//! The supervisor's spec: what it accepts, what it refuses, and that a file is read once and
//! removed.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;

use super::{SpecError, SuperviseSpec, loose, parse, read_spec};

/// A spec as the agent writes one.
pub(crate) fn spec(record_dir: &Path) -> SuperviseSpec {
    SuperviseSpec {
        uid: 701,
        gid: 701,
        new_session: true,
        profile: "(version 1)\n(allow default)\n".to_owned(),
        params: vec![("RUN".to_owned(), "/Users/js-slot-701/exec".to_owned())],
        program: "/bin/sh".to_owned(),
        args: vec!["-c".to_owned(), "exit 0".to_owned()],
        max_file_bytes: 1 << 30,
        max_processes: 256,
        record_dir: record_dir.display().to_string(),
        parent: None,
    }
}

/// A scratch directory for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("mac-supervisor-spec-{name}"))
}

/// Writes `text` to `<dir>/spec.json` with `mode`; its path.
fn written(dir: &Path, text: &str, mode: u32) -> PathBuf {
    let path = dir.join("spec.json");
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// A spec reads back as it was written, from text and from a file of the caller's own with
/// mode 0600, and the file is gone after the read. Catches a supervisor that leaves its spec
/// on disk, or one that cannot read the agent's.
#[test]
fn a_spec_reads_back_as_it_was_written_and_its_file_is_removed() {
    let dir = scratch("round-trip");
    let want = spec(dir.path());
    let text = serde_json::to_string(&want).unwrap();
    assert_eq!(parse(&text).unwrap(), want);
    let path = written(dir.path(), &text, 0o600);
    assert_eq!(read_spec(&path).unwrap(), want);
    assert!(!path.exists());
}

/// Every refusal of the text, each by its own message: an unknown field, text that is not
/// JSON, a NUL in each kind of string, a relative program and a relative record directory.
/// Catches a supervisor that starts from a spec it should not trust or cannot hand to a C
/// call.
#[test]
fn a_spec_text_that_is_wrong_is_refused_by_name() {
    let dir = scratch("text");
    let with = |change: fn(&mut SuperviseSpec)| {
        let mut changed = spec(dir.path());
        change(&mut changed);
        serde_json::to_string(&changed).unwrap()
    };
    let nuls: [fn(&mut SuperviseSpec); 7] = [
        |s| s.profile.push('\0'),
        |s| s.program.push('\0'),
        |s| s.record_dir.push('\0'),
        |s| s.args.push("a\0".to_owned()),
        |s| s.params.push(("K\0".to_owned(), "v".to_owned())),
        |s| s.params.push(("K".to_owned(), "v\0".to_owned())),
        |s| s.args[0].insert(0, '\0'),
    ];
    for change in nuls {
        assert_eq!(
            parse(&with(change)).unwrap_err().to_string(),
            "the spec holds a NUL"
        );
    }
    assert_eq!(
        parse(&with(|s| s.program = "bin/sh".to_owned()))
            .unwrap_err()
            .to_string(),
        "the program \"bin/sh\" is not an absolute path"
    );
    assert_eq!(
        parse(&with(|s| s.record_dir = "records".to_owned()))
            .unwrap_err()
            .to_string(),
        "the record directory \"records\" is not an absolute path"
    );
    let mut value = serde_json::to_value(spec(dir.path())).unwrap();
    value["extra"] = serde_json::Value::Bool(true);
    let why = parse(&value.to_string()).unwrap_err().to_string();
    assert!(
        why.starts_with("the spec is not a supervisor spec: unknown field `extra`"),
        "{why}"
    );
    let why = parse("not json").unwrap_err().to_string();
    assert!(
        why.starts_with("the spec is not a supervisor spec: "),
        "{why}"
    );
}

/// A file that is a symbolic link (to a good spec), a directory and a missing file are refused
/// before anything is read, and nothing is removed; a file with mode 0644 or 0620, text that
/// does not parse, and text that is not UTF-8 are refused after the file is removed. Catches a
/// start from a spec another user could have changed, and a refused spec left on disk.
#[test]
fn a_spec_file_that_cannot_be_trusted_is_refused() {
    let dir = scratch("file");
    let good = serde_json::to_string(&spec(dir.path())).unwrap();

    let target = dir.join("target.json");
    fs::write(&target, &good).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.join("link.json");
    symlink(&target, &link).unwrap();
    assert!(matches!(read_spec(&link), Err(SpecError::Open(_))));
    assert!(link.symlink_metadata().is_ok());
    assert!(target.exists());

    let folder = dir.join("folder.json");
    fs::create_dir(&folder).unwrap();
    assert!(matches!(read_spec(&folder), Err(SpecError::NotRegular)));
    assert!(folder.is_dir());

    assert!(matches!(
        read_spec(&dir.join("absent.json")),
        Err(SpecError::Open(_))
    ));

    for mode in [0o644, 0o620] {
        let path = written(dir.path(), &good, mode);
        let why = read_spec(&path).unwrap_err().to_string();
        assert_eq!(
            why,
            format!("the spec has mode {mode:o}; only its owner may hold permissions")
        );
        assert!(!path.exists(), "{mode:o}");
    }

    let path = written(dir.path(), "{\"uid\": 1", 0o600);
    assert!(matches!(read_spec(&path), Err(SpecError::Json(_))));
    assert!(!path.exists());

    let path = dir.join("spec.json");
    fs::write(&path, [0xff, 0xfe]).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(read_spec(&path), Err(SpecError::Read(_))));
    assert!(!path.exists());
}

/// A spec whose directory the caller may not write to cannot be removed, and is refused while
/// it is still there. Catches a start whose spec stays readable on disk. (The tests run as an
/// ordinary user: root could remove it.)
#[test]
fn a_spec_that_cannot_be_removed_is_refused() {
    let dir = scratch("unlink");
    let held = dir.join("held");
    fs::create_dir(&held).unwrap();
    let path = written(&held, &serde_json::to_string(&spec(&held)).unwrap(), 0o600);
    fs::set_permissions(&held, fs::Permissions::from_mode(0o500)).unwrap();
    let read = read_spec(&path);
    fs::set_permissions(&held, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(read, Err(SpecError::Unlink(_))), "{read:?}");
    assert!(path.exists());
}

/// A file of another owner is refused by both uids, whatever its mode; the caller's own file is
/// refused for any bit of its group or others, and passes with the owner's bits alone. Catches
/// a check that trusts another user's file, or one that only looks at some of the bits.
#[test]
fn only_the_caller_s_own_private_file_is_trusted() {
    assert_eq!(
        loose(0, 701, 0o100_600).map(|why| why.to_string()),
        Some("the spec belongs to uid 0, not to uid 701".to_owned())
    );
    for bits in [0o040, 0o020, 0o010, 0o004, 0o002, 0o001] {
        assert!(
            matches!(loose(701, 701, 0o100_600 | bits), Some(SpecError::Mode(_))),
            "{bits:o}"
        );
    }
    for mode in [0o100_600, 0o100_400, 0o100_700] {
        assert!(loose(701, 701, mode).is_none(), "{mode:o}");
    }
}
