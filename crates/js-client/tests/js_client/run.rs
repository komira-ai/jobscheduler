//! Reading one run against servers that answer the entries the lease server documents.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use js_client::{Client, LogTail, Outcome, RunRead, RunReadError};

use crate::support::{Fake, Scratch, hangup_url, json_reply, text_reply};

const LIMIT: Duration = Duration::from_secs(20);

fn client_of(dir: &Scratch, url: &str) -> Client {
    let token: PathBuf = dir.write("token", b"tok-123\n", 0o600);
    Client::connect(url, &token, None, LIMIT).unwrap()
}

/// Asserts that `run` asks `GET /v1/leases/{id}/runs/{run_id}` with the token, and reads
/// every field of the entry of a contained run that exited, its tail included. It catches
/// a wrong path, a missing token, and a field dropped or renamed.
#[test]
fn a_run_with_a_tail_is_read_whole() {
    let dir = Scratch::new("run_tail");
    let fake = Fake::plain(vec![json_reply(
        200,
        r#"{"run_id":"r-9","state":"exited","outcome":"succeeded","exit_code":0,"detail":"ok","labels":{"cell":"q6"},"log_tail":{"stdout":"hello\n","stderr":"warn\n","truncated":false}}"#,
    )]);
    let client = client_of(&dir, &fake.url("http"));

    let run = client.run("1-1", "r-9").unwrap();
    assert_eq!(
        run,
        RunRead {
            run_id: "r-9".to_owned(),
            state: "exited".to_owned(),
            outcome: Some(Outcome::Succeeded),
            exit_code: Some(0),
            detail: Some("ok".to_owned()),
            labels: BTreeMap::from([("cell".to_owned(), "q6".to_owned())]),
            log_tail: Some(LogTail {
                stdout: "hello\n".to_owned(),
                stderr: "warn\n".to_owned(),
                truncated: false,
            }),
        }
    );

    let seen = fake.requests();
    let head = seen[0].to_ascii_lowercase();
    assert!(
        head.starts_with("get /v1/leases/1-1/runs/r-9 http/1.1"),
        "{head}"
    );
    assert!(head.contains("authorization: bearer tok-123\r\n"), "{head}");
}

/// Asserts that an entry with no `log_tail` (a queued run, or one whose host could not
/// read the output) reads as `None`, not as an empty tail, and that the optional fields
/// the server leaves out read as `None` or empty.
#[test]
fn a_run_without_a_tail_has_none() {
    let dir = Scratch::new("run_no_tail");
    let fake = Fake::plain(vec![json_reply(
        200,
        r#"{"run_id":"r-1","state":"queued","a_field_added_later":true}"#,
    )]);
    let run = client_of(&dir, &fake.url("http"))
        .run("1-1", "r-1")
        .unwrap();
    assert_eq!(
        run,
        RunRead {
            run_id: "r-1".to_owned(),
            state: "queued".to_owned(),
            outcome: None,
            exit_code: None,
            detail: None,
            labels: BTreeMap::new(),
            log_tail: None,
        }
    );
    fake.requests();
}

/// Asserts that `truncated: true` is kept with both streams, that a tail that leaves out
/// its streams reads as empty text, and that a failed run keeps its exit code and outcome.
#[test]
fn a_truncated_tail_says_so() {
    let dir = Scratch::new("run_truncated");
    let fake = Fake::plain(vec![
        json_reply(
            200,
            r#"{"run_id":"r-2","state":"ended","outcome":"failed","exit_code":3,"log_tail":{"stdout":"tail","stderr":"boom","truncated":true}}"#,
        ),
        json_reply(
            200,
            r#"{"run_id":"r-3","state":"exited","log_tail":{"truncated":true}}"#,
        ),
    ]);
    let client = client_of(&dir, &fake.url("http"));

    let first = client.run("1-1", "r-2").unwrap();
    assert_eq!(
        (first.outcome, first.exit_code),
        (Some(Outcome::Failed), Some(3))
    );
    assert_eq!(
        first.log_tail,
        Some(LogTail {
            stdout: "tail".to_owned(),
            stderr: "boom".to_owned(),
            truncated: true,
        })
    );
    let second = client.run("1-1", "r-3").unwrap();
    assert_eq!(
        second.log_tail,
        Some(LogTail {
            stdout: String::new(),
            stderr: String::new(),
            truncated: true,
        })
    );
    fake.requests();
}

