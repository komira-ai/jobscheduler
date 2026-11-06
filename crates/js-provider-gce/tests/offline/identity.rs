//! The check of a machine's signed identity. The tokens are made-up (a project, a zone and
//! instances of the tests) and signed with a throwaway RSA key whose private half was
//! destroyed; its modulus is in `fixtures/identity/modulus.hex`, so
//! `GoogleIdentityVerifier::with_keys` verifies them and the pinned keys do not.

use std::sync::Arc;

use js_elastic::{Acquired, CapacityProvider, Hello, SignedIdentity};
use js_provider_gce::{
    FakeIdentityVerifier, GOOGLE_KEYS, GoogleIdentityVerifier, IdentityClaims, IdentityVerifier,
};

use crate::rig::{PROJECT, SCHEDULER, ZONE, ask, live, name_of, rig};

const MODULUS_HEX: &str = include_str!("../fixtures/identity/modulus.hex");
const GOOD: &str = include_str!("../fixtures/identity/good.jwt");
const OTHER_INSTANCE: &str = include_str!("../fixtures/identity/other-instance.jwt");
const OTHER_PROJECT: &str = include_str!("../fixtures/identity/other-project.jwt");
const BARE_ISSUER: &str = include_str!("../fixtures/identity/bare-issuer.jwt");
const ALTERED_SIGNATURE: &str = include_str!("../fixtures/identity/altered-signature.jwt");
const HS256: &str = include_str!("../fixtures/identity/hs256.jwt");
const UNKNOWN_KID: &str = include_str!("../fixtures/identity/unknown-kid.jwt");
const OTHER_ISSUER: &str = include_str!("../fixtures/identity/other-issuer.jwt");
const NO_ISSUER: &str = include_str!("../fixtures/identity/no-issuer.jwt");
const NO_GOOGLE: &str = include_str!("../fixtures/identity/no-google.jwt");
const NO_COMPUTE_ENGINE: &str = include_str!("../fixtures/identity/no-compute-engine.jwt");
const MISSING_CLAIM: &str = include_str!("../fixtures/identity/missing-claim.jwt");
const NO_KID: &str = include_str!("../fixtures/identity/no-kid.jwt");
const NO_ALG: &str = include_str!("../fixtures/identity/no-alg.jwt");
const CLAIMS_NOT_BASE64: &str = include_str!("../fixtures/identity/claims-not-base64.jwt");
const CLAIMS_NOT_JSON: &str = include_str!("../fixtures/identity/claims-not-json.jwt");

