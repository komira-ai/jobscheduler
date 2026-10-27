//! The client against servers that answer canned replies.

use std::path::PathBuf;
use std::time::Duration;

use js_client::{Client, ClientError, REQUEST_LIMIT, Reply};
use serde_json::{Value, json};

use crate::support::{Cert, Fake, Scratch, closed_port, json_reply, text_reply};

const LIMIT: Duration = Duration::from_secs(20);

fn token_file(dir: &Scratch) -> PathBuf {
    dir.write("token", b"tok-123\n", 0o600)
}

fn refused(connected: Result<Client, ClientError>) -> String {
    connected.err().expect("connect was refused").to_string()
}

fn talk(client: &Client, path: &str) -> Reply {
    client.get(path).unwrap()
}

/// Asserts that the four calls send their method, path, token and JSON body, and return
/// the status and the parsed body.
#[test]
fn requests_carry_the_token_and_replies_are_parsed() {
    let dir = Scratch::new("client_calls");
    let fake = Fake::plain(vec![
        json_reply(200, r#"{"leases":[]}"#),
        json_reply(201, r#"{"id":"1-1"}"#),
        json_reply(202, r#"{"ok":true}"#),
        json_reply(409, r#"{"error":"not-held"}"#),
    ]);
    let client = Client::connect(&fake.url("http"), &token_file(&dir), None, LIMIT).unwrap();

    let got = client.get("/v1/leases").unwrap();
    assert_eq!((got.status, got.body), (200, json!({ "leases": [] })));
    let got = client.post("/v1/leases", &json!({ "a": 1 })).unwrap();
    assert_eq!((got.status, got.body), (201, json!({ "id": "1-1" })));
    let body = json!({ "ttl_seconds": 5 });
    let got = client
        .clone()
        .post_within("/v1/leases/1-1/renew", &body, Duration::from_secs(5))
        .unwrap();
    assert_eq!((got.status, got.body), (202, json!({ "ok": true })));
    let got = client.delete("/v1/leases/1-1").unwrap();
    assert_eq!(
        (got.status, got.body),
        (409, json!({ "error": "not-held" }))
    );

    let seen = fake.requests();
    assert_eq!(seen.len(), 4);
    let heads: Vec<_> = seen.iter().map(|r| r.to_ascii_lowercase()).collect();
    for (head, first) in heads.iter().zip([
        "get /v1/leases http/1.1",
        "post /v1/leases http/1.1",
        "post /v1/leases/1-1/renew http/1.1",
        "delete /v1/leases/1-1 http/1.1",
    ]) {
        assert!(head.starts_with(first), "{head}");
        assert!(head.contains("authorization: bearer tok-123\r\n"), "{head}");
    }
    assert!(
        heads[1].contains("content-type: application/json"),
        "{}",
        heads[1]
    );
    assert!(seen[1].ends_with(r#"{"a":1}"#), "{}", seen[1]);
    assert!(seen[2].ends_with(r#"{"ttl_seconds":5}"#), "{}", seen[2]);
}

/// A reply with one extra header line.
fn reply_with_header(status: u16, header: &str) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {status} Reply\r\n{header}\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
    );
    head.into_bytes()
}

/// Asserts that `Retry-After` in whole seconds is read, and that an absent header, the
/// date form and a text that is not a number read as no wait. It catches a client that
/// drops the header (so a caller retries into a rate limit at once) and one that takes a
/// date for seconds.
#[test]
fn retry_after_is_read_in_whole_seconds_only() {
    let dir = Scratch::new("client_retry_after");
    let fake = Fake::plain(vec![
        reply_with_header(429, "retry-after: 7"),
        reply_with_header(429, "retry-after:  3 "),
        json_reply(200, "{}"),
        reply_with_header(429, "retry-after: Wed, 21 Oct 2026 07:28:00 GMT"),
        reply_with_header(429, "retry-after: -1"),
    ]);
    let client = Client::connect(&fake.url("http"), &token_file(&dir), None, LIMIT).unwrap();
    let waits: Vec<Option<Duration>> = (0..5).map(|_| talk(&client, "/x").retry_after).collect();
    let secs = |n| Some(Duration::from_secs(n));
    assert_eq!(waits, [secs(7), secs(3), None, None, None]);
    fake.requests();
}

/// Asserts that a body that is empty or not JSON reads as `null`, with the status kept.
#[test]
fn a_body_that_is_not_json_reads_as_null() {
    let dir = Scratch::new("client_null");
    let fake = Fake::plain(vec![
        text_reply(502, "text/plain", "bad gateway"),
        text_reply(204, "text/plain", ""),
    ]);
    let client = Client::connect(&fake.url("http"), &token_file(&dir), None, LIMIT).unwrap();
    let first = talk(&client, "/x");
    assert_eq!((first.status, first.body), (502, Value::Null));
    let second = talk(&client, "/y");
    assert_eq!((second.status, second.body), (204, Value::Null));
    fake.requests();
}

/// Asserts that a server nobody listens on is a server failure naming its address and
/// never the token.
#[test]
fn an_unreachable_server_is_a_server_failure() {
    let dir = Scratch::new("client_down");
    let url = closed_port();
    let client = Client::connect(&url, &token_file(&dir), None, REQUEST_LIMIT).unwrap();
    let err = client.get("/v1/leases").err().unwrap();
    assert!(matches!(err, ClientError::Server(_)), "{err:?}");
    let said = err.to_string();
    assert!(
        said.starts_with(&format!("cannot talk to {url}: ")),
        "{said}"
    );
    assert!(!said.contains("tok-123"), "{said}");
}

/// Asserts that a reply over the 1 MiB read limit is a server failure, not a truncated
/// answer.
#[test]
fn a_reply_over_the_limit_is_a_server_failure() {
    let dir = Scratch::new("client_big");
    let big = "a".repeat(1024 * 1024 + 1);
    let fake = Fake::plain(vec![text_reply(200, "text/plain", &big)]);
    let client = Client::connect(&fake.url("http"), &token_file(&dir), None, LIMIT).unwrap();
    let err = client.get("/big").err().unwrap();
    assert!(matches!(err, ClientError::Server(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("cannot talk to http://127.0.0.1:"),
        "{err}"
    );
    fake.requests();
}

/// Asserts that a bad `--server` is refused before the token file is read, and that a
/// bad token file is refused by name.
#[test]
fn the_flags_are_checked_in_order() {
    let dir = Scratch::new("client_flags");
    let missing = dir.path("missing");
    assert_eq!(
        refused(Client::connect("http://example.com", &missing, None, LIMIT)),
        "--server: \"http://example.com\" is plain http to a host off the loopback: a bearer token never crosses a network in clear, use https://"
    );
    let said = refused(Client::connect("http://localhost:1", &missing, None, LIMIT));
    assert!(
        said.starts_with(&format!(
            "--token-file: {} cannot be read",
            missing.display()
        )),
        "{said}"
    );
}

/// Asserts that an `https://` server needs a readable `--ca-cert` holding a certificate,
/// and that `--ca-cert` is not read for plain http.
#[test]
fn the_ca_cert_rules() {
    let dir = Scratch::new("client_ca_rules");
    let token = token_file(&dir);
    assert_eq!(
        refused(Client::connect("https://localhost:1", &token, None, LIMIT)),
        "--ca-cert: required for an https:// server"
    );
    let missing = dir.path("missing.pem");
    let said = refused(Client::connect(
        "https://localhost:1",
        &token,
        Some(&missing),
        LIMIT,
    ));
    assert!(
        said.starts_with(&format!("--ca-cert: {}: ", missing.display())),
        "{said}"
    );
    let empty = dir.write("empty.pem", b"nothing here\n", 0o644);
    assert_eq!(
        refused(Client::connect(
            "https://localhost:1",
            &token,
            Some(&empty),
            LIMIT
        )),
        format!("--ca-cert: {}: no certificate found", empty.display())
    );
    assert!(Client::connect("http://localhost:1", &token, Some(&missing), LIMIT).is_ok());
}

/// Asserts that over TLS the client trusts the certificate it was given, and no other.
#[test]
fn tls_trusts_only_the_given_certificate() {
    let dir = Scratch::new("client_tls");
    let token = token_file(&dir);
    let cert = Cert::new();
    let other = Cert::new();
    let ca = dir.write("ca.pem", cert.pem.as_bytes(), 0o644);
    let other_ca = dir.write("other.pem", other.pem.as_bytes(), 0o644);

    let fake = Fake::tls(vec![json_reply(200, r#"{"secure":true}"#)], &cert);
    let client = Client::connect(&fake.url("https"), &token, Some(&ca), LIMIT).unwrap();
    let got = talk(&client, "/v1/leases");
    assert_eq!((got.status, got.body), (200, json!({ "secure": true })));
    assert!(
        fake.requests()[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer tok-123")
    );

    let fake = Fake::tls(vec![json_reply(200, "{}")], &cert);
    let client = Client::connect(&fake.url("https"), &token, Some(&other_ca), LIMIT).unwrap();
    let err = client.get("/v1/leases").err().unwrap();
    assert!(matches!(err, ClientError::Server(_)), "{err:?}");
    // The handshake failed, so the server read no request and never saw the token.
    assert_eq!(fake.requests(), vec![String::new()]);
}

/// Asserts the text of the error kinds.
#[test]
fn errors_print_as_their_text() {
    let flag = ClientError::Flag {
        flag: "--server",
        why: "is required".to_owned(),
    };
    assert_eq!(flag.to_string(), "--server: is required");
    assert_eq!(ClientError::Server("down".to_owned()).to_string(), "down");
}

/// Asserts that the debug text of a reply shows the status and the wait but not the body,
/// which can hold a session secret. It catches a derived `Debug` that prints the body.
#[test]
fn a_reply_prints_without_its_body() {
    let reply = Reply {
        status: 200,
        body: json!({ "session": "sess-secret-0001" }),
        retry_after: Some(Duration::from_secs(3)),
    };
    let text = format!("{reply:?}");
    assert!(!text.contains("sess-secret-0001"), "{text}");
    assert_eq!(
        text,
        "Reply { status: 200, body: <redacted>, retry_after: Some(3s) }"
    );
}
