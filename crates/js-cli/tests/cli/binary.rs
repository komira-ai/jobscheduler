//! The program itself, run as a child process against the server of the test: the real
//! waiter, the real signal handlers and the process's own exit status.

use std::process::{Command, Output};

use crate::support::Harness;

const PROGRAM: &str = env!("CARGO_BIN_EXE_js");

fn program(h: &Harness, words: &[&str]) -> Output {
    Command::new(PROGRAM)
        .args(["--server", &h.js.server])
        .arg("--token-file")
        .arg(&h.js.token_file)
        .args(words)
        .output()
        .unwrap()
}

/// Asserts that the program prints its usage on `--help` with status 0, and lists the
/// leases of the server with the status 0.
#[test]
fn the_program_prints_help_and_lists() {
    let h = Harness::new("binary_list");
    let help = Command::new(PROGRAM).arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("Usage: js")
    );

    let listed = program(&h, &["--json", "lease", "list"]);
    assert_eq!(listed.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(listed.stdout).unwrap(),
        "{\"leases\":[]}\n"
    );
    let refused = program(&h, &["lease", "renew", "9-9"]);
    assert_eq!(refused.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(refused.stderr).unwrap(),
        "js: refused: unknown-lease\n"
    );
}

/// Asserts that a SIGTERM sent to the program while it runs a command (the command sends
/// it, to its parent) is forwarded to the command, the program exits 143 and the lease is
/// released, and that the program renews through its own waiter without waiting for it.
#[test]
fn the_program_forwards_sigterm_and_releases() {
    let h = Harness::new("binary_run");
    let script = "kill -TERM $PPID; exec sleep 600";
    let ran = program(
        &h,
        &[
            "lease",
            "run",
            "--class",
            "gpu-host",
            "--gpus",
            "1",
            "--gpu-class",
            "nvidia-a100",
            "--max-wall",
            "2h",
            "--ttl",
            "60s",
            "--",
            "sh",
            "-c",
            script,
        ],
    );
    assert_eq!(
        ran.status.code(),
        Some(143),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert!(
        String::from_utf8(ran.stderr)
            .unwrap()
            .starts_with("lease 1-1 host gpu-01 ")
    );
    assert!(h.listed().is_empty());
}