fn hex_bytes(text: &str) -> Vec<u8> {
    let text = text.trim();
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

fn test_key() -> GoogleIdentityVerifier {
    GoogleIdentityVerifier::with_keys(vec![("test-key-1".to_owned(), hex_bytes(MODULUS_HEX))])
}

fn token(text: &str) -> SignedIdentity {
    SignedIdentity {
        document: text.to_owned(),
        signature: String::new(),
    }
}

fn claims(project: &str, instance_id: &str) -> IdentityClaims {
    IdentityClaims {
        project_id: project.to_owned(),
        zone: ZONE.to_owned(),
        instance_id: instance_id.to_owned(),
        instance_name: name_of(SCHEDULER, "verified"),
    }
}

/// Asserts that the pinned verifier, built with `new` or by default, holds the keys of
/// `GOOGLE_KEYS` and so rejects a token signed by the test key. It catches a verifier
/// that accepts any key.
#[test]
fn the_pinned_keys_reject_a_token_of_another_key() {
    let identity = token(GOOD);
    assert_eq!(GoogleIdentityVerifier::new().key_count(), GOOGLE_KEYS.len());
    assert_eq!(GoogleIdentityVerifier::new().verify(&identity), None);
    assert_eq!(GoogleIdentityVerifier::default().verify(&identity), None);
    assert_eq!(test_key().key_count(), 1);
}

/// Asserts that tokens signed by the key are accepted and read into the project, zone,
/// instance id and instance name of their `google.compute_engine` claim, whether they
/// name the instance of the tests, another instance, another project, or give the issuer
/// without its scheme. It catches a verifier that rejects a good signature, reads the wrong
/// fields, or checks the claims against the instance (the provider does that).
#[test]
fn a_token_signed_by_the_key_is_accepted_with_its_claims() {
    let verifier = test_key();
    assert_eq!(verifier.verify(&token(GOOD)), Some(claims(PROJECT, "1")));
    assert_eq!(
        verifier.verify(&token(OTHER_INSTANCE)),
        Some(claims(PROJECT, "2"))
    );
    assert_eq!(
        verifier.verify(&token(OTHER_PROJECT)),
        Some(claims("other-project", "1"))
    );
    assert_eq!(
        verifier.verify(&token(BARE_ISSUER)),
        Some(claims(PROJECT, "1"))
    );
}

/// Asserts that the identity's own `signature` field is not read: a token is accepted
/// whatever the field says. It catches a verifier that expects the signature apart from
/// the token.
#[test]
fn the_signature_field_of_the_identity_is_not_read() {
    let identity = SignedIdentity {
        document: GOOD.to_owned(),
        signature: "not read".to_owned(),
    };
    assert_eq!(test_key().verify(&identity), Some(claims(PROJECT, "1")));
}

/// Asserts that a token whose signature bytes are altered, whose signature is not base64,
/// whose header or claims changed after it was signed, or that is signed by a key the
/// verifier does not hold, is rejected. It catches a verifier that does not check the
/// signature against the exact header and claims.
#[test]
fn a_token_with_a_bad_signature_is_rejected() {
    let verifier = test_key();
    assert_eq!(verifier.verify(&token(ALTERED_SIGNATURE)), None);
    let (signed, _) = GOOD.rsplit_once('.').unwrap();
    assert_eq!(verifier.verify(&token(&format!("{signed}.!!!"))), None);
    assert_eq!(verifier.verify(&token(&format!("{signed}."))), None);
    let (header, claims) = signed.split_once('.').unwrap();
    let (_, other_claims) = OTHER_INSTANCE
        .rsplit_once('.')
        .unwrap()
        .0
        .split_once('.')
        .unwrap();
    let (_, signature) = GOOD.rsplit_once('.').unwrap();
    assert_ne!(claims, other_claims);
    assert_eq!(
        verifier.verify(&token(&format!("{header}.{other_claims}.{signature}"))),
        None
    );
    let stranger =
        GoogleIdentityVerifier::with_keys(vec![("test-key-1".to_owned(), vec![0xff; 256])]);
    assert_eq!(stranger.verify(&token(GOOD)), None);
}

/// Asserts that a correctly signed token is still rejected when its header does not say
/// RS256, names no key or a key the verifier does not hold, or when its claims name a
/// different issuer, none, no `google` claim, no `compute_engine` claim, or lack one of
/// the four fields, or are not base64 or not JSON. It catches a verifier that trusts a
/// signature without reading the algorithm, the key id, the issuer and the claims.
#[test]
fn a_signed_token_that_does_not_say_what_it_must_is_rejected() {
    let verifier = test_key();
    for (case, text) in [
        ("HS256", HS256),
        ("no alg", NO_ALG),
        ("no kid", NO_KID),
        ("unknown kid", UNKNOWN_KID),
        ("other issuer", OTHER_ISSUER),
        ("no issuer", NO_ISSUER),
        ("no google", NO_GOOGLE),
        ("no compute engine", NO_COMPUTE_ENGINE),
        ("missing claim", MISSING_CLAIM),
        ("claims not base64", CLAIMS_NOT_BASE64),
        ("claims not json", CLAIMS_NOT_JSON),
    ] {
        assert_eq!(verifier.verify(&token(text)), None, "{case}");
    }
}

/// Asserts that text that is not a token is rejected: empty, no dot, one dot, a header
/// that is not base64 or not JSON, and a header that is JSON but not an object with a key
/// id. It catches a verifier that reads a malformed token before checking its shape.
#[test]
fn text_that_is_not_a_token_is_rejected() {
    let verifier = test_key();
    for (case, text) in [
        ("empty", ""),
        ("no dot", "e30"),
        ("one dot", "e30.e30"),
        ("header not base64", "!!!.e30.c2ln"),
        ("header not json", "bm90IGpzb24.e30.c2ln"),
        ("header an array", "W10.e30.c2ln"),
        ("header empty object", "e30.e30.c2ln"),
        ("three dots", "e30.e30.e30.c2ln"),
    ] {
        assert_eq!(verifier.verify(&token(text)), None, "{case}");
    }
}

/// Asserts that the fake verifier accepts what it signed, reading the same four fields,
/// and rejects a document or signature changed, a signature made with another key, and a
/// document that is correctly signed but is not a JSON object with the four string fields
/// (not JSON, each field missing, a field that is not a string, a JSON value that is not
/// an object). It catches a document read without checking its fields.
#[test]
fn the_fake_verifier_checks_its_own_signature_and_the_fields() {
    let fake = FakeIdentityVerifier::new("test-key");
    let good = r#"{"project_id":"sample-project","zone":"region-one-a","instance_id":"7","instance_name":"js-host"}"#;
    let identity = fake.sign(good);
    assert_eq!(identity.document, good);
    assert_eq!(
        fake.verify(&identity),
        Some(IdentityClaims {
            project_id: "sample-project".to_owned(),
            zone: "region-one-a".to_owned(),
            instance_id: "7".to_owned(),
            instance_name: "js-host".to_owned(),
        })
    );
    let changed = SignedIdentity {
        document: good.replace("region-one-a", "region-one-b"),
        signature: identity.signature.clone(),
    };
    assert_eq!(fake.verify(&changed), None);
    let unsigned = SignedIdentity {
        signature: "00".to_owned(),
        ..identity.clone()
    };
    assert_eq!(fake.verify(&unsigned), None);
    assert_eq!(
        FakeIdentityVerifier::new("other-key").verify(&identity),
        None
    );
    for document in [
        "not json",
        "[]",
        r#"{"zone":"region-one-a","instance_id":"7","instance_name":"js-host"}"#,
        r#"{"project_id":"sample-project","instance_id":"7","instance_name":"js-host"}"#,
        r#"{"project_id":"sample-project","zone":"region-one-a","instance_name":"js-host"}"#,
        r#"{"project_id":"sample-project","zone":"region-one-a","instance_id":"7"}"#,
        r#"{"project_id":"sample-project","zone":"region-one-a","instance_id":7,"instance_name":"js-host"}"#,
    ] {
        assert_eq!(fake.verify(&fake.sign(document)), None, "{document}");
    }
}

/// Asserts that a provider checking tokens against the test key matches the hello of the
/// instance it launched when the token names that instance (project, zone, id and name),
/// and refuses the token of another instance and the token of another project, though
/// both are signed by the key. It catches a provider that trusts a signature without
/// comparing what it says with the instance it launched.
#[test]
fn a_provider_accepts_only_the_token_of_the_instance_it_launched() {
    let mut rig = rig(live(2));
    rig.provider = rig.provider.with_verifier(Arc::new(test_key()));
    let held = match rig.provider.acquire(&ask("verified")) {
        Acquired::Accepted { held, .. } => held,
        other => panic!("not accepted: {other:?}"),
    };
    let hello = |identity: &str, instance: bool| Hello {
        name: name_of(SCHEDULER, "verified"),
        instance_id: instance.then(|| held.id.to_string()),
        identity: Some(token(identity)),
    };
    for instance in [false, true] {
        assert_eq!(
            rig.provider.verify_instance(&hello(GOOD, instance)),
            Some(held.id.clone())
        );
        assert_eq!(
            rig.provider
                .verify_instance(&hello(OTHER_INSTANCE, instance)),
            None
        );
        assert_eq!(
            rig.provider
                .verify_instance(&hello(OTHER_PROJECT, instance)),
            None
        );
        assert_eq!(
            rig.provider
                .verify_instance(&hello(ALTERED_SIGNATURE, instance)),
            None
        );
    }
}
