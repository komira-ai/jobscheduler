//! The program itself, run as a child process against the server of the test: the real
//! waiter and the process's own exit status.

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
    let refused = program(&h, &["lease", "release", "9-9"]);
    assert_eq!(refused.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(refused.stderr).unwrap(),
        "js: refused: unknown-lease\n"
    );
}

/// Asserts that the program, as a process, exits 2 with the message that `lease run` was
/// removed and that `lease acquire --ttl` was, prints nothing on stdout, and leaves the
/// server with no lease. It catches an exit status that is not the process's own (a status
/// the library returns but `main` drops) and a removed command that still takes a lease.
#[test]
fn the_program_refuses_the_removed_commands() {
    let h = Harness::new("binary_removed");
    let run = program(&h, &["lease", "run", "--class", "gpu-host", "--", "true"]);
    assert_eq!(run.status.code(), Some(2));
    assert_eq!(run.stdout, b"");
    assert_eq!(
        String::from_utf8(run.stderr).unwrap(),
        "js: js lease run was removed: leases are no longer renewed; submit a job\n"
    );
    let words = [
        "lease",
        "acquire",
        "--class",
        "gpu-host",
        "--max-wall",
        "1h",
        "--ttl",
        "60s",
    ];
    let ttl = program(&h, &words);
    assert_eq!(ttl.status.code(), Some(2));
    assert_eq!(ttl.stdout, b"");
    assert!(
        String::from_utf8(ttl.stderr)
            .unwrap()
            .starts_with("js: --ttl was removed: a lease has no time to live")
    );
    assert!(h.listed().is_empty());
}
