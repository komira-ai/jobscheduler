//! The `js-mac-session` binary off macOS: its flags parse, and it refuses to serve.

#![cfg(not(target_os = "macos"))]

use std::process::Command;

fn helper() -> Command {
    Command::new(env!("CARGO_BIN_EXE_js-mac-session"))
}

/// Catches: a binary that would bind a socket on a platform whose host it lacks.
#[test]
fn it_refuses_to_serve_off_macos() {
    let output = helper()
        .args(["--daemon-requirement", "cdhash H\"00\""])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("serves only on macOS"), "{stderr}");
}

#[test]
fn the_requirement_flag_is_required() {
    let output = helper().output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--daemon-requirement"), "{stderr}");
}

/// Catches: a `node-admin` mode left in the binary after its removal: the word is now an
/// argument the helper does not take, refused by the flag parser before anything runs, on
/// stderr and with exit code 2, even with every required flag given.
#[test]
fn node_admin_is_not_a_mode() {
    let output = helper()
        .args([
            "node-admin",
            "phase0",
            "status",
            "--daemon-requirement",
            "cdhash H\"00\"",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("error: unexpected argument 'node-admin' found\n"),
        "{stderr}"
    );
    assert!(!stderr.contains("serves only on macOS"), "{stderr}");
}