/// Asserts that the three documented refusals are three typed errors that name the id
/// asked, and that 404 with another code is not mistaken for either 404.
#[test]
fn the_documented_refusals_are_typed() {
    let dir = Scratch::new("run_refusals");
    let fake = Fake::plain(vec![
        json_reply(404, r#"{"error":"unknown-lease"}"#),
        json_reply(404, r#"{"error":"unknown-run"}"#),
        json_reply(410, r#"{"error":"run-forgotten"}"#),
        json_reply(404, r#"{"error":"something-new"}"#),
    ]);
    let client = client_of(&dir, &fake.url("http"));

    let err = client.run("9-9", "r-1").unwrap_err();
    assert!(
        matches!(&err, RunReadError::UnknownLease(id) if id == "9-9"),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "the server has no lease 9-9");
    let err = client.run("1-1", "r-7").unwrap_err();
    assert!(
        matches!(&err, RunReadError::UnknownRun(id) if id == "r-7"),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "the lease has no run r-7");
    let err = client.run("1-1", "r-5").unwrap_err();
    assert!(
        matches!(&err, RunReadError::Forgotten(id) if id == "r-5"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "run r-5 was forgotten: the server restarted and its work is lost"
    );
    let err = client.run("1-1", "r-6").unwrap_err();
    assert!(
        matches!(&err, RunReadError::Refused { status: 404, code } if code == "something-new"),
        "{err:?}"
    );
    fake.requests();
}

/// Asserts that any other status is `Refused` with its code (empty when the body has
/// none), that a 200 which is not a run entry is `Malformed`, and that a 404 code on the
/// wrong status (a 200 saying `unknown-run`) is not taken for a refusal.
#[test]
fn other_answers_are_refused_or_malformed() {
    let dir = Scratch::new("run_other");
    let fake = Fake::plain(vec![
        json_reply(409, r#"{"error":"not-held"}"#),
        text_reply(502, "text/plain", "bad gateway"),
        json_reply(200, r#"{"run_id":"r-1"}"#),
        json_reply(200, r#"{"error":"unknown-run"}"#),
        json_reply(410, r#"{"error":"unknown-run"}"#),
    ]);
    let client = client_of(&dir, &fake.url("http"));

    let err = client.run("1-1", "r-1").unwrap_err();
    assert!(
        matches!(&err, RunReadError::Refused { status: 409, code } if code == "not-held"),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "the server answered 409 not-held");
    let err = client.run("1-1", "r-1").unwrap_err();
    assert!(
        matches!(&err, RunReadError::Refused { status: 502, code } if code.is_empty()),
        "{err:?}"
    );
    let err = client.run("1-1", "r-1").unwrap_err();
    assert!(
        matches!(&err, RunReadError::Malformed(why) if why.contains("state")),
        "{err:?}"
    );
    assert!(
        err.to_string()
            .starts_with("the run entry cannot be read: "),
        "{err}"
    );
    let err = client.run("1-1", "r-1").unwrap_err();
    assert!(matches!(&err, RunReadError::Malformed(_)), "{err:?}");
    let err = client.run("1-1", "r-1").unwrap_err();
    assert!(
        matches!(&err, RunReadError::Refused { status: 410, code } if code == "unknown-run"),
        "{err:?}"
    );
    fake.requests();
}

/// Asserts that a server that cannot be reached is `Client`, with the client's own text
/// and never the token.
#[test]
fn an_unreachable_server_is_a_client_error() {
    let dir = Scratch::new("run_down");
    let url = hangup_url();
    let err = client_of(&dir, &url).run("1-1", "r-1").unwrap_err();
    assert!(matches!(err, RunReadError::Client(_)), "{err:?}");
    let said = err.to_string();
    assert!(
        said.starts_with(&format!("cannot talk to {url}: ")),
        "{said}"
    );
    assert!(!said.contains("tok-123"), "{said}");
}

/// The reply of a run that has not started, for the tests that must show whether a
/// request reached the server.
fn queued_reply() -> Vec<u8> {
    json_reply(200, r#"{"run_id":"run-3-1","state":"queued"}"#)
}

/// Asserts that a `lease_id` holding `/`, `?`, `#`, `..`, `%` or nothing is refused as
/// `BadId("lease_id")` before any request is made: the fake has one reply, a refused id
/// that sent a request would use it up, and the one request the fake saw at the end is the
/// good call's. It catches an id put into the path unchecked, where it would turn the
/// GET into another route carrying the token.
#[test]
fn a_lease_id_outside_the_minted_form_sends_nothing() {
    let dir = Scratch::new("run_bad_lease");
    let fake = Fake::plain(vec![queued_reply()]);
    let client = client_of(&dir, &fake.url("http"));

    for bad in ["1/2", "1?x", "1#x", "..", "1%2f1", ""] {
        let err = client.run(bad, "run-3-1").unwrap_err();
        assert!(
            matches!(err, RunReadError::BadId("lease_id")),
            "{bad:?}: {err:?}"
        );
    }
    client.run("12-345", "run-3-1").unwrap();
    let seen = fake.requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].starts_with("GET /v1/leases/12-345/runs/run-3-1 HTTP/1.1"),
        "{seen:?}"
    );
}

/// Asserts the same for `run_id`: each of `/`, `?`, `#`, `..`, `%` and the empty string is
/// `BadId("run_id")` and no request reaches the fake.
#[test]
fn a_run_id_outside_the_minted_form_sends_nothing() {
    let dir = Scratch::new("run_bad_run");
    let fake = Fake::plain(vec![queued_reply()]);
    let client = client_of(&dir, &fake.url("http"));

    for bad in ["run/1", "run?x", "run#x", "..", "run%2f1", ""] {
        let err = client.run("12-345", bad).unwrap_err();
        assert!(
            matches!(err, RunReadError::BadId("run_id")),
            "{bad:?}: {err:?}"
        );
    }
    client.run("12-345", "run-3-1").unwrap();
    let seen = fake.requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].starts_with("GET /v1/leases/12-345/runs/run-3-1 HTTP/1.1"),
        "{seen:?}"
    );
}

/// Asserts that an error for a mistyped field names the field and holds none of the
/// value: a secret-looking string in `exit_code` and one in `log_tail.truncated` appear
/// neither in the error's text nor in its debug form. It catches the parser's own message
/// being carried, which quotes the value it found.
#[test]
fn a_malformed_entry_error_holds_no_value_from_the_body() {
    let dir = Scratch::new("run_malformed_secret");
    let fake = Fake::plain(vec![
        json_reply(
            200,
            r#"{"run_id":"r-1","state":"exited","exit_code":"s3cret-token-value"}"#,
        ),
        json_reply(
            200,
            r#"{"run_id":"r-1","state":"exited","log_tail":{"stdout":"x","stderr":"y","truncated":"hunter2-secret"}}"#,
        ),
        json_reply(200, r#"{"run_id":"r-1","state":"exited","outcome":"s3cret-outcome"}"#),
    ]);
    let client = client_of(&dir, &fake.url("http"));

    for (secret, field) in [
        ("s3cret-token-value", "exit_code"),
        ("hunter2-secret", "log_tail.truncated"),
        ("s3cret-outcome", "outcome"),
    ] {
        let err = client.run("1-1", "r-1").unwrap_err();
        assert!(matches!(err, RunReadError::Malformed(_)), "{err:?}");
        for text in [err.to_string(), format!("{err:?}")] {
            assert!(!text.contains(secret), "{text}");
            assert!(text.contains(field), "{field}: {text}");
        }
    }
    fake.requests();
}
