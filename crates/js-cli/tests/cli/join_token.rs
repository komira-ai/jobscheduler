//! `join-token mint` and `join-token revoke`: against a real lease server that admits
//! agents (the route exists only then), and against a server that records what it reads
//! and answers a canned reply, for the bytes sent and the answers the real server never
//! gives.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use serde_json::json;

use crate::native::recording;
use crate::support::{Harness, Js, Scratch, T0, against, http};

/// A pool that asks nothing of a host but an `arch` (the scheduler describes a host by
/// it), for the server's `--agent-pool-config`. No host says hello here.
const POOL: &str = r#"
name = "macs"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 1
memory_bytes = 1

[capabilities]
arch = "x86_64"
"#;

/// A real server whose agents register hosts, so the join-token route exists.
fn agent_server(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let pool = scratch.write("agent-pool.toml", POOL, 0o644);
    let pool = pool.to_str().unwrap().to_owned();
    Harness::build_in(scratch, &["--agent-pool-config", &pool], None)
}

/// The words of a mint for `lambdalabs:i-1`, plus `extra`.
fn mint<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec![
        "join-token",
        "mint",
        "--provider",
        "lambdalabs",
        "--instance-id",
        "i-1",
    ];
    words.extend(extra);
    words
}

/// The words of a revoke for `lambdalabs:i-1`.
const REVOKE: [&str; 6] = [
    "join-token",
    "revoke",
    "--provider",
    "lambdalabs",
    "--instance-id",
    "i-1",
];

fn is_token(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Asserts that a mint without `--out` prints the token alone on a line (64 hex digits),
/// nothing on stderr, and that the ttl is the server's default (30 minutes) until `--ttl`
/// says another: the expiry in `--json` is the server's clock plus the ttl. It catches
/// output that wraps the token in words (a script cannot take it as it is), a ttl that is
/// dropped or sent as a different unit, and a text and JSON form that disagree.
#[test]
fn a_mint_prints_the_token_alone_or_as_json() {
    let server = agent_server("join_mint_stdout");
    let got = server.js.go(&mint(&[]));
    assert_eq!(got.code, 0, "{}", got.err);
    assert!(is_token(got.out.strip_suffix('\n').unwrap()), "{}", got.out);
    assert_eq!(got.err, "");

    let got = server.js.go(&mint(&["--json"]));
    assert_eq!(got.code, 0, "{}", got.err);
    let body = got.json();
    assert!(is_token(body["token"].as_str().unwrap()), "{body}");
    assert_eq!(body["expires_at_ms"], T0 + 30 * 60 * 1000);

    let got = server.js.go(&mint(&["--json", "--ttl", "2h"]));
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(got.json()["expires_at_ms"], T0 + 2 * 3600 * 1000);
}

/// Asserts that a mint with `--out` creates the file with mode 0600 holding the token and a
/// newline, and that stdout names the file and the expiry and holds nothing of the token
/// (text and JSON). A second mint to a new file for the same instance gives another token.
/// It catches a token file other users can read, a missing newline, and a token echoed
/// beside the path.
#[test]
fn a_mint_with_out_writes_a_private_file_and_prints_no_token() {
    let server = agent_server("join_mint_out");
    let path = server.scratch.path("minted");
    let shown = path.to_str().unwrap();
    let got = server.js.go(&mint(&["--out", shown]));
    assert_eq!(got.code, 0, "{}", got.err);
    let written = fs::read_to_string(&path).unwrap();
    let token = written.strip_suffix('\n').unwrap();
    assert!(is_token(token), "{written:?}");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        got.out,
        format!(
            "join token for lambdalabs:i-1 written to {shown} expires {}\n",
            T0 + 30 * 60 * 1000
        )
    );
    assert_eq!(got.err, "");

    let second = server.scratch.path("minted-2");
    let shown = second.to_str().unwrap();
    let got = server.js.go(&mint(&["--out", shown, "--json"]));
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(
        got.json(),
        json!({
            "provider": "lambdalabs",
            "instance_id": "i-1",
            "out": shown,
            "expires_at_ms": T0 + 30 * 60 * 1000,
        })
    );
    let other = fs::read_to_string(&second).unwrap();
    assert!(is_token(other.trim_end()), "{other:?}");
    assert_ne!(other, written);
    assert!(!got.out.contains(other.trim_end()), "{}", got.out);
}

