//! What a join-token mint makes of the server's answer, with the answers built by hand: no
//! socket is opened. That `Client::post` sends the bearer to the path it is given is the
//! client crate's own test (`requests_carry_the_token_and_replies_are_parsed`).

use std::cell::RefCell;

use serde_json::json;

use super::*;

/// A token as the server mints it.
pub(crate) const MINTED: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// An expiry on the server's clock.
pub(crate) const EXPIRES: u64 = 1_800_000_000_000;

/// A reply with `status` and `body`.
pub(crate) fn reply(status: u16, body: Value) -> Result<Reply, ClientError> {
    Ok(Reply {
        status,
        body,
        retry_after: None,
    })
}

/// A server that records what it was asked and answers `answer`.
struct Canned {
    asked: RefCell<Vec<(String, Value)>>,
    answer: RefCell<Option<Result<Reply, ClientError>>>,
}

impl Post for Canned {
    fn post(&self, path: &str, body: &Value) -> Result<Reply, ClientError> {
        self.asked
            .borrow_mut()
            .push((path.to_owned(), body.clone()));
        self.answer.borrow_mut().take().unwrap()
    }
}

/// The failure of a mint that got `answer`: its exit status, its kind and its text.
fn failed(answer: Result<Reply, ClientError>) -> (u8, &'static str, String) {
    let failure = mint_answer(answer).err().unwrap();
    let (code, kind) = failure.parts();
    (code, kind, failure.to_string())
}

/// What a mint says when its answer was lost or unusable, after the cause `cause`.
fn unconfirmed(cause: &str) -> String {
    format!(
        "{cause}; a join token may have been minted and lost: nothing was written, and an \
         unused token expires on the server by itself"
    )
}

/// Asserts that a mint is one POST of the mint route with the body it was given, and that
/// the 201 it gets back is the token and the expiry. It catches a route or a body that
/// changes on the way, and a second request.
#[test]
fn a_mint_is_one_post_of_the_route_with_the_given_body() {
    let body = json!({ "provider": "burst", "instance_id": "i-0abc", "ttl_seconds": 900 });
    let server = Canned {
        asked: RefCell::default(),
        answer: RefCell::new(Some(reply(
            201,
            json!({ "token": MINTED, "expires_at_ms": EXPIRES }),
        ))),
    };
    let minted = mint_join_token(&server, &body).ok().unwrap();
    assert_eq!(
        (minted.token.as_str(), minted.expires_at_ms),
        (MINTED, EXPIRES)
    );
    assert_eq!(
        *server.asked.borrow(),
        [("/v1/agent/join-tokens".to_owned(), body)]
    );
}

/// Asserts that a 201 with fields next to the token and the expiry, whether strings,
/// numbers, objects or lists, is accepted. It catches a strict reader that would refuse a
/// server that adds a field, which is a compatible change.
#[test]
fn a_201_with_extra_fields_is_accepted() {
    let body = json!({
        "token": MINTED,
        "expires_at_ms": EXPIRES,
        "note": "x",
        "limits": { "uses": 1 },
        "list": [1, 2],
    });
    let minted = mint_answer(reply(201, body)).ok().unwrap();
    assert_eq!(
        (minted.token.as_str(), minted.expires_at_ms),
        (MINTED, EXPIRES)
    );
}

/// Asserts that every answer other than 201 is a failure that holds the status and the
/// server's error code and nothing else of the body (a reason, a page), with the exit
/// status and kind of its status: 400 is a rejected request (4), 404 and 429 are refusals
/// (3), anything else, 401 included, a server failure (1); a body that is no JSON (a
/// proxy's page, read as `null`) or a code that is no string has the code `none`; a code is
/// cut to 64 characters and loses its control characters. It catches a body printed whole,
/// which a proxy's error page could fill with secrets, and a status mapped to the wrong
/// exit.
#[test]
fn a_refusal_holds_the_status_and_the_code_only() {
    let long = format!("\u{1b}[31m{}", "c".repeat(300));
    let cases = [
        (
            400,
            json!({ "error": "bad-request", "reason": "SECRET-REASON" }),
            (4, "rejected", "400 (bad-request)".to_owned()),
        ),
        (
            401,
            json!({ "error": "unauthorized" }),
            (1, "server", "401 (unauthorized)".to_owned()),
        ),
        (
            404,
            json!({ "error": "not-found" }),
            (3, "refused", "404 (not-found)".to_owned()),
        ),
        (
            429,
            json!({ "error": "too-many-join-tokens" }),
            (3, "refused", "429 (too-many-join-tokens)".to_owned()),
        ),
        (503, Value::Null, (1, "server", "503 (none)".to_owned())),
        (
            500,
            json!({ "error": long }),
            (1, "server", format!("500 ([31m{})", "c".repeat(60))),
        ),
        (
            500,
            json!({ "error": 7 }),
            (1, "server", "500 (none)".to_owned()),
        ),
    ];
    for (status, body, (code, kind, said)) in cases {
        let want = format!("join token not minted: the server answered {said}");
        assert_eq!(failed(reply(status, body)), (code, kind, want), "{status}");
    }
}

/// Asserts that a 201 whose body is not a usable answer (not JSON or empty, which read as
/// `null`; an array; a string; a missing, short, long, upper-case or non-string token; an
/// expiry that is missing, a string, negative or a float) is a failure (exit 1, kind
/// `mint-unconfirmed`) with exactly the text that names the field and says the token may
/// have been minted, and never holds the value. It catches a file written with whatever the
/// server said, an odd token echoed, and a message that implies nothing was minted.
#[test]
fn a_201_without_a_usable_answer_says_a_token_may_exist() {
    let float = format!(r#"{{"token":"{MINTED}","expires_at_ms":1800000000000.0}}"#);
    let float: Value = serde_json::from_str(&float).unwrap();
    let cases = [
        (Value::Null, "token"),
        (json!([1, 2]), "token"),
        (json!("just a string"), "token"),
        (json!({ "expires_at_ms": EXPIRES }), "token"),
        (
            json!({ "token": "short", "expires_at_ms": EXPIRES }),
            "token",
        ),
        (
            json!({ "token": MINTED.to_uppercase(), "expires_at_ms": EXPIRES }),
            "token",
        ),
        (
            json!({ "token": format!("{MINTED}0"), "expires_at_ms": EXPIRES }),
            "token",
        ),
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
        (float, "expires_at_ms"),
    ];
    for (body, field) in cases {
        let said = unconfirmed(&format!("the server's answer has no usable {field:?}"));
        let got = failed(reply(201, body.clone()));
        assert_eq!(got, (1, "mint-unconfirmed", said), "{body}");
        assert!(!got.2.contains(MINTED), "{body}");
    }
}

/// Asserts that a request that got no answer (the connection refused, closed by the server
/// before it answered, or the answer cut short: the text the client gives for each) is a
/// failure (exit 1, kind `mint-unconfirmed`) that keeps the client's text and says a token
/// may have been minted. It catches a message that tells the operator nothing was minted
/// when the server may have done so.
#[test]
fn a_request_without_an_answer_says_a_token_may_exist() {
    for cause in [
        "cannot talk to http://[::1]:7000: io: Connection refused (os error 111)",
        "cannot talk to http://[::1]:7000: the connection was closed before an answer",
        "cannot talk to http://[::1]:7000: the answer ended before its length",
    ] {
        let got = failed(Err(ClientError::Server(cause.to_owned())));
        assert_eq!(got, (1, "mint-unconfirmed", unconfirmed(cause)), "{cause}");
    }
}
