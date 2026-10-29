//! The auth key per launch: the request's shape and its auth, the token's reuse by the
//! injected clock, and the steps the launch gets.

use std::time::Duration;

use js_elastic::BootStep;
use js_lease_server::network::{
    AUTH_KEY_PATH, AUTH_KEY_SLOT, ApiFailure, MAX_KEY_VALID_FOR, NetworkError, NetworkMode,
    TOKEN_REFRESH_MARGIN,
};

use crate::fake::{AUTH_KEY, CLIENT_SECRET, Fake, Reply, TOKEN};
use crate::support::{TAG, TEN_MINUTES, clock, ctx, host, mode};

const TOKEN_ROUTE: &str = "POST /api/v2/oauth/token";
const KEY_ROUTE: &str = "POST /api/v2/tailnet/-/keys";

/// Asserts the key request: single-use, ephemeral, pre-approved, tagged, ten minutes,
/// described by pool and host, sent with the access token as a bearer, as JSON; and
/// that the token was asked for by the client-credentials grant with the client's id and
/// secret as a form.
#[test]
fn the_key_request_is_ephemeral_single_use_tagged_and_authorised() {
    let fake = Fake::tailscale(host().as_str());
    let (_, shared) = clock();
    let mode = mode(&fake, shared);
    let host = host();
    mode.prepare(&ctx(&host, TEN_MINUTES)).unwrap();

    let seen = fake.seen();
    assert_eq!(seen.len(), 2);
    let token = &seen[0];
    assert_eq!(format!("{} {}", token.method, token.target), TOKEN_ROUTE);
    assert_eq!(
        token.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        token.text(),
        format!(
            "grant_type=client_credentials&client_id=client-id-1&client_secret={CLIENT_SECRET}"
        )
    );
    assert_eq!(token.header("authorization"), None);

    let mint = &seen[1];
    assert_eq!(format!("{} {}", mint.method, mint.target), KEY_ROUTE);
    assert_eq!(
        mint.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(mint.header("content-type"), Some("application/json"));
    assert_eq!(
        mint.json(),
        serde_json::json!({
            "capabilities": {"devices": {"create": {
                "reusable": false,
                "ephemeral": true,
                "preauthorized": true,
                "tags": [TAG],
            }}},
            "expirySeconds": 600,
            "description": host.to_string(),
        })
    );
}

/// Asserts that a launch gets the key as a secret in its slot and that the steps name the
/// slot and the file: the archive is fetched with its hash, then the key file is
/// written, `tailscale up` reads it, and the file is removed last. `describe` gives the
/// same steps and calls nothing.
#[test]
fn the_contribution_names_the_key_by_slot_and_describe_is_the_same_steps() {
    let fake = Fake::tailscale(host().as_str());
    let (_, shared) = clock();
    let mode = mode(&fake, shared);
    let host = host();

    let described = mode.describe(&ctx(&host, TEN_MINUTES));
    assert!(
        fake.seen().is_empty(),
        "describe must not call the Tailscale API"
    );

    let contribution = mode.prepare(&ctx(&host, TEN_MINUTES)).unwrap();
    assert_eq!(contribution.steps, described);
    assert_eq!(contribution.secrets.len(), 1);
    let (slot, key) = &contribution.secrets[0];
    assert_eq!(slot.0, AUTH_KEY_SLOT);
    assert_eq!(key.expose(), AUTH_KEY);

    let steps = &contribution.steps;
    assert!(matches!(
        &steps[0],
        BootStep::Fetch { sha256, .. } if *sha256 == [0x5a; 32]
    ));
    let file = steps
        .iter()
        .position(|s| {
            matches!(s, BootStep::SecretFile { path, slot }
            if path == AUTH_KEY_PATH && slot.0 == AUTH_KEY_SLOT)
        })
        .expect("the key file step");
    let up = steps
        .iter()
        .position(
            |s| matches!(s, BootStep::Run(argv) if argv.get(1).map(String::as_str) == Some("up")),
        )
        .expect("the tailscale up step");
    assert!(file < up);
    let BootStep::Run(argv) = &steps[up] else {
        unreachable!()
    };
    assert!(argv.contains(&format!("--auth-key=file:{AUTH_KEY_PATH}")));
    assert!(argv.contains(&format!("--hostname={host}")));
    assert!(argv.contains(&"--accept-dns=false".to_owned()));
    assert_eq!(
        steps.last(),
        Some(&BootStep::Run(vec![
            "rm".to_owned(),
            "-f".to_owned(),
            "--".to_owned(),
            AUTH_KEY_PATH.to_owned()
        ]))
    );
    // No step holds the key's text.
    assert!(!format!("{steps:?}").contains(AUTH_KEY));
}

/// Asserts that the access token is reused while the clock is more than the refresh
/// margin before its expiry, and fetched again at the margin, to the second.
#[test]
fn the_token_is_reused_until_a_minute_before_it_expires() {
    let fake = Fake::tailscale(host().as_str());
    let (manual, shared) = clock();
    let mode = mode(&fake, shared);
    let host = host();
    let launch = || mode.prepare(&ctx(&host, TEN_MINUTES)).unwrap();

    launch();
    launch();
    assert_eq!(fake.count(TOKEN_ROUTE), 1);

    // The token lives 3600 s; one second short of the margin it is still good.
    manual.advance(Duration::from_secs(3600) - TOKEN_REFRESH_MARGIN - Duration::from_secs(1));
    launch();
    assert_eq!(fake.count(TOKEN_ROUTE), 1);

    manual.advance(Duration::from_secs(1));
    launch();
    assert_eq!(fake.count(TOKEN_ROUTE), 2);
    assert_eq!(fake.count(KEY_ROUTE), 4);
}

/// Asserts that a token the API refuses (revoked, say) is dropped, so the next launch
/// fetches another and succeeds.
#[test]
fn a_refused_token_is_fetched_again_on_the_next_launch() {
    let fake = Fake::tailscale(host().as_str());
    fake.set(KEY_ROUTE, Reply::json(401, "{}"));
    fake.on(
        KEY_ROUTE,
        Reply::json(200, &format!(r#"{{"key":"{AUTH_KEY}"}}"#)),
    );
    let (_, shared) = clock();
    let mode = mode(&fake, shared);
    let host = host();

    let first = mode.prepare(&ctx(&host, TEN_MINUTES)).unwrap_err();
    assert_eq!(first, NetworkError::KeyMint(ApiFailure::Rejected(401)));
    assert_eq!(fake.count(TOKEN_ROUTE), 1);

    mode.prepare(&ctx(&host, TEN_MINUTES)).unwrap();
    assert_eq!(fake.count(TOKEN_ROUTE), 2);
}

/// Asserts the key lifetime limits to the second: 1 s to the maximum is accepted and goes
/// out as `expirySeconds`; zero, under a second and over the maximum are refused before
/// any call is made.
#[test]
fn a_key_lifetime_is_one_second_to_the_maximum() {
    let host = host();
    for (valid_for, secs) in [
        (Duration::from_secs(1), 1),
        (MAX_KEY_VALID_FOR, MAX_KEY_VALID_FOR.as_secs()),
    ] {
        let fake = Fake::tailscale(host.as_str());
        let (_, shared) = clock();
        mode(&fake, shared).prepare(&ctx(&host, valid_for)).unwrap();
        let sent = fake.seen().pop().unwrap().json();
        assert_eq!(sent["expirySeconds"], secs);
    }
    for valid_for in [
        Duration::ZERO,
        Duration::from_millis(999),
        MAX_KEY_VALID_FOR + Duration::from_secs(1),
    ] {
        let fake = Fake::tailscale(host.as_str());
        let (_, shared) = clock();
        let err = mode(&fake, shared)
            .prepare(&ctx(&host, valid_for))
            .unwrap_err();
        assert_eq!(
            err,
            NetworkError::KeyLifetime {
                asked: valid_for,
                max: MAX_KEY_VALID_FOR.as_secs()
            }
        );
        assert!(fake.seen().is_empty(), "{valid_for:?}");
    }
}

/// Asserts that `check` exchanges the OAuth client for a token and creates nothing: one
/// token request, no key request.
#[test]
fn check_fetches_a_token_and_mints_no_key() {
    let fake = Fake::tailscale(host().as_str());
    let (_, shared) = clock();
    let mode = mode(&fake, shared);
    mode.check().unwrap();
    mode.check().unwrap();
    assert_eq!(fake.count(TOKEN_ROUTE), 1);
    assert_eq!(fake.count(KEY_ROUTE), 0);
    assert_eq!(fake.seen().len(), 1);
}
