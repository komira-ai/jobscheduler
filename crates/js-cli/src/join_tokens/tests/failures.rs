//! `lease join-token mint` when the server's answer is a refusal, odd or lost: what is
//! printed, the exit status, that nothing is written and that no request is repeated.

use super::*;

/// What a mint says when its answer was lost or unusable, after the cause `cause`.
fn unconfirmed(cause: &str) -> String {
    format!(
        "js: {cause}; a join token may have been minted and lost: nothing was written, and \
         an unused token expires on the server by itself\n"
    )
}

/// Asserts that a refusal prints the status and the server's error code and nothing else of
/// the body (a reason, a page), with the exit status of its kind, and writes nothing: 400
/// is a rejected request (4), 404 and 429 are refusals (3), anything else a server failure
/// (1); a code is cut to 64 characters and loses its control characters. It catches a body
/// printed whole, which a proxy's error page could fill with secrets.
#[test]
fn a_refusal_prints_the_status_and_the_code_only() {
    let long = "c".repeat(300);
    let cases: [(u16, Value, u8, String); 6] = [
        (
            400,
            json!({ "error": "bad-request", "reason": "provider must be the pool's name, SECRET-REASON" }),
            4,
            "400 (bad-request)".to_owned(),
        ),
        (
            401,
            json!({ "error": "unauthorized" }),
            1,
            "401 (unauthorized)".to_owned(),
        ),
        (
            429,
            json!({ "error": "too-many-join-tokens" }),
            3,
            "429 (too-many-join-tokens)".to_owned(),
        ),
        (
            404,
            json!({ "error": "not-found" }),
            3,
            "404 (not-found)".to_owned(),
        ),
        (
            // A proxy's page is no JSON, which the client reads as `null`.
            503,
            Value::Null,
            1,
            "503 (none)".to_owned(),
        ),
        (
            500,
            json!({ "error": format!("\u{1b}[31m{long}") }),
            1,
            format!("500 ([31m{})", "c".repeat(60)),
        ),
    ];
    let dir = scratch("refused_answers");
    for (status, body, code, said) in cases {
        let server = Fake::new(move || reply(status, body.clone()));
        let out = dir.join("minted");
        let got = go(&server, &["--out", out.to_str().unwrap()], false);
        assert_eq!(got.code, code, "{status}: {}", got.err);
        assert_eq!(got.out, "");
        let want = format!("js: join token not minted: the server answered {said}\n");
        assert_eq!(got.err, want, "{status}");
        assert!(!out.exists());
        assert_eq!(server.requests(), 1);
    }
}

/// Asserts that a 201 whose body is not a usable answer (not JSON, empty, an array, a
/// string, a missing, short, upper-case or non-string token, an expiry that is a string,
/// negative, a float or missing) is a failure (exit 1) with exactly the redacted text
/// naming the field and saying the token may have been minted, one request, nothing on
/// standard output and no file, to a file and to standard output alike. It catches a file
/// written with whatever the server said, an odd token echoed, and a message that implies
/// nothing was minted.
#[test]
fn a_201_without_a_usable_answer_is_a_failure_that_says_a_token_may_exist() {
    let upper = MINTED.to_uppercase();
    let cases: Vec<(Value, &str)> = vec![
        (Value::Null, "token"),
        (json!([1, 2]), "token"),
        (json!("just a string"), "token"),
        (json!({ "expires_at_ms": EXPIRES }), "token"),
        (
            json!({ "token": "short", "expires_at_ms": EXPIRES }),
            "token",
        ),
        (json!({ "token": upper, "expires_at_ms": EXPIRES }), "token"),
        (json!({ "token": 7, "expires_at_ms": EXPIRES }), "token"),
        (json!({ "token": MINTED }), "expires_at_ms"),
        (
            json!({ "token": MINTED, "expires_at_ms": "1800000000000" }),
            "expires_at_ms",
        ),
        (
            json!({ "token": MINTED, "expires_at_ms": -5 }),
            "expires_at_ms",
        ),
        (
            json!({ "token": MINTED, "expires_at_ms": 1.5 }),
            "expires_at_ms",
        ),
    ];
    for (body, field) in cases {
        let dir = scratch("unusable");
        let shown = body.clone();
        let server = Fake::new(move || reply(201, body.clone()));
        let out = dir.join("minted");
        let want = unconfirmed(&format!("the server's answer has no usable {field:?}"));
        let got = go(&server, &["--out", out.to_str().unwrap()], false);
        assert_eq!((got.code, got.out.as_str()), (1, ""), "{shown}");
        assert_eq!(got.err, want, "{shown}");
        assert!(!out.exists(), "{shown}");
        let got = go(&server, &[], false);
        assert_eq!((got.code, got.out.as_str()), (1, ""), "{shown}");
        assert_eq!(got.err, want, "{shown}");
        assert_eq!(server.requests(), 2, "{shown}");
        assert!(listing(dir.path()).is_empty());
    }
}

