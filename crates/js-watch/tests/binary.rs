//! The program itself, run as a child process.

use std::process::Command;

/// Asserts that the binary starts, names itself and exits 0 on `--version`. It catches a
/// `main` that is not wired to the library's `run`.
#[test]
fn the_program_answers_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_js-watch"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("js-watch "), "{text}");
}
