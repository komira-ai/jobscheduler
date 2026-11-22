//! The verb: its command line, a spec it refuses, and the spec gone before the job starts.

use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;

use super::record::{EXIT, READY, SPEC};
use super::run::tests::Fake;
use super::spec::tests::spec;
use super::{VERB, run_over, spec_path, status};

/// `js-agent __supervise-native` followed by `rest`.
fn words(rest: &[&str]) -> Vec<OsString> {
    ["js-agent", VERB]
        .iter()
        .chain(rest)
        .map(OsString::from)
        .collect()
}

/// A scratch directory with an empty record directory in it.
fn scratch(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), &format!("mac-supervisor-verb-{name}"));
    let records = dir.join("records");
    fs::create_dir(&records).unwrap();
    (dir, records)
}

/// Writes `text` as the spec in `records`, with `mode`; its path.
fn spec_file(records: &Path, text: &str, mode: u32) -> PathBuf {
    let path = records.join(SPEC);
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// The status of the verb over `host` with `--spec <path>`, and what it wrote to its error
/// stream.
fn verb(host: &Fake, path: &Path) -> (u8, String) {
    let mut err = Vec::new();
    let args = words(&["--spec", path.to_str().unwrap()]);
    let code = run_over(host, &args, &mut err);
    (code, String::from_utf8(err).unwrap())
}

/// The command line is `--spec <file>` and nothing else. Catches a verb that starts from a
/// spec it was not given.
#[test]
fn the_command_line_is_the_spec_alone() {
    let path = |rest: &[&str]| spec_path(&words(rest)[2..]);
    assert_eq!(
        path(&["--spec", "/r/spec.json"]),
        Ok(PathBuf::from("/r/spec.json"))
    );
    assert_eq!(path(&[]), Err("--spec is required".to_owned()));
    for rest in [
        &["--spec"][..],
        &["--file", "/r/spec.json"][..],
        &["--spec", "/r/spec.json", "--spec", "/r/other.json"][..],
    ] {
        let why = path(rest).unwrap_err();
        assert!(why.starts_with("unexpected arguments "), "{rest:?}: {why}");
    }
}

/// Each spec the verb must not start from: an unknown field, a NUL in the program, in an
/// argument, in a parameter's name and in its value, a relative program, a relative record
/// directory, a file that is a link, a file with mode 0644, and text that is not JSON. Each
/// exits 2 with a usage line, makes no record and calls nothing of the host; the spec is gone
/// whenever it could be read (all but the link, which is not followed). Catches a start from a
/// spec another user could have changed.
#[test]
fn a_spec_that_is_wrong_exits_2_and_writes_nothing() {
    type Change = (&'static str, fn(&mut serde_json::Value));
    let changes: [Change; 7] = [
        ("unknown field", |v| v["extra"] = true.into()),
        ("NUL in program", |v| v["program"] = "/bin/s\0h".into()),
        ("NUL in args", |v| {
            v["args"] = serde_json::json!(["-c", "a\0"])
        }),
        ("NUL in a parameter name", |v| {
            v["params"] = serde_json::json!([["R\0", "v"]]);
        }),
        ("NUL in a parameter value", |v| {
            v["params"] = serde_json::json!([["R", "v\0"]]);
        }),
        ("relative program", |v| v["program"] = "sh".into()),
        ("relative record_dir", |v| {
            v["record_dir"] = "records".into()
        }),
    ];
    for (name, change) in changes {
        let (_dir, records) = scratch("refused");
        let mut value = serde_json::to_value(spec(&records)).unwrap();
        change(&mut value);
        refused(name, &records, &value.to_string(), 0o600);
    }
    let (_dir, records) = scratch("refused-mode");
    let good = serde_json::to_string(&spec(&records)).unwrap();
    refused("mode 0644", &records, &good, 0o644);
    let (_dir, records) = scratch("refused-json");
    refused("bad JSON", &records, "{\"uid\":", 0o600);

    let (dir, records) = scratch("link");
    let target = spec_file(
        dir.path(),
        &serde_json::to_string(&spec(&records)).unwrap(),
        0o600,
    );
    let link = records.join("link.json");
    symlink(&target, &link).unwrap();
    let host = Fake::new();
    let (code, err) = verb(&host, &link);
    assert_eq!(code, status::USAGE, "{err}");
    assert!(
        err.starts_with(&format!("js-agent: {VERB}: opening the spec: ")),
        "{err}"
    );
    assert!(!records.join(READY).exists());
    assert!(!records.join(EXIT).exists());
    assert!(host.calls().is_empty());
    assert!(target.exists());
    assert!(link.symlink_metadata().is_ok());
}

/// Runs the verb over a spec of `text` with `mode` in `records`, and checks it was refused:
/// status 2, a usage line, no record, no call of the host, and the spec gone.
fn refused(name: &str, records: &Path, text: &str, mode: u32) {
    let path = spec_file(records, text, mode);
    let host = Fake::new();
    let (code, err) = verb(&host, &path);
    assert_eq!(code, status::USAGE, "{name}");
    assert!(
        err.starts_with(&format!("js-agent: {VERB}: ")),
        "{name}: {err}"
    );
    assert!(err.ends_with("usage: js-agent __supervise-native --spec <file>\n"));
    assert!(!records.join(READY).exists(), "{name}");
    assert!(!records.join(EXIT).exists(), "{name}");
    assert!(host.calls().is_empty(), "{name}");
    assert!(!path.exists(), "{name}: the spec stays");
}

/// The spec file is gone when the job starts: the host's start of the job looks for it. The
/// start then goes through. Catches a profile text or parameter list left on disk for the
/// job's user to read.
#[test]
fn the_spec_is_unlinked_before_the_job_starts() {
    let (_dir, records) = scratch("unlinked");
    let path = spec_file(
        &records,
        &serde_json::to_string(&spec(&records)).unwrap(),
        0o600,
    );
    let host = Fake {
        gone_at_spawn: RefCell::new(Some((path.clone(), false))),
        ..Fake::new()
    };
    let (code, err) = verb(&host, &path);
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(host.gone_at_spawn.borrow().clone(), Some((path, true)));
    assert!(host.calls().iter().any(|call| call == "spawn"));
}

/// Off macOS the real verb reads and removes its spec, makes its records, and fails at the
/// limits (the first step an ordinary user with no new session reaches), with the exit record
/// and the line saying so. Catches a Linux build that would start a job with no fence.
#[cfg(not(target_os = "macos"))]
#[test]
fn off_macos_the_verb_fails_at_its_first_fence_step() {
    let (_dir, records) = scratch("off-macos");
    let unfenced = super::SuperviseSpec {
        new_session: false,
        ..spec(&records)
    };
    let path = spec_file(&records, &serde_json::to_string(&unfenced).unwrap(), 0o600);
    let args = words(&["--spec", path.to_str().unwrap()]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = super::run(&args, &mut out, &mut err);
    assert_eq!(code, std::process::ExitCode::from(status::SETUP_FAILED));
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "js-agent: __supervise-native: limits: macOS only\n"
    );
    assert!(out.is_empty());
    assert!(!path.exists());
    let exit = fs::read_to_string(records.join(EXIT)).unwrap();
    assert_eq!(
        super::record::parse_exit(&exit).map(|exit| exit.ended),
        Some(super::Ended::SetupFailed {
            step: super::Step::Limits,
            why: "macOS only".to_owned(),
        })
    );
    assert_eq!(fs::read_to_string(records.join(READY)).unwrap(), "");
}
