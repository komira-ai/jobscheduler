//! The join token: read from standard input only, checked for the form the server mints,
//! written once, and kept out of everything the verb prints.

use std::ffi::OsString;
use std::io::{self, Read};

use js_agent::enroll::{Token, TokenError};

use super::rig::{OTHER, Rig, TOKEN, sha, status};

/// Asserts that the token's form is the server's: 64 lower-case hex digits and at most one
/// line break; the sha256 is the one of the digits. It catches a form that is looser than the
/// server's (and so writes a file the server's token would never be), or stricter (which
/// refuses a real token).
#[test]
fn the_token_form_is_the_servers() {
    for good in [TOKEN, OTHER, &"0".repeat(64)] {
        for text in [good.to_owned(), format!("{good}\n")] {
            let token = Token::parse(text.as_bytes()).unwrap();
            assert_eq!(token.sha256(), sha(good));
        }
    }
    let long_enough = "a".repeat(65);
    let twice = format!("{TOKEN}\n\n");
    let two_lines = format!("{TOKEN}\n{TOKEN}\n");
    let upper = TOKEN.to_uppercase();
    let not_hex = format!("{}g", &TOKEN[..63]);
    let crlf = format!("{}\r\n", &TOKEN[..63]);
    let cases: [(&str, &str); 9] = [
        ("", "is empty"),
        ("\n", "is empty"),
        ("abc", "is not 64 characters long"),
        (&long_enough, "is not 64 characters long"),
        (&twice, "has more than one line"),
        (&two_lines, "has more than one line"),
        (&upper, "is not lower-case hex digits"),
        (&not_hex, "is not lower-case hex digits"),
        (&crlf, "is not lower-case hex digits"),
    ];
    for (text, why) in cases {
        let err = Token::parse(text.as_bytes()).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("the token on standard input {why}"),
            "{text:?}"
        );
    }
}

/// Asserts that the token's `Debug` text, and the text of every refusal it caused, hold no
/// digit of it. It catches a derived `Debug` and an error that quotes its input.
#[test]
fn the_token_is_in_no_debug_or_error_text() {
    let token = Token::parse(TOKEN.as_bytes()).unwrap();
    assert_eq!(format!("{token:?}"), "Token(<redacted>)");
    let bad = format!("{}g", &TOKEN[..63]);
    let err = Token::parse(bad.as_bytes()).unwrap_err();
    for text in [format!("{err}"), format!("{err:?}")] {
        assert!(!text.contains(&TOKEN[..16]), "{text}");
    }
}

/// A standard input that fails to be read.
struct Broken;

impl Read for Broken {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("the pipe broke"))
    }
}

/// Asserts that a standard input that cannot be read is a refusal with the system's words,
/// and that `read` stops after a bounded number of bytes (a stream that never ends does not
/// fill memory). It catches an unbounded read and a swallowed read error.
#[test]
fn reading_standard_input_is_bounded_and_reports_its_errors() {
    let err = Token::read(&mut Broken).unwrap_err();
    assert!(matches!(err, TokenError::Read(_)));
    assert_eq!(
        err.to_string(),
        "cannot read standard input: the pipe broke"
    );
    let mut endless = io::repeat(b'a');
    let err = Token::read(&mut endless).unwrap_err();
    assert_eq!(
        err.to_string(),
        "the token on standard input is not 64 characters long"
    );
    let mut good = format!("{TOKEN}\n").into_bytes();
    good.extend_from_slice(&[b'x'; 3]);
    assert!(Token::read(&mut good.as_slice()).is_err());
}

/// Asserts that every malformed token is a refusal (status 2) that writes nothing, does not
/// run the setup (the token is read first, so a mistake costs nothing), and does not repeat
/// the input. It catches a bad token that is written, and one that is echoed.
#[test]
fn a_malformed_token_is_refused_before_anything_is_done() {
    let wrong = format!("{}g", &TOKEN[..63]);
    let upper = TOKEN.to_uppercase();
    for text in ["", "abc\n", wrong.as_str(), upper.as_str()] {
        let rig = Rig::new("enroll_bad_token");
        let ran = rig.go(&["--apply"], text);
        assert_eq!(ran.status, status(2), "{text:?}: {ran:?}");
        assert!(
            ran.err.contains("the token on standard input"),
            "{text:?}: {ran:?}"
        );
        assert!(
            text.is_empty() || !ran.err.contains(text.trim_end()),
            "{ran:?}"
        );
        assert_eq!(ran.out, "");
        assert_eq!(rig.setup_args(), None);
        assert_eq!(rig.names(), Vec::<String>::new());
    }
}

/// Asserts that a standard input that is a terminal is refused (status 2) with the way to
/// pipe a token in, without reading it. It catches a verb that waits on the keyboard for a
/// secret, or one that echoes what is typed.
#[test]
fn a_terminal_on_standard_input_is_refused() {
    let mut rig = Rig::new("enroll_terminal");
    rig.host.stdin_is_terminal = true;
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(
        ran.err.contains("standard input is a terminal") && ran.err.contains("join-token mint"),
        "{ran:?}"
    );
    assert_eq!(rig.setup_args(), None);
    assert_eq!(rig.names(), Vec::<String>::new());
}

/// Asserts that the token reaches the host through standard input only: the setup script is
/// called with no argument that holds a digit of it, the report holds its sha256 and not the
/// token, and no file but the token file holds it. It catches a token passed on a command
/// line, where `ps` shows it to every user.
#[test]
fn the_token_is_on_no_command_line_and_in_no_output() {
    let rig = Rig::new("enroll_token_hidden");
    let ran = rig.go(
        &["--apply", "--server", "https://leases.example:7300"],
        TOKEN,
    );
    assert_eq!(ran.status, status(0), "{ran:?}");
    assert!(ran.out.contains(&sha(TOKEN)));
    let argv = rig.setup_args().unwrap().join(" ");
    assert!(
        !argv.contains(&TOKEN[..16]) && !argv.contains(&sha(TOKEN)[..16]),
        "{argv}"
    );
    let words: Vec<OsString> = vec![
        "js-agent".into(),
        "enroll".into(),
        "--token".into(),
        TOKEN.into(),
    ];
    let mut input = TOKEN.as_bytes();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = js_agent::enroll::run(&words, &mut input, &mut out, &mut err, &rig.host);
    assert_eq!(format!("{code:?}"), status(2), "there is no --token flag");
}
