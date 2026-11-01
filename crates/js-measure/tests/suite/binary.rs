//! The program itself, run as a child process.

use std::process::Command;

/// Asserts that the binary starts, names itself and exits 0 on `--version`. It catches a
/// `main` that is not wired to the library.
#[test]
fn the_program_answers_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_js-measure"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("js-measure "), "{text}");
}

/// Asserts that a failing command exits nonzero with the reason on standard error, naming
/// the program, and nothing on standard output.
#[test]
fn a_failure_exits_nonzero_and_says_why() {
    let output = Command::new(env!("CARGO_BIN_EXE_js-measure"))
        .args(["summary", "--records", "/nonexistent/records.jsonl"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(
        text.starts_with("js-measure: /nonexistent/records.jsonl: "),
        "{text}"
    );
}
