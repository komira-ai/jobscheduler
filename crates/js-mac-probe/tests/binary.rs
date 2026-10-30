//! The binary's child modes, started as the probe starts them inside a profile.

use std::process::Command;

fn probe() -> Command {
    Command::new(env!("CARGO_BIN_EXE_js-mac-probe"))
}

#[test]
fn the_write_child_creates_a_file_and_exits_zero() {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("binary-write");
    let out = probe()
        .arg("child")
        .arg("write")
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"probe\n");
    std::fs::remove_file(&path).unwrap();
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
