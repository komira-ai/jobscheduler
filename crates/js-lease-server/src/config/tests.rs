//! Unit tests of the configuration: the token and its file, and the default limits.

use super::*;

/// Asserts that a token compares equal only to itself and prints without its bytes.
#[test]
fn a_token_matches_itself_and_hides_itself() {
    let token = Token(b"s3cret".to_vec());
    assert!(token.matches(b"s3cret"));
    assert!(!token.matches(b"s3creT"));
    assert!(!token.matches(b"s3cret!"));
    assert!(!token.matches(b""));
    assert_eq!(format!("{token:?}"), "Token(..)");
}

/// Asserts that a token is the content without trailing whitespace, and that empty
/// content and any space, line break, control or non-ASCII byte inside the token
/// is refused: no header value could ever equal such a token.
#[test]
fn a_token_is_one_run_of_printable_ascii() {
    let good = "abcdefghijklmnopqrstuvwxyz-0123456789";
    let trailing = format!("{good}\r\n");
    assert!(
        parse_token(trailing.as_bytes())
            .unwrap()
            .matches(good.as_bytes())
    );
    assert_eq!(parse_token(b" \r\n").err().as_deref(), Some("is empty"));
    for bad in [
        &b"two\nlines\n"[..],
        b"has space",
        b" leading",
        b"tab\tinside",
        b"nul\0byte",
        "caf\u{e9}".as_bytes(),
    ] {
        let reason = parse_token(bad).err().unwrap();
        assert!(reason.contains("printable ASCII"), "{bad:?}");
    }
}

/// Asserts that a token of 31 bytes is refused and one of 32 is accepted, counting
/// the bytes without the trailing whitespace, and that the refusal says how long
/// the token was and how long it must be.
#[test]
fn a_token_must_have_at_least_32_bytes() {
    let short = "x".repeat(MIN_TOKEN_BYTES - 1);
    let reason = parse_token(format!("{short}\n\n").as_bytes())
        .err()
        .unwrap();
    assert!(reason.contains("31 bytes"), "{reason}");
    assert!(reason.contains("at least 32"), "{reason}");
    let enough = "x".repeat(MIN_TOKEN_BYTES);
    assert!(parse_token(enough.as_bytes()).is_ok());
}

/// Asserts that the default limits are what the flags default to, so the two
/// cannot drift apart.
#[test]
fn the_default_limits_are_the_flag_defaults() {
    let cli = Cli::try_parse_from([
        "js-lease-server",
        "--state-file=a",
        "--boot=resume",
        "--pool-config=b",
        "--token-file=c",
    ])
    .unwrap();
    assert_eq!(Limits::from(&cli), Limits::default());
}
