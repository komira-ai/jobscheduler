//! The program itself, run as a child process against the server of the test: the
//! process's own exit status.

use std::process::{Command, Output};

use crate::support::{Harness, REVOKE};

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

/// Asserts that the program prints its usage on `--help` with status 0, revokes a join
/// token on a server that admits agents with status 0 and its line on stdout, and exits 3
/// with the refusal on stderr against a server that admits none. It catches an exit status
/// that is not the process's own (a status the library returns but `main` drops).
#[test]
fn the_program_prints_help_and_revokes() {
    let help = Command::new(PROGRAM).arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("Usage: js")
    );

    let h = Harness::agents("binary_revoke");
    let revoked = program(&h, &REVOKE);
    assert_eq!(revoked.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(revoked.stdout).unwrap(),
        "join tokens for lambdalabs:i-1 revoked\n"
    );

    let h = Harness::new("binary_refused");
    let refused = program(&h, &REVOKE);
    assert_eq!(refused.status.code(), Some(3));
    assert_eq!(refused.stdout, b"");
    assert_eq!(
        String::from_utf8(refused.stderr).unwrap(),
        "js: refused: join-tokens-unsupported\n"
    );
}

/// Asserts that the program, as a process, exits 2 for the removed `lease` commands with
/// clap's message naming the unrecognized subcommand and nothing on stdout. It catches an
/// exit status that is not the process's own and a removed command that still parses.
#[test]
fn the_program_refuses_the_removed_commands() {
    let h = Harness::new("binary_removed");
    for words in [
        &["lease", "list"][..],
        &[
            "lease",
            "acquire",
            "--class",
            "gpu-host",
            "--max-wall",
            "1h",
        ],
        &["lease", "run", "--class", "gpu-host", "--", "true"],
    ] {
        let got = program(&h, words);
        assert_eq!(got.status.code(), Some(2), "{words:?}");
        assert_eq!(got.stdout, b"");
        assert!(
            String::from_utf8(got.stderr)
                .unwrap()
                .starts_with("error: unrecognized subcommand 'lease'\n"),
            "{words:?}"
        );
    }
}
