//! What the store refuses of a request this crate signed wrong: the checks the fake store makes
//! with this crate's own signer, made here by the store, so that a signer that is wrong in the
//! same way on both sides is caught.

use std::time::{Duration, SystemTime};

use js_logmint::{MintError, Minter, Refusal};
use js_logstore::{ManualClock, Metadata, Store, StoreError, Stream, UreqTransport};
use js_testkit::s3::unique;

use super::support::{Rig, SHORTEST_TTL};

/// A region that is not the test store's.
fn other_region(region: &str) -> &'static str {
    match region {
        "us-east-1" => "eu-west-1",
        _ => "us-east-1",
    }
}

/// Proves the store checks the region a request is signed for: a put signed for another region
/// is refused and nothing is stored, and an `AssumeRole` signed for another region is refused.
/// Catches a signer, or a configuration, that puts a region in the credential scope other than
/// the store's.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_request_signed_for_another_region_is_refused() {
    let rig = Rig::new();
    let wrong = other_region(&rig.test.region);
    let elsewhere = Store::new(
        UreqTransport::new(Duration::from_secs(30)),
        js_logstore::SystemClock,
        Rig::endpoint_in(&rig.test, wrong),
        rig.root(),
    );
    let key = rig.job(&rig.prefix, "job-1").chunk_key(Stream::Stdout, 0);
    let put = elsewhere.put(&key, b"x".to_vec(), &Metadata::new());
    // The store answers 400 `AuthorizationHeaderMalformed`, a status the client does not
    // expect of a put.
    assert_eq!(
        put,
        Err(StoreError::Malformed(
            "the store answered a status the client does not expect"
        )),
        "a put signed for {wrong}"
    );
    let root = rig.client(rig.root());
    assert_eq!(root.get_range(&key, 0, 1), Ok(None));
    let request = rig.request(&unique("host-a"), SHORTEST_TTL);
    assert_eq!(
        rig.minter_in(wrong).mint(&request),
        Err(MintError::Refused(Refusal::Status {
            status: 403,
            code: "AccessDenied".to_owned(),
        }))
    );
}

/// Proves the store checks the signature itself: a put signed with a secret that is not the
/// key's is refused as `SignatureDoesNotMatch`, and nothing is stored. Catches a signing key
/// derived other than the store derives it.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_request_signed_with_the_wrong_secret_is_refused() {
    let rig = Rig::new();
    let mut wrong = rig.root();
    wrong.secret_access_key.push('x');
    let key = rig.job(&rig.prefix, "job-1").chunk_key(Stream::Stdout, 0);
    assert_eq!(
        rig.client(wrong).put(&key, b"x".to_vec(), &Metadata::new()),
        Err(StoreError::Denied {
            status: 403,
            code: "SignatureDoesNotMatch".to_owned(),
        })
    );
    assert_eq!(rig.client(rig.root()).get_range(&key, 0, 1), Ok(None));
}

/// Proves the window of signing times the store takes, which the live-log tests of js-agent and
/// js-lease-server keep their moved clocks inside: a put signed 16 minutes ahead of the store's
/// clock is refused as `RequestTimeTooSkewed`, and one signed 14 minutes ahead is taken.
/// Catches a signing time sent other than as the clock read it.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_request_signed_outside_the_stores_window_is_refused() {
    let rig = Rig::new();
    let key = rig.job(&rig.prefix, "job-1").chunk_key(Stream::Stdout, 0);
    let ahead = |minutes: u64| {
        let clock = ManualClock::new(SystemTime::now() + Duration::from_secs(minutes * 60));
        rig.client_at(rig.root(), clock)
            .put(&key, b"x".to_vec(), &Metadata::new())
    };
    assert_eq!(
        ahead(16),
        Err(StoreError::Denied {
            status: 403,
            code: "RequestTimeTooSkewed".to_owned(),
        })
    );
    ahead(14).expect("a signing time inside the window");
}

/// Proves the store takes a minted credential only with the session token it was issued with,
/// and that the client reads its refusal of any other as an expired credential, so the agent
/// asks for a new one: the credential without its token, and with a token one byte longer
/// (one the store did not issue, as after a revocation), are each refused with the store's
/// `InvalidTokenId`, and nothing is stored. Catches a token sent in a header other than the one
/// the store reads or left out of the signed headers, and a store's refusal of the token read
/// as a refusal to retry with the same credential, which stops an upload until the
/// credential's expiry nears.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_minted_credential_is_refused_without_its_own_session_token() {
    let rig = Rig::new();
    let (credential, own) = rig.mint(&unique("host-a"));
    let key = rig.job(&own, "job-1").chunk_key(Stream::Stdout, 0);
    let mut bare = credential.clone();
    bare.session_token = None;
    let mut forged = credential;
    if let Some(token) = forged.session_token.as_mut() {
        token.push('x');
    }
    let put = |credential| {
        rig.client(credential)
            .put(&key, b"x".to_vec(), &Metadata::new())
    };
    let refused = Err(StoreError::Expired {
        code: "InvalidTokenId".to_owned(),
    });
    assert_eq!(put(bare), refused);
    assert_eq!(put(forged), refused);
    assert_eq!(rig.client(rig.root()).get_range(&key, 0, 1), Ok(None));
}