/// Asserts that a mint whose `--out` file exists is refused as a usage failure (exit 2)
/// naming `--out`, leaves the file as it was, and sends no request at all. It catches a
/// token written over a file that was there (and so lost for the agent that reads it)
/// and a request made, and a token minted, before the path was checked.
#[test]
fn an_existing_out_file_is_refused_before_any_request() {
    let scratch = Scratch::new("join_out_exists");
    let (url, seen) = recording(http("201 Created", r#"{"token":"x","expires_at_ms":1}"#));
    let js = against(&url, &scratch);
    let taken = scratch.write("taken", "keep me\n", 0o644);
    let got = js.go(&mint(&["--out", taken.to_str().unwrap()]));
    assert_eq!(got.code, 2, "{}", got.err);
    assert!(got.err.contains("--out"), "{}", got.err);
    assert!(got.err.contains("exists"), "{}", got.err);
    assert_eq!(fs::read_to_string(&taken).unwrap(), "keep me\n");
    assert!(seen.lock().unwrap().is_empty());
}

/// Asserts that an `--out` in a directory that is not there is a usage failure naming
/// `--out` and the path. It catches a failure that does not say which flag or file.
#[test]
fn an_out_file_that_cannot_be_made_is_a_usage_failure() {
    let scratch = Scratch::new("join_out_missing_dir");
    let (url, seen) = recording(http("201 Created", r#"{"token":"x","expires_at_ms":1}"#));
    let js = against(&url, &scratch);
    let path = scratch.path("no-such-dir").join("token");
    let got = js.go(&mint(&["--out", path.to_str().unwrap()]));
    assert_eq!(got.code, 2, "{}", got.err);
    assert!(
        got.err.starts_with("js: --out: ") && got.err.contains("no-such-dir"),
        "{}",
        got.err
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// Asserts that a mint the server refuses leaves no file behind and says why: a 400 with
/// a reason (a provider the pool does not admit) is the contract exit 4 carrying the
/// server's reason. It catches an empty token file left where the agent will read it.
#[test]
fn a_refused_mint_removes_its_out_file_and_exits_with_the_contract_status() {
    let scratch = Scratch::new("join_provider_mismatch");
    let reply = http(
        "400 Bad Request",
        r#"{"error":"bad-request","reason":"provider must be the pool's name, \"burst\""}"#,
    );
    let (url, seen) = recording(reply);
    let js = against(&url, &scratch);
    let path = scratch.path("minted");
    let got = js.go(&mint(&["--out", path.to_str().unwrap()]));
    assert_eq!(got.code, 4, "{}", got.err);
    assert!(
        got.err.contains("provider must be the pool's name"),
        "{}",
        got.err
    );
    assert!(!path.exists());
    assert_eq!(got.out, "");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "POST /v1/agent/join-tokens HTTP/1.1");
    assert_eq!(
        seen[0].1,
        json!({ "provider": "lambdalabs", "instance_id": "i-1" })
    );
}

/// Asserts the body of a mint: `ttl_seconds` is in it only when `--ttl` is given, in whole
/// seconds. It catches a null or default ttl sent when none was asked for.
#[test]
fn the_ttl_is_sent_only_when_given() {
    let scratch = Scratch::new("join_ttl_body");
    let (url, seen) = recording(http("201 Created", r#"{"token":"abc","expires_at_ms":5}"#));
    let js = against(&url, &scratch);
    assert_eq!(js.go(&mint(&[])).code, 0);
    let got = js.go(&mint(&["--ttl", "15m"]));
    assert_eq!((got.code, got.out.as_str()), (0, "abc\n"), "{}", got.err);
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0].1,
        json!({ "provider": "lambdalabs", "instance_id": "i-1" })
    );
    assert_eq!(
        seen[1].1,
        json!({ "provider": "lambdalabs", "instance_id": "i-1", "ttl_seconds": 900 })
    );
}

/// Asserts the ttl limits: a zero or a unit that does not exist is refused by the command
/// line (exit 2) before any request, and a ttl above a day is the server's 400, exit 4,
/// with its reason. It catches a ttl that wraps or is clamped silently.
#[test]
fn the_ttl_is_parsed_and_the_servers_limit_is_reported() {
    let server = agent_server("join_ttl_limits");
    for bad in ["0", "2d", "1.5h", "soon"] {
        let got = server.js.go(&mint(&["--ttl", bad]));
        assert_eq!(got.code, 2, "{bad}: {}", got.err);
        assert!(got.err.contains("--ttl"), "{bad}: {}", got.err);
    }
    let got = server.js.go(&mint(&["--ttl", "24h"]));
    assert_eq!(got.code, 0, "{}", got.err);
    let got = server.js.go(&mint(&["--ttl", "25h"]));
    assert_eq!(got.code, 4, "{}", got.err);
    assert!(
        got.err.contains("ttl_seconds is above one day"),
        "{}",
        got.err
    );
}

/// Asserts that an id the server cannot take is a 400, the contract exit 4, for both
/// commands, and nothing is written. It catches a request error told as a server failure.
#[test]
fn an_invalid_instance_is_the_contract_exit() {
    let server = agent_server("join_invalid_instance");
    let path = server.scratch.path("minted");
    let bad = [
        "join-token",
        "mint",
        "--provider",
        "bad provider",
        "--instance-id",
        "i-1",
        "--out",
        path.to_str().unwrap(),
    ];
    let got = server.js.go(&bad);
    assert_eq!(got.code, 4, "{}", got.err);
    assert!(!path.exists());
    let got = server.js.go(&[
        "join-token",
        "revoke",
        "--provider",
        "lambdalabs",
        "--instance-id",
        "",
    ]);
    assert_eq!(got.code, 4, "{}", got.err);
}

/// Asserts that a client token the server does not know is refused with the server
/// failure exit 1 saying the token was refused, for both commands, and no file is left.
/// It catches a 401 told as success or as a contract error.
#[test]
fn a_bad_client_token_is_refused() {
    let server = agent_server("join_bad_bearer");
    let wrong = server.scratch.write(
        "wrong-token",
        "not-the-clients-token-0123456789abcdef",
        0o600,
    );
    let js = Js {
        token_file: wrong,
        ..server.js.clone()
    };
    let path = server.scratch.path("minted");
    let got = js.go(&mint(&["--out", path.to_str().unwrap()]));
    assert_eq!(got.code, 1, "{}", got.err);
    assert!(got.err.contains("refused the token (401)"), "{}", got.err);
    assert!(!path.exists());
    let got = js.go(&REVOKE);
    assert_eq!(got.code, 1, "{}", got.err);
    assert!(got.err.contains("refused the token (401)"), "{}", got.err);
}

/// Asserts that a server that admits no agents has no route, which both commands report
/// as the refusal `join-tokens-unsupported` (exit 3). It catches a 404 told as an
/// unexplained server error.
#[test]
fn a_server_without_agents_refuses_with_a_code() {
    let server = Harness::new("join_unsupported");
    for words in [mint(&[]), REVOKE.to_vec()] {
        let got = server.js.go(&words);
        assert_eq!(got.code, 3, "{}", got.err);
        assert_eq!(got.err, "js: refused: join-tokens-unsupported\n");
    }
}

/// Asserts the answers the real server gives for a mint it cannot make: 429 and 503 are
/// the server failure exit 1 with the server's code, and a 201 without a token is a
/// failure naming the field. It catches a refusal told as success and a body taken on
/// trust.
#[test]
fn other_answers_are_told_as_they_are() {
    let scratch = Scratch::new("join_other_answers");
    let cases = [
        (
            http(
                "429 Too Many Requests",
                r#"{"error":"too-many-join-tokens"}"#,
            ),
            "unexpected answer 429 (too-many-join-tokens)",
        ),
        (
            http(
                "503 Service Unavailable",
                r#"{"error":"token-unavailable"}"#,
            ),
            "unexpected answer 503 (token-unavailable)",
        ),
        (
            http("201 Created", r#"{"expires_at_ms":5}"#),
            r#"no usable "token""#,
        ),
        (
            http("201 Created", r#"{"token":"abc"}"#),
            r#"no usable "expires_at_ms""#,
        ),
    ];
    for (reply, said) in cases {
        let (url, _) = recording(reply);
        let got = against(&url, &scratch).go(&mint(&[]));
        assert_eq!(got.code, 1, "{said}: {}", got.err);
        assert!(got.err.contains(said), "{said}: {}", got.err);
        assert_eq!(got.out, "");
    }
}

/// Asserts the request of a revoke: a DELETE of the join-token route whose body names the
/// provider and instance, and that a 204 is exit 0 with a line (text and JSON). It catches
/// a revoke that sends no body (the server would revoke nothing) or the wrong method.
#[test]
fn a_revoke_sends_its_body_and_a_204_is_success() {
    let scratch = Scratch::new("join_revoke_body");
    let (url, seen) = recording(http("204 No Content", ""));
    let js = against(&url, &scratch);
    let got = js.go(&REVOKE);
    assert_eq!(
        (got.code, got.out.as_str()),
        (0, "join tokens for lambdalabs:i-1 revoked\n"),
        "{}",
        got.err
    );
    let mut json_words = REVOKE.to_vec();
    json_words.push("--json");
    let got = js.go(&json_words);
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(
        got.json(),
        json!({ "provider": "lambdalabs", "instance_id": "i-1", "state": "revoked" })
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].0, "DELETE /v1/agent/join-tokens HTTP/1.1");
    assert_eq!(
        seen[0].1,
        json!({ "provider": "lambdalabs", "instance_id": "i-1" })
    );
}

/// Asserts that a revoke against a real server is a 204 whether or not the instance has
/// a token, so a terminate that is retried needs no care.
#[test]
fn a_revoke_is_idempotent_on_a_real_server() {
    let server = agent_server("join_revoke_real");
    assert_eq!(server.js.go(&mint(&[])).code, 0);
    for _ in 0..2 {
        let got = server.js.go(&REVOKE);
        assert_eq!(got.code, 0, "{}", got.err);
    }
}
