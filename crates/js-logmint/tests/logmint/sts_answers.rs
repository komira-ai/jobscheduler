//! The STS minter against scripted answers: how a failure is classified, and how the answer's
//! credential is read.

use std::io;
use std::time::{Duration, UNIX_EPOCH};

use js_logmint::{MintError, Minter, Refusal};
use js_logstore::{Credential, TransportError, Unavailable};

use crate::support::{Scripted, credentials, fields, reply, request, scripted_minter, sts_error};

/// What one mint over a transport answering `answer` gives, for an hour.
fn mint(
    answer: Result<js_logstore::HttpResponse, TransportError>,
) -> Result<Credential, MintError> {
    mint_for(answer, Duration::from_secs(3600))
}

/// What one mint over a transport answering `answer` gives, for `ttl`.
fn mint_for(
    answer: Result<js_logstore::HttpResponse, TransportError>,
    ttl: Duration,
) -> Result<Credential, MintError> {
    let ask = js_logmint::MintRequest {
        ttl,
        ..request("a/b", "host-1")
    };
    scripted_minter(Scripted::new(vec![answer])).mint(&ask)
}

fn unavailable(status: u16, code: &str) -> Result<Credential, MintError> {
    Err(MintError::Unavailable(Unavailable::Status {
        status,
        code: code.to_owned(),
    }))
}

fn refused(status: u16, code: &str) -> Result<Credential, MintError> {
    Err(MintError::Refused(Refusal::Status {
        status,
        code: code.to_owned(),
    }))
}

/// Asserts the classes: a 503, a 500, a 429, a 400 `Throttling`, a transport error and a
/// timeout are `Unavailable`; a 403 `AccessDenied`, and a 400 `AccessDenied`,
/// `InvalidClientTokenId`, `MalformedPolicyDocument` or `ValidationError`, a 404 and a 302 are
/// `Refused`, each with its status and code; a message that names another code, or says to
/// retry, changes nothing; a code that is not 1 to 64 letters and digits is kept as empty; and
/// the texts name the class, the status and the code only. It catches a classification by
/// message text and a message or secret carried into an error.
#[test]
fn errors_are_classified_by_code_not_by_message() {
    let cases = [
        (
            503,
            "ServiceUnavailable",
            unavailable(503, "ServiceUnavailable"),
        ),
        (500, "InternalFailure", unavailable(500, "InternalFailure")),
        (429, "TooManyRequests", unavailable(429, "TooManyRequests")),
        (400, "Throttling", unavailable(400, "Throttling")),
        (403, "AccessDenied", refused(403, "AccessDenied")),
        (400, "AccessDenied", refused(400, "AccessDenied")),
        (
            400,
            "InvalidClientTokenId",
            refused(400, "InvalidClientTokenId"),
        ),
        (
            400,
            "MalformedPolicyDocument",
            refused(400, "MalformedPolicyDocument"),
        ),
        (400, "ValidationError", refused(400, "ValidationError")),
        (404, "NotFound", refused(404, "NotFound")),
        (302, "", refused(302, "")),
    ];
    for (status, code, class) in cases {
        for message in [
            "plain words",
            "Throttling: please retry later",
            "AccessDenied MalformedPolicyDocument <Code>Throttling</Code>",
        ] {
            let message = message
                .replace("<Code>", "&lt;Code&gt;")
                .replace("</Code>", "&lt;/Code&gt;");
            assert_eq!(
                mint(sts_error(status, code, &message)),
                class,
                "{status} {code} {message}"
            );
        }
    }
    for odd in ["Not A Code", &"C".repeat(65), "Code<", ""] {
        assert_eq!(mint(sts_error(403, odd, "no")), refused(403, ""), "{odd:?}");
    }
    assert_eq!(mint(reply(403, "no code at all")), refused(403, ""));
    assert_eq!(mint(reply(400, "<Code>Throttling")), refused(400, ""));
    assert_eq!(
        mint(reply(503, "\u{fffd}<Code>Busy</Code>")),
        unavailable(503, "Busy")
    );

    for failure in [
        TransportError::Timeout,
        TransportError::Io(io::ErrorKind::ConnectionRefused),
        TransportError::Exchange,
        TransportError::TooLarge,
    ] {
        assert_eq!(
            mint(Err(failure.clone())),
            Err(MintError::Unavailable(Unavailable::Transport(failure)))
        );
    }

    let texts = [
        (
            unavailable(503, "ServiceUnavailable"),
            "the STS is unavailable: status 503, code `ServiceUnavailable`",
        ),
        (
            Err(MintError::Unavailable(Unavailable::Transport(
                TransportError::Timeout,
            ))),
            "the STS is unavailable: the store did not answer in time",
        ),
        (
            refused(403, "AccessDenied"),
            "the STS refused the mint: status 403, code `AccessDenied`",
        ),
        (
            Err(MintError::Refused(Refusal::Answer("why"))),
            "the STS refused the mint: why",
        ),
    ];
    for (error, text) in texts {
        assert_eq!(error.unwrap_err().to_string(), text);
    }
}

