//! Every failure of the API maps to a typed error: the token exchange, the key mint, and
//! the transport. None of the errors holds the body the server sent, which can echo a
//! secret.

use std::time::Duration;

use js_lease_server::network::{
    ANSWER_LIMIT, ApiFailure, NetworkError, NetworkMode, TailscaleSettings,
};

use crate::fake::{AUTH_KEY, CLIENT_SECRET, Fake, Reply, TOKEN, closed_port_url};
use crate::support::{TAG, TEN_MINUTES, api_at, binary, clock, ctx, host, mode};

const TOKEN_ROUTE: &str = "POST /api/v2/oauth/token";
const KEY_ROUTE: &str = "POST /api/v2/tailnet/-/keys";

fn token_failure(reply: Reply) -> NetworkError {
    let fake = Fake::tailscale(host().as_str());
    fake.set(TOKEN_ROUTE, reply);
    let (_, shared) = clock();
    mode(&fake, shared).check().unwrap_err()
}

fn mint_failure(reply: Reply) -> NetworkError {
    let fake = Fake::tailscale(host().as_str());
    fake.set(KEY_ROUTE, reply);
    let (_, shared) = clock();
    let host = host();
    mode(&fake, shared)
        .prepare(&ctx(&host, TEN_MINUTES))
        .unwrap_err()
}

