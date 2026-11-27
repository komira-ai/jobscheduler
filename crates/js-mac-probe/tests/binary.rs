//! The binary's child modes, started as the probe starts them inside a profile.

use std::process::Command;

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

#[test]
fn the_launchd_job_child_starts_hold_writes_both_pids_and_ends_with_it() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "binary-launchd-job");
    let ready = scratch.join("ready");
    let child = probe()
        .args(["child", "launchd-job", "--secs", "0", "--tag", "t", "--out"])
        .arg(&ready)
        .spawn()
        .unwrap();
    let job = child.id();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = std::fs::read_to_string(&ready).unwrap();
    let (written_job, hold) = js_mac_probe::launchd::ready(&text).unwrap();
    assert_eq!(written_job, job);
    assert_ne!(hold, job);
}

#[test]
fn the_hold_child_prints_its_tag() {
    let out = probe()
        .args(["child", "hold", "--secs", "0", "--tag", "js-mac-probe.7"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"js-mac-probe.7\n");
}
