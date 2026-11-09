//! Answers the real server never gives, and a server that is not there: every way the
//! client can fail without a refusal.

use crate::support::{REVOKE, Scratch, against, canned, http as reply};

fn fails_with(url: &str, scratch: &Scratch, words: &[&str], message: &str) {
    let got = against(url, scratch).go(words);
    assert_eq!(
        (got.code, got.out.as_str()),
        (1, ""),
        "{words:?}: {}",
        got.err
    );
    assert!(got.err.starts_with("js: "), "{}", got.err);
    assert!(got.err.contains(message), "{words:?}: {}", got.err);
}

/// Asserts that a server error and a status the client does not know are exit 1 with the
/// status and the code (or `none` when the body has none), for both join-token routes.
#[test]
fn unexpected_statuses_are_server_failures() {
    let scratch = Scratch::new("failures_status");
    let broken = canned(&reply(
        "500 Internal Server Error",
        r#"{"error":"state-write-failed"}"#,
    ));
    let empty = canned(&reply("503 Service Unavailable", ""));
    let moved = canned(&reply("302 Found", "<a>moved</a>"));
    let mint = mint();
    let routes: [&[&str]; 2] = [&mint, &REVOKE];
    for words in routes {
        fails_with(
            &broken,
            &scratch,
            words,
            "unexpected answer 500 (state-write-failed)",
        );
        fails_with(&empty, &scratch, words, "unexpected answer 503 (none)");
        fails_with(&moved, &scratch, words, "unexpected answer 302 (none)");
    }
}

/// Asserts that a mint success whose body lacks a field, or is not JSON, is exit 1 naming
/// the field.
#[test]
fn malformed_answers_name_the_missing_field() {
    let scratch = Scratch::new("failures_malformed");
    let cases = [
        ("201 Created", r#"{"expires_at_ms":5}"#, "\"token\""),
        ("201 Created", r#"{"token":"t"}"#, "\"expires_at_ms\""),
        ("201 Created", "not json", "\"token\""),
    ];
    for (status, body, field) in cases {
        let url = canned(&reply(status, body));
        fails_with(&url, &scratch, &mint(), field);
    }
}

/// The words of a mint for `lambdalabs:i-1`.
fn mint() -> Vec<&'static str> {
    let mut words = vec!["join-token", "mint"];
    words.extend(&REVOKE[2..]);
    words
}

/// Asserts that a connection that ends before the body is complete, and one that ends
/// with no reply at all, are exit 1 naming the server. Catches a lost connection reported
/// as success or as a crash. The server stays bound, so no other process can answer in
/// its place (a bound-then-dropped port can be taken between choosing it and dialling it).
#[test]
fn a_truncated_reply_and_a_hangup_are_server_failures() {
    let scratch = Scratch::new("failures_transport");
    let cut = canned("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc");
    fails_with(&cut, &scratch, &REVOKE, "cannot talk to http://127.0.0.1:");
    let silent = canned("");
    fails_with(&silent, &scratch, &REVOKE, "cannot talk to");
}

/// Asserts that a body over the reply cap is a failure rather than a read to the end.
#[test]
fn an_oversized_reply_is_a_failure() {
    let scratch = Scratch::new("failures_big");
    let body = "x".repeat(2 * 1024 * 1024);
    let url = canned(&reply("200 OK", &body));
    fails_with(&url, &scratch, &REVOKE, "cannot talk to");
}

/// Asserts that control characters in the error and reason a server sends (an escape
/// sequence a terminal would act on, a bell, a carriage return, a line feed) are left
/// out of what `js` prints, for a refusal, a rejection and an unexpected status, and that
/// the text around them is kept.
#[test]
fn control_characters_in_a_server_error_are_not_printed() {
    let scratch = Scratch::new("failures_control");
    let release: &[&str] = &REVOKE;
    let cases: [(&str, &str, &[&str], &str); 3] = [
        (
            "409 Conflict",
            r#"{"error":"no\u001b[2Jcap"}"#,
            release,
            "js: refused: no[2Jcap\n",
        ),
        (
            "422 Unprocessable Entity",
            r#"{"error":"bad\u001b]0;x\u0007","reason":"a\nb\r\u007fc"}"#,
            release,
            "js: rejected: bad]0;x: abc\n",
        ),
        (
            "500 Internal Server Error",
            r#"{"error":"x\u001by"}"#,
            &REVOKE,
            "js: unexpected answer 500 (xy)\n",
        ),
    ];
    for (status, body, words, printed) in cases {
        let url = canned(&reply(status, body));
        let got = against(&url, &scratch).go(words);
        assert_eq!(got.err, printed, "{status}");
    }
}
