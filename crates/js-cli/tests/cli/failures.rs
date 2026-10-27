//! Answers the real server never gives, and a server that is not there: every way the
//! client can fail without a refusal.

use std::net::TcpListener;

use crate::support::{Scratch, acquire_words, against, canned, http as reply};

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
/// status and the code (or `none` when the body has none), for every route.
#[test]
fn unexpected_statuses_are_server_failures() {
    let scratch = Scratch::new("failures_status");
    let broken = canned(&reply(
        "500 Internal Server Error",
        r#"{"error":"state-write-failed"}"#,
    ));
    let empty = canned(&reply("503 Service Unavailable", ""));
    let moved = canned(&reply("302 Found", "<a>moved</a>"));
    let acquire = acquire_words(&[]);
    let routes: [&[&str]; 4] = [
        &acquire,
        &["lease", "renew", "1-1"],
        &["lease", "release", "1-1"],
        &["lease", "list"],
    ];
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

/// Asserts that a success whose body lacks a field is exit 1 naming the field: the id,
/// the host and the expiry of a start, the id of a pending one, the expiry of a renewal,
/// and the list, its entries and their fields.
#[test]
fn malformed_answers_name_the_missing_field() {
    let scratch = Scratch::new("failures_malformed");
    let acquire = acquire_words(&[]);
    let cases: [(&str, &str, &[&str], &str); 8] = [
        (
            "201 Created",
            r#"{"host":"h","expires_at_ms":5}"#,
            &acquire,
            "\"id\"",
        ),
        (
            "201 Created",
            r#"{"id":"1-1","expires_at_ms":5}"#,
            &acquire,
            "\"host\"",
        ),
        (
            "201 Created",
            r#"{"id":"1-1","host":"h"}"#,
            &acquire,
            "\"expires_at_ms\"",
        ),
        ("202 Accepted", "not json", &acquire, "\"id\""),
        (
            "200 OK",
            "{}",
            &["lease", "renew", "1-1"],
            "\"expires_at_ms\"",
        ),
        ("200 OK", "{}", &["lease", "list"], "\"leases\""),
        (
            "200 OK",
            r#"{"leases":[{"state":"held","expires_at_ms":1}]}"#,
            &["lease", "list"],
            "\"id\"",
        ),
        (
            "200 OK",
            r#"{"leases":[{"id":"1-1","state":"held"}]}"#,
            &["lease", "list"],
            "\"expires_at_ms\"",
        ),
    ];
    for (status, body, words, field) in cases {
        let url = canned(&reply(status, body));
        fails_with(&url, &scratch, words, field);
    }
}

/// Asserts that a lease entry without a state is a malformed answer too.
#[test]
fn an_entry_without_a_state_is_malformed() {
    let scratch = Scratch::new("failures_state");
    let url = canned(&reply(
        "200 OK",
        r#"{"leases":[{"id":"1-1","expires_at_ms":1}]}"#,
    ));
    fails_with(&url, &scratch, &["lease", "list"], "\"state\"");
}

/// Asserts that a connection that ends before the body is complete, and one that was
/// never accepted, are exit 1 naming the server.
#[test]
fn a_truncated_reply_and_a_closed_port_are_server_failures() {
    let scratch = Scratch::new("failures_transport");
    let cut = canned("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc");
    fails_with(
        &cut,
        &scratch,
        &["lease", "list"],
        "cannot talk to http://127.0.0.1:",
    );
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let url = format!("http://{closed}");
    fails_with(&url, &scratch, &["lease", "list"], "cannot talk to");
}

/// Asserts that a body over the reply cap is a failure rather than a read to the end.
#[test]
fn an_oversized_reply_is_a_failure() {
    let scratch = Scratch::new("failures_big");
    let body = "x".repeat(2 * 1024 * 1024);
    let url = canned(&reply("200 OK", &body));
    fails_with(&url, &scratch, &["lease", "list"], "cannot talk to");
}

/// Asserts that control characters in the error and reason a server sends (an escape
/// sequence a terminal would act on, a bell, a carriage return, a line feed) are left
/// out of what `js` prints, for a refusal, a rejection and an unexpected status, and that
/// the text around them is kept.
#[test]
fn control_characters_in_a_server_error_are_not_printed() {
    let scratch = Scratch::new("failures_control");
    let renew: &[&str] = &["lease", "renew", "1-1"];
    let cases: [(&str, &str, &[&str], &str); 3] = [
        (
            "409 Conflict",
            r#"{"error":"no\u001b[2Jcap"}"#,
            renew,
            "js: refused: no[2Jcap\n",
        ),
        (
            "422 Unprocessable Entity",
            r#"{"error":"bad\u001b]0;x\u0007","reason":"a\nb\r\u007fc"}"#,
            renew,
            "js: rejected: bad]0;x: abc\n",
        ),
        (
            "500 Internal Server Error",
            r#"{"error":"x\u001by"}"#,
            &["lease", "list"],
            "js: unexpected answer 500 (xy)\n",
        ),
    ];
    for (status, body, words, printed) in cases {
        let url = canned(&reply(status, body));
        let got = against(&url, &scratch).go(words);
        assert_eq!(got.err, printed, "{status}");
    }
}