/// Asserts that each class of answer to the token exchange is its own typed failure,
/// under `Credentials`: 4xx refused, 5xx unavailable, any other status unexpected, and a
/// success without a usable token malformed.
#[test]
fn token_errors_are_typed() {
    let cases = [
        (Reply::json(400, "{}"), ApiFailure::Rejected(400)),
        (Reply::json(401, "{}"), ApiFailure::Rejected(401)),
        (Reply::json(403, "{}"), ApiFailure::Rejected(403)),
        (Reply::json(499, "{}"), ApiFailure::Rejected(499)),
        (Reply::json(500, "{}"), ApiFailure::Unavailable(500)),
        (Reply::json(503, "{}"), ApiFailure::Unavailable(503)),
        (Reply::json(599, "{}"), ApiFailure::Unavailable(599)),
        (Reply::json(302, "{}"), ApiFailure::Unexpected(302)),
        (Reply::json(200, "not json"), ApiFailure::Malformed),
        (Reply::json(200, "{}"), ApiFailure::Malformed),
        (
            Reply::json(200, r#"{"access_token":"x"}"#),
            ApiFailure::Malformed,
        ),
        (
            Reply::json(200, r#"{"access_token":"","expires_in":60}"#),
            ApiFailure::Malformed,
        ),
        (
            Reply::json(200, r#"{"access_token":5,"expires_in":60}"#),
            ApiFailure::Malformed,
        ),
    ];
    for (reply, failure) in cases {
        assert_eq!(
            token_failure(reply.clone()),
            NetworkError::Credentials(failure.clone()),
            "{failure}"
        );
    }
}

/// Asserts that a launch whose token exchange fails reports `Credentials`, not a mint
/// failure, and sends no key request.
#[test]
fn a_launch_without_a_token_is_a_credentials_failure() {
    let fake = Fake::tailscale(host().as_str());
    fake.set(TOKEN_ROUTE, Reply::json(401, "{}"));
    let (_, shared) = clock();
    let host = host();
    let err = mode(&fake, shared)
        .prepare(&ctx(&host, TEN_MINUTES))
        .unwrap_err();
    assert_eq!(err, NetworkError::Credentials(ApiFailure::Rejected(401)));
    assert_eq!(fake.count(KEY_ROUTE), 0);
}

/// Asserts that each class of answer to the key request is its own typed failure under
/// `KeyMint`, including a success that holds no key.
#[test]
fn key_mint_errors_are_typed() {
    let cases = [
        (Reply::json(400, "{}"), ApiFailure::Rejected(400)),
        (Reply::json(403, "{}"), ApiFailure::Rejected(403)),
        (Reply::json(429, "{}"), ApiFailure::Rejected(429)),
        (Reply::json(500, "{}"), ApiFailure::Unavailable(500)),
        (Reply::json(502, "{}"), ApiFailure::Unavailable(502)),
        (Reply::json(301, "{}"), ApiFailure::Unexpected(301)),
        (Reply::json(200, "not json"), ApiFailure::Malformed),
        (Reply::json(200, "{}"), ApiFailure::Malformed),
        (Reply::json(200, r#"{"key":""}"#), ApiFailure::Malformed),
        (Reply::json(200, r#"{"key":7}"#), ApiFailure::Malformed),
    ];
    for (reply, failure) in cases {
        assert_eq!(
            mint_failure(reply),
            NetworkError::KeyMint(failure.clone()),
            "{failure}"
        );
    }
    assert_eq!(
        mint_failure(Reply::json(403, "{}")).to_string(),
        "network: key mint failed: refused with status 403"
    );
}

/// Asserts that a connection that is refused, an answer that is cut short, an answer
/// over the limit and a server that never answers are transport failures, and that an
/// answer of exactly the limit is read.
#[test]
fn transport_failures_and_the_answer_limit() {
    let (_, shared) = clock();

    // Nothing listens.
    let api = api_at(&closed_port_url(), shared.clone(), Duration::from_secs(5));
    assert!(matches!(
        api.check(),
        Err(NetworkError::Credentials(ApiFailure::Transport(_)))
    ));
    assert!(ApiFailure::Transport(String::new()).transient());

    // The answer stops before its length.
    let fake = Fake::empty();
    fake.on(
        TOKEN_ROUTE,
        Reply::Raw(b"HTTP/1.1 200 OK\r\ncontent-length: 50\r\n\r\n{".to_vec()),
    );
    let api = api_at(&fake.url(), shared.clone(), Duration::from_secs(5));
    assert!(matches!(
        api.check(),
        Err(NetworkError::Credentials(ApiFailure::Transport(_)))
    ));

    // One byte over the limit; exactly the limit is a token (spaces after the JSON).
    let json = r#"{"access_token":"t","expires_in":60}"#;
    let padded = |len: u64| {
        let pad = usize::try_from(len).unwrap() - json.len();
        format!("{json}{}", " ".repeat(pad))
    };
    let fake = Fake::empty();
    fake.on(TOKEN_ROUTE, Reply::json(200, &padded(ANSWER_LIMIT + 1)));
    let api = api_at(&fake.url(), shared.clone(), Duration::from_secs(30));
    assert!(matches!(
        api.check(),
        Err(NetworkError::Credentials(ApiFailure::Transport(_)))
    ));
    fake.set(TOKEN_ROUTE, Reply::json(200, &padded(ANSWER_LIMIT)));
    let api = api_at(&fake.url(), shared.clone(), Duration::from_secs(30));
    api.check().unwrap();

    // The server takes the request and says nothing: the time limit ends it.
    let fake = Fake::empty();
    fake.on(TOKEN_ROUTE, Reply::Hang);
    let api = api_at(&fake.url(), shared, Duration::from_millis(300));
    assert!(matches!(
        api.check(),
        Err(NetworkError::Credentials(ApiFailure::Transport(_)))
    ));
}

/// Asserts that the secrets the server echoes in an error body, and the client secret,
/// the token and the key, appear in no error's `Display` or `Debug`, nor in the `Debug`
/// of the API, the mode or the settings.
#[test]
fn no_error_or_debug_holds_a_secret() {
    let echo = format!(
        r#"{{"message":"bad {CLIENT_SECRET} {TOKEN} {AUTH_KEY}","access_token":"{TOKEN}"}}"#
    );
    let mut shown = String::new();
    for status in [400, 401, 500, 302] {
        for err in [
            token_failure(Reply::json(status, &echo)),
            mint_failure(Reply::json(status, &echo)),
        ] {
            shown.push_str(&format!("{err} {err:?}\n"));
        }
    }
    shown.push_str(&format!("{:?}\n", mint_failure(Reply::json(200, &echo))));

    let fake = Fake::tailscale(host().as_str());
    let (_, shared) = clock();
    let mode = mode(&fake, shared.clone());
    let host = host();
    let contribution = mode.prepare(&ctx(&host, TEN_MINUTES)).unwrap();
    shown.push_str(&format!("{mode:?} {contribution:?}\n"));
    shown.push_str(&format!(
        "{:?}\n",
        api_at(&fake.url(), shared, Duration::from_secs(5))
    ));
    shown.push_str(&format!(
        "{:?}\n",
        TailscaleSettings {
            tag: TAG.to_owned(),
            binary: binary()
        }
    ));
    for secret in [CLIENT_SECRET, TOKEN, AUTH_KEY] {
        assert!(!shown.contains(secret), "{secret} is in:\n{shown}");
    }
    assert!(
        shown.contains("***"),
        "the secret in the contribution prints as ***"
    );
}
