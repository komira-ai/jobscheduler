//! `lease join-token mint` on the command line, for what is settled before any request:
//! the help, the flags clap refuses, a client token file others could read, and a terminal
//! on standard output. None of it opens a socket: `--server` names an address nothing is
//! asked of, and each test shows that by the exact refusal and its status (a connection
//! attempt would end in exit 1 with `cannot talk to`). The request, the answers and the
//! delivery of the token are tested in the crate (`src/join_tokens/tests.rs`).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use crate::support::{Never, Scratch, against, services};

/// A `--server` that parses and is never asked anything.
const NOT_ASKED: &str = "http://[::1]:9";

/// The words of a mint for `burst:i-0abc` plus `extra`.
fn words<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec![
        "lease",
        "join-token",
        "mint",
        "--provider",
        "burst",
        "--instance",
        "i-0abc",
    ];
    words.extend(extra);
    words
}

/// Asserts that the help says where the JSON and the token go with `--json`, and that the
/// server is what refuses a ttl over a day. It catches a help that leaves a user to find
/// out by losing a token.
#[test]
fn the_help_says_where_things_go_and_who_limits_the_ttl() {
    let scratch = Scratch::new("mint_help");
    let js = against(NOT_ASKED, &scratch);
    let got = js.go(&["lease", "join-token", "mint", "--help"]);
    assert_eq!(got.code, 0, "{}", got.err);
    // The help is wrapped to the width of the terminal: compare it as words.
    let words = got.out.split_whitespace().collect::<Vec<_>>().join(" ");
    for want in [
        "JSON object on standard error",
        "token stays on standard output",
        "refuses more than a day",
    ] {
        assert!(words.contains(want), "{want}: {}", got.out);
    }
}

/// Asserts that the command line takes no token, requires both ids, refuses a zero ttl and
/// refuses an empty `--out` (exit 2, clap's own text for the last) without a request. It
/// catches a flag that would put a secret in `ps`, and an empty path taken for the current
/// directory.
#[test]
fn the_command_line_has_no_token_and_needs_both_ids() {
    let scratch = Scratch::new("mint_flags");
    let js = against(NOT_ASKED, &scratch);
    let refused: [&[&str]; 5] = [
        &["--token", "x"],
        &["lease", "join-token", "mint", "--provider", "burst"],
        &["lease", "join-token", "mint", "--instance", "i-0abc"],
        &["--ttl", "0"],
        &["--out", ""],
    ];
    for extra in refused {
        let all = match extra[0] {
            "lease" => extra.to_vec(),
            _ => words(extra),
        };
        let got = js.go(&all);
        assert_eq!(
            (got.code, got.out.as_str()),
            (2, ""),
            "{extra:?}: {}",
            got.err
        );
        assert!(got.err.starts_with("error: "), "{extra:?}: {}", got.err);
    }
    let got = js.go(&words(&["--out", ""]));
    assert_eq!(
        got.err,
        "error: a value is required for '--out <PATH>' but none was supplied\n\n\
         For more information, try '--help'.\n"
    );
}

/// Asserts that a terminal on standard output is refused (exit 2) with the exact text and
/// nothing on standard output, before the client asks anything of the server. It catches a
/// token shown on a screen (scrollback, a shared terminal) and a mint that happens first.
#[test]
fn a_terminal_gets_no_token_and_no_request() {
    let scratch = Scratch::new("mint_terminal");
    let js = against(NOT_ASKED, &scratch);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let on_terminal = services(Arc::new(Never)).with_terminal_stdout(true);
    let code = js_cli::run_with(js.args(&words(&[])), &mut out, &mut err, on_terminal);
    let err = String::from_utf8(err).unwrap();
    assert_eq!((code, out.as_slice()), (2, b"".as_slice()), "{err}");
    assert_eq!(
        err,
        "js: standard output is a terminal, and a token is never printed to one: pipe it \
         (| js-agent enroll ...) or give --out PATH\n"
    );
}

/// Asserts that a client token file others can read or write is refused (status 2) with its
/// path and mode before any request, and that nothing is written to `--out`. It catches a
/// mint that sends a client token other users could have read, which would let them mint
/// join tokens of their own.
#[test]
fn a_client_token_file_others_can_read_is_refused_before_any_request() {
    let scratch = Scratch::new("mint_client_token");
    let js = against(NOT_ASKED, &scratch);
    let out = scratch.path("minted");
    for mode in [0o644, 0o640, 0o604, 0o660, 0o606] {
        fs::set_permissions(&js.token_file, fs::Permissions::from_mode(mode)).unwrap();
        let got = js.go(&words(&["--out", out.to_str().unwrap()]));
        assert_eq!(got.code, 2, "{mode:o}: {}", got.err);
        assert!(
            got.err.contains(js.token_file.to_str().unwrap())
                && got.err.contains(&format!("mode {mode:04o}"))
                && !got.err.contains("cannot talk to"),
            "{mode:o}: {}",
            got.err
        );
        assert_eq!(got.out, "");
        let got = js.go(&words(&[]));
        assert_eq!(
            (got.code, got.out.as_str()),
            (2, ""),
            "{mode:o}: {}",
            got.err
        );
    }
    assert!(!out.exists());
}

/// Asserts that an `--out` that names no file (a trailing separator, or a last component
/// of `.`) is refused (exit 2) with the exact text, before the client asks anything of the
/// server and with nothing made in the directory. It catches a token minted for a path that
/// can never be made. (Every other destination check is tested in the crate, beside the
/// delivery.)
#[test]
fn a_destination_that_names_no_file_is_refused_before_any_request() {
    let scratch = Scratch::new("mint_out_names");
    let js = against(NOT_ASKED, &scratch);
    fs::create_dir(scratch.path("sub")).unwrap();
    let base = scratch.path("");
    let base = base.to_str().unwrap().trim_end_matches('/').to_owned();
    let cases = [
        (
            format!("{base}/sub/"),
            format!("js: --out: cannot use \"{base}/sub/\": it ends in a separator\n"),
        ),
        (
            format!("{base}/sub/."),
            format!("js: --out: cannot use \"{base}/sub/.\": it ends in `.`\n"),
        ),
    ];
    for (path, want) in cases {
        let got = js.go(&words(&["--out", &path]));
        assert_eq!((got.code, got.out.as_str()), (2, ""), "{path:?}");
        assert_eq!(got.err, want, "{path:?}");
    }
    assert_eq!(fs::read_dir(scratch.path("sub")).unwrap().count(), 0);
}