/// Asserts that a 201 with fields next to the token and the expiry, whether strings,
/// numbers or objects, is accepted: the token goes to the file and the facts to standard
/// error. It catches a strict reader that would refuse a server that adds a field, which is
/// a compatible change.
#[test]
fn a_201_with_extra_fields_is_accepted() {
    let dir = scratch("extra");
    let body = json!({
        "token": MINTED,
        "expires_at_ms": EXPIRES,
        "note": "x",
        "limits": { "uses": 1 },
        "list": [1, 2],
    });
    let server = Fake::new(move || reply(201, body.clone()));
    let out = dir.join("minted");
    let got = go(&server, &["--out", out.to_str().unwrap()], false);
    assert_eq!((got.code, got.out.as_str()), (0, ""), "{}", got.err);
    assert_eq!(got.err, facts());
    assert_eq!(fs::read_to_string(out).unwrap(), format!("{MINTED}\n"));
}

/// Asserts what `--json` does on failure: standard output stays empty and standard error
/// gets one JSON object line with the failure's kind and the same message the text form
/// prints, with the exit status of the failure (4 for a rejected request, 1 for an unusable
/// answer, 3 for a refusal). It catches a failure that goes to standard output, where a
/// pipe to `js-agent enroll` would take it for a token, and a kind or status that changes
/// with the format.
#[test]
fn json_failures_go_to_standard_error_as_one_object() {
    let cases = [
        (
            400,
            json!({ "error": "bad-request" }),
            4,
            "rejected",
            "join token not minted: the server answered 400 (bad-request)".to_owned(),
        ),
        (
            429,
            json!({ "error": "too-many-join-tokens" }),
            3,
            "refused",
            "join token not minted: the server answered 429 (too-many-join-tokens)".to_owned(),
        ),
        (
            201,
            json!([]),
            1,
            "mint-unconfirmed",
            unconfirmed("the server's answer has no usable \"token\"")
                .trim_start_matches("js: ")
                .trim_end()
                .to_owned(),
        ),
    ];
    for (status, body, code, kind, message) in cases {
        let server = Fake::new(move || reply(status, body.clone()));
        let got = go(&server, &[], true);
        assert_eq!((got.code, got.out.as_str()), (code, ""), "{status}");
        let want = format!("{}\n", json!({ "error": kind, "message": message }));
        assert_eq!(got.err, want, "{status}");
    }
}

/// Asserts that a request that gets no answer (the connection is refused, the server reads
/// the request and closes without answering, the answer is cut short: the errors the client
/// gives for each) is a failure (exit 1) after exactly one request per run, with no retry,
/// no token on any stream, no `--out` file, nothing else in the directory, and a message
/// that says a token may have been minted. It catches a retry that mints a second token, and
/// a message that tells the operator nothing was minted when the server may have done so.
#[test]
fn a_request_without_an_answer_is_not_repeated_and_says_a_token_may_exist() {
    let causes = [
        "cannot talk to http://[::1]:7000: io: Connection refused (os error 111)",
        "cannot talk to http://[::1]:7000: the connection was closed before an answer",
        "cannot talk to http://[::1]:7000: the answer ended before its length",
    ];
    for cause in causes {
        let dir = scratch("no_answer");
        let server = Fake::new(move || Err(ClientError::Server(cause.to_owned())));
        let out = dir.join("minted");
        let got = go(&server, &["--out", out.to_str().unwrap()], false);
        assert_eq!(
            (got.code, got.out.as_str()),
            (1, ""),
            "{cause}: {}",
            got.err
        );
        assert_eq!(got.err, unconfirmed(cause), "{cause}");
        assert_eq!(got.err.matches('\n').count(), 1, "{cause}: {}", got.err);
        assert!(!got.err.contains(MINTED), "{cause}");
        assert!(listing(dir.path()).is_empty(), "{cause}");
        assert_eq!(server.requests(), 1, "{cause}: one request per run");

        let got = go(&server, &[], false);
        assert_eq!(
            (got.code, got.out.as_str()),
            (1, ""),
            "{cause}: {}",
            got.err
        );
        assert_eq!(got.err, unconfirmed(cause), "{cause}");
        assert_eq!(server.requests(), 2, "{cause}: one request per run");
    }
}