/// Asserts how a successful answer is read: a well-formed one gives exactly its key id, secret,
/// session token and expiry (fractional seconds and an offset taken); and an answer that is not
/// UTF-8, has no `<Credentials>` or two, lacks any of the four fields or has one twice, has an
/// empty field, a field with a space, a `<` or an `&`, or an `Expiration` that is not RFC 3339,
/// is `Refused` without repeating any of it. It catches a minter that hands out a credential it
/// read wrongly.
#[test]
fn a_successful_answer_is_read_strictly() {
    let expiry = UNIX_EPOCH + Duration::from_secs(1_800_003_600);
    let good = credentials(&fields("2027-01-15T09:00:00Z"));
    assert_eq!(
        mint(reply(200, &good)).unwrap(),
        Credential {
            access_key_id: "ASIAEXAMPLE".to_owned(),
            secret_access_key: "minted-secret".to_owned(),
            session_token: Some("minted-token".to_owned()),
            expires_at: expiry,
        }
    );
    let fraction = credentials(&fields("2027-01-15T10:00:00.250+01:00"));
    let read = mint(reply(200, &fraction)).unwrap();
    assert_eq!(read.expires_at, expiry + Duration::from_millis(250));

    let missing: Result<Credential, MintError> = Err(MintError::Refused(Refusal::Answer(
        "the answer lacks a well-formed credential",
    )));
    let fields_of = fields("2027-01-15T09:00:00Z");
    let mut bad: Vec<String> = vec![
        String::from_utf8_lossy(b"\xff").into_owned(),
        fields_of.clone(),
        format!("{good}{good}"),
        credentials(&fields("tomorrow")),
        credentials(&fields("")),
        credentials(&fields("2027-01-15 09:00:00Z")),
        credentials(&fields_of.replace("minted-secret", "minted secret")),
        credentials(&fields_of.replace("minted-token", "minted&amp;token")),
        credentials(&fields_of.replace("minted-token", "")),
    ];
    for name in [
        "AccessKeyId",
        "SecretAccessKey",
        "SessionToken",
        "Expiration",
    ] {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        let start = fields_of.find(&open).unwrap();
        let end = fields_of.find(&close).unwrap() + close.len();
        let element = &fields_of[start..end];
        bad.push(credentials(&fields_of.replace(element, "")));
        bad.push(credentials(&format!("{fields_of}{element}")));
        bad.push(credentials(
            &fields_of.replace(&close, &format!("<{close}")),
        ));
    }
    for body in &bad {
        assert_eq!(mint(reply(200, body)), missing, "{body}");
    }
    let not_utf8 = Ok(js_logstore::HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: b"<Credentials>\xff</Credentials>".to_vec(),
    });
    assert_eq!(mint(not_utf8), missing);
}

/// Asserts the margin at its bound: for a lifetime asked of 100 seconds, a credential with 60
/// seconds left is taken and one with 59 is refused, as is one that has already expired. It
/// catches an off-by-one that hands out a credential with less than a minute to live.
#[test]
fn a_credential_needs_a_minute_left() {
    let ttl = Duration::from_secs(100);
    let at = |expiration: &str| mint_for(reply(200, &credentials(&fields(expiration))), ttl);
    assert!(at("2027-01-15T08:01:00Z").is_ok());
    for expiration in ["2027-01-15T08:00:59Z", "2027-01-15T07:59:00Z"] {
        assert!(
            matches!(at(expiration), Err(MintError::Refused(Refusal::Answer(_)))),
            "{expiration}"
        );
    }
}
