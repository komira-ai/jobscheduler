//! The binary's child modes, started as the probe starts them inside a profile, and the
//! launchd probe's job and children.

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};

use js_mac_probe::launchd::{self, Hello, Kind};
use js_testkit::dir::TestDir;

fn probe() -> Command {
    Command::new(env!("CARGO_BIN_EXE_js-mac-probe"))
}

#[test]
fn the_write_child_creates_a_file_and_exits_zero() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "binary-write");
    let path = scratch.join("file");
    let out = probe()
        .arg("child")
        .arg("write")
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"probe\n");
}

#[test]
fn a_refused_child_exits_one_with_the_reason_on_stderr() {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("no-such-dir")
        .join("f");
    let out = probe()
        .arg("child")
        .arg("write")
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("errno=2 "), "{stderr}");
}

fn hello_of(out: &std::process::Output) -> Hello {
    assert!(out.status.success(), "{out:?}");
    Hello::parse(&String::from_utf8_lossy(&out.stdout)).unwrap()
}

fn hold(scratch: &TestDir, extra: &[&str]) -> (Hello, u32) {
    let child = probe()
        .args(["child", "hold", "--tag"])
        .arg(scratch.join("t"))
        .args(["--secs", "0"])
        .args(extra)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    (hello_of(&child.wait_with_output().unwrap()), pid)
}

/// The uid that owns what this test creates, which is the test's own.
fn own_uid(scratch: &TestDir) -> u32 {
    std::fs::metadata(scratch.path()).unwrap().uid()
}

fn own_gid(scratch: &TestDir) -> u32 {
    std::fs::metadata(scratch.path()).unwrap().gid()
}

#[test]
fn a_hold_child_prints_its_own_ids_and_stays_in_its_parents_group() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hold-plain");
    let (hello, pid) = hold(&scratch, &[]);
    assert_eq!(hello.pid, i32::try_from(pid).unwrap());
    assert_ne!(hello.pgid, hello.pid);
    assert_ne!(hello.sid, hello.pid);
    assert_eq!(hello.uid, own_uid(&scratch));
}

#[test]
fn a_hold_child_with_setsid_leads_a_session_and_group_of_its_own() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hold-setsid");
    let (hello, _) = hold(&scratch, &["--setsid"]);
    assert_eq!(hello.sid, hello.pid);
    assert_eq!(hello.pgid, hello.pid);
}

#[test]
fn a_hold_child_that_already_leads_its_group_cannot_start_a_session_and_exits_one() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hold-leader");
    let out = probe()
        .args(["child", "hold", "--tag"])
        .arg(scratch.join("t"))
        .args(["--secs", "0", "--setsid"])
        .process_group(0)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("errno=1 "), "{stderr}");
}

fn job(state: &std::path::Path, drop: &str) -> std::process::Output {
    probe()
        .args(["child", "launchd-job", "--state"])
        .arg(state)
        .args(["--secs", "0", "--drop", drop])
        .output()
        .unwrap()
}

fn read(path: std::path::PathBuf) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn the_job_starts_a_child_of_each_kind_in_its_place_and_lists_them_once() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "job");
    let (plain, _) = hold(&scratch, &[]);
    let ids = format!("{}:{}", own_uid(&scratch), own_gid(&scratch));
    let state = scratch.path();
    let first = probe()
        .args(["child", "launchd-job", "--state"])
        .arg(state)
        .args(["--secs", "0", "--drop", &ids])
        .spawn()
        .unwrap();
    let job_pid = i32::try_from(first.id()).unwrap();
    assert!(first.wait_with_output().unwrap().status.success());

    let listed = read(state.join(launchd::CHILDREN));
    let children = launchd::parse_children(&listed);
    let kinds: Vec<Kind> = children.iter().map(|c| c.kind).collect();
    assert_eq!(kinds, Kind::started(true));
    let ok: Vec<Hello> = children
        .iter()
        .map(|c| *c.outcome.as_ref().unwrap())
        .collect();
    // In the job's group and session, which are the test's.
    assert_eq!((ok[0].pgid, ok[0].sid), (plain.pgid, plain.sid));
    // A group of its own in the job's session.
    assert_eq!((ok[1].pgid, ok[1].sid), (ok[1].pid, plain.sid));
    // A session of its own.
    assert_eq!((ok[2].pgid, ok[2].sid), (ok[2].pid, ok[2].pid));
    // A group of its own, as the ids it was given.
    assert_eq!((ok[3].pgid, ok[3].uid), (ok[3].pid, own_uid(&scratch)));
    assert!(ok.iter().all(|h| h.pid != job_pid));
    assert_eq!(
        launchd::parse_instances(&read(state.join(launchd::INSTANCES))),
        [job_pid]
    );
    assert!(!state.join("children.tmp").exists());

    // A restart of the job starts no children and leaves the list as it was.
    let again = job(state, &ids);
    assert!(again.status.success(), "{again:?}");
    assert_eq!(read(state.join(launchd::CHILDREN)), listed);
    let pids = launchd::parse_instances(&read(state.join(launchd::INSTANCES)));
    assert_eq!(pids.len(), 2);
    assert_eq!(pids[0], job_pid);
}

#[test]
fn a_child_the_kernel_refuses_to_start_is_listed_with_the_reason_and_the_rest_start() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "job-refused");
    // No process may take uid or gid -1.
    let out = job(scratch.path(), "4294967295:4294967295");
    assert!(out.status.success(), "{out:?}");
    let children = launchd::parse_children(&read(scratch.join(launchd::CHILDREN)));
    assert_eq!(children.len(), 4);
    assert!(
        children[..3].iter().all(|c| c.outcome.is_ok()),
        "{children:?}"
    );
    assert_eq!(children[3].kind, Kind::Dropped);
    let reason = children[3].outcome.as_ref().unwrap_err();
    assert!(reason.starts_with("cannot start: "), "{reason}");
}

#[test]
fn a_job_without_ids_starts_no_dropped_child() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "job-nodrop");
    let out = probe()
        .args(["child", "launchd-job", "--state"])
        .arg(scratch.path())
        .args(["--secs", "0"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let children = launchd::parse_children(&read(scratch.join(launchd::CHILDREN)));
    let kinds: Vec<Kind> = children.iter().map(|c| c.kind).collect();
    assert_eq!(kinds, Kind::started(false));
}

#[test]
fn a_job_whose_state_directory_is_missing_exits_one_and_starts_nothing() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "job-missing");
    let out = job(&scratch.join("no-such-dir"), "1:1");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("errno=2 "), "{stderr}");
    assert!(!scratch.join("no-such-dir").exists());
}
