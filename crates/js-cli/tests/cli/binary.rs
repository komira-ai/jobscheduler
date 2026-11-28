//! The program itself, run as a child process: the process's own exit status.

use std::process::{Command, Output};

use crate::support::{Js, Scratch, against, hangup_port};

const PROGRAM: &str = env!("CARGO_BIN_EXE_js");

fn program(js: &Js, words: &[&str]) -> Output {
    Command::new(PROGRAM)
        .args(["--server", &js.server])
        .arg("--token-file")
        .arg(&js.token_file)
        .args(words)
        .output()
        .unwrap()
}

/// Asserts that the program prints its usage on `--help` with status 0, and exits 1 with
/// nothing on stdout and its reason on stderr when the TLS handshake fails before any request
/// is written (a server that hangs up on the connection). It catches an exit status that
/// is not the process's own (a status the library returns but `main` drops).
#[test]
fn the_program_prints_help_and_exits_1_when_no_server_answers() {
    let help = Command::new(PROGRAM).arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("Usage: js")
    );

    let url = format!("https://127.0.0.1:{}", hangup_port());
    let failed = program(
        &against(&url, &Scratch::new("binary_hangup")),
        &["pool", "list"],
    );
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(failed.stdout, b"");
    assert!(
        String::from_utf8(failed.stderr)
            .unwrap()
            .starts_with("js: "),
        "the reason is on stderr"
    );
}

/// Asserts that the program, as a process, exits 2 for the removed `lease` and `join-token`
/// commands with clap's message naming the unrecognized subcommand and nothing on stdout.
/// It catches an exit status that is not the process's own and a removed command that
/// still parses.
#[test]
fn the_program_refuses_the_removed_commands() {
    let scratch = Scratch::new("binary_removed");
    let js = against("http://127.0.0.1:1", &scratch);
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
        &[
            "join-token",
            "mint",
            "--provider",
            "p",
            "--instance-id",
            "i",
        ],
        &[
            "join-token",
            "revoke",
            "--provider",
            "p",
            "--instance-id",
            "i",
        ],
    ] {
        let got = program(&js, words);
        assert_eq!(got.status.code(), Some(2), "{words:?}");
        assert_eq!(got.stdout, b"");
        assert!(
            String::from_utf8(got.stderr)
                .unwrap()
                .starts_with(&format!("error: unrecognized subcommand '{}'\n", words[0])),
            "{words:?}"
        );
    }
}
