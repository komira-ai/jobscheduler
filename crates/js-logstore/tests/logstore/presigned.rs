//! Prefix grants and the signer that presigns with them: the token's form and the credential
//! derived from it, every refusal of a malformed grant, the signer's refusals before any
//! request, its expiry boundary on an injected clock, a signature fixed by its inputs, and the
//! query form against the published S3 example.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use js_logstore::presigned::{GRANT_TOKEN_VERSION, grant_key_id};
use js_logstore::sigv4::{self, Scope, Unsigned};
use js_logstore::{
    Credential, GrantError, ManualClock, Metadata, Method, PrefixGrant, PrefixSigner, StoreError,
    Transport,
};
use sha2::{Digest, Sha256};

use crate::support::{BUCKET, Rig, START, at, endpoint};
use crate::vectors::PRESIGNED_GET;

/// The prefix every grant here covers.
const PREFIX: &str = "a/b/js-host1/";

/// The grant of [`PREFIX`] in the test bucket from the signing key `SIGNKEY1`, for an hour
/// from [`START`].
fn grant() -> PrefixGrant {
    PrefixGrant::new("SIGNKEY1", BUCKET, PREFIX, at(START + 3600)).unwrap()
}

/// Lower-case hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A signer of `credential` on the path-style test endpoint, on a clock at `now`.
fn signer(credential: Credential, now: SystemTime) -> PrefixSigner<ManualClock> {
    PrefixSigner::new(endpoint(true), credential, ManualClock::new(now)).unwrap()
}

/// Asserts the token's exact form, that it reads back to the same grant, the accessors, and
/// the credential derived from it, computed here independently: the access key id is `JSPP`
/// and the first 32 hex digits of the token's SHA-256, the secret the hex HMAC-SHA256 of the
/// token under the signing secret, the session token the token, and the expiry the grant's.
/// An expiry with a fraction of a second is cut down to the whole second. It catches a token
/// that does not round-trip, a secret derived from anything but the token, and an expiry
/// rounded up.
#[test]
fn a_grant_writes_its_token_and_derives_its_credential() {
    let grant = grant();
    let token = format!(
        "{GRANT_TOKEN_VERSION}:SIGNKEY1:logs:{}:{PREFIX}",
        START + 3600
    );
    assert_eq!(grant.token(), token);
    assert_eq!(PrefixGrant::parse(&token), Ok(grant.clone()));
    assert_eq!(
        (grant.signing_key_id(), grant.bucket(), grant.prefix()),
        ("SIGNKEY1", BUCKET, PREFIX)
    );
    assert_eq!(grant.expires_at(), at(START + 3600));

    let credential = grant.credential("signing-secret");
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(b"signing-secret").unwrap();
    mac.update(token.as_bytes());
    let digest = hex(&Sha256::digest(token.as_bytes()));
    assert_eq!(credential.access_key_id, format!("JSPP{}", &digest[..32]));
    assert_eq!(credential.access_key_id, grant_key_id(&token));
    assert_eq!(
        credential.secret_access_key,
        hex(&mac.finalize().into_bytes())
    );
    assert_eq!(credential.session_token.as_deref(), Some(token.as_str()));
    assert_eq!(credential.expires_at, at(START + 3600));
    assert_ne!(grant.credential("other-secret"), credential);

    let fraction = at(START + 3600) + Duration::from_millis(999);
    let cut = PrefixGrant::new("SIGNKEY1", BUCKET, PREFIX, fraction).unwrap();
    assert_eq!(cut.expires_at(), at(START + 3600));
}

/// Asserts every refusal of a grant's fields, each `Malformed` with the field named: a signing
/// key id that is empty, of 129 bytes or not letters and digits; a bucket of 2 bytes or with
/// an upper-case letter; a prefix that is empty, `/`, without its trailing `/`, with an empty,
/// `.` or `..` segment, a `*`, or of 513 bytes before its `/`; an expiry at or before the
/// epoch. And every refusal of a token: another version, too few fields, an expiry with a sign,
/// a leading zero, past `u64` or past what a time can hold, and a field the grant refuses. A prefix of exactly 512 bytes
/// is taken, and so is a bucket of every allowed kind of byte. It catches a grant whose prefix could widen its scope and a token read loosely.
#[test]
fn a_grant_refuses_each_malformed_field_and_token() {
    let expiry = at(START);
    let field = |key: &str, bucket: &str, prefix: &str, expires_at: SystemTime| {
        PrefixGrant::new(key, bucket, prefix, expires_at).err()
    };
    let malformed = |what| Some(GrantError::Malformed(what));
    let long_key = "k".repeat(129);
    for key in ["", long_key.as_str(), "SIGN-KEY", "SIGN:KEY"] {
        assert_eq!(
            field(key, BUCKET, PREFIX, expiry),
            malformed("the signing key id")
        );
    }
    for bucket in ["ab", "Logs", "lo:gs"] {
        assert_eq!(field("K", bucket, PREFIX, expiry), malformed("the bucket"));
    }
    let long = format!("{}/", "p".repeat(513));
    for prefix in [
        "",
        "/",
        "a/b",
        "a//b/",
        "./",
        "a/../",
        "a/*/",
        "a:b/",
        long.as_str(),
    ] {
        assert_eq!(
            field("K", BUCKET, prefix, expiry),
            malformed("the prefix"),
            "{prefix}"
        );
    }
    let longest = format!("{}/", "p".repeat(512));
    assert!(field("K", BUCKET, &longest, expiry).is_none());
    assert!(field("K", "l0.g-s", PREFIX, expiry).is_none());
    let before = UNIX_EPOCH - Duration::from_secs(1);
    for expires_at in [UNIX_EPOCH, before] {
        assert_eq!(
            field("K", BUCKET, PREFIX, expires_at),
            malformed("the expiry")
        );
    }

    let token = malformed("the token");
    for text in [
        "jspp2:K:logs:1800003600:a/b/",
        "jspp1:K:logs:1800003600",
        "jspp1:K:logs:+1800003600:a/b/",
        "jspp1:K:logs:01800003600:a/b/",
        "jspp1:K:logs:99999999999999999999999:a/b/",
        "jspp1:K:logs:18446744073709551615:a/b/",
        "",
    ] {
        assert_eq!(PrefixGrant::parse(text).err(), token, "{text}");
    }
    assert_eq!(
        PrefixGrant::parse("jspp1:K:logs:1800003600:a/b").err(),
        malformed("the prefix")
    );
}

/// Asserts the exact presigned put of a chunk under the prefix: the URL with the query
/// parameters in canonical order, the signature among them (a fixed value, so a signer that
/// leaves the prefix, the token or the expiry out of what it signs is caught), the content
/// type, the metadata and the host sent as headers, and the body. The fake store, holding the
/// signing key, accepts it and stores the object with its metadata, and accepts a presigned
/// delete of it.
#[test]
fn the_signer_presigns_a_put_and_a_delete_under_its_prefix() {
    let rig = Rig::new();
    let key = rig.fake.prefix_signing_key();
    let grant = PrefixGrant::new(&key.access_key_id, BUCKET, PREFIX, at(START + 3600)).unwrap();
    let credential = grant.credential(&key.secret_access_key);
    let signer = PrefixSigner::new(rig.endpoint.clone(), credential, rig.clock.clone()).unwrap();
    assert_eq!(signer.grant(), &grant);
    let object = format!("{PREFIX}job-1/stdout/00000000000000000000");
    let metadata = Metadata::from([("first-byte-at-ms".to_owned(), "17".to_owned())]);
    let put = signer.put(&object, b"bytes".to_vec(), &metadata).unwrap();
    assert_eq!(put.method, Method::Put);
    let (path, query) = put.url.split_once('?').unwrap();
    assert_eq!(
        path,
        "https://store.example.com/logs/a/b/js-host1/job-1/stdout/00000000000000000000"
    );
    let token = grant.token().replace(':', "%3A").replace('/', "%2F");
    let expected = format!(
        "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}%2F20270115%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20270115T080000Z&X-Amz-Expires=3600&X-Amz-Security-Token={token}&X-Amz-Signature=8f2100f2035dfbbde7e6e33129fab5417b914e8908cdb635f7ba476af23b2c20&X-Amz-SignedHeaders=content-type%3Bhost%3Bx-amz-meta-first-byte-at-ms",
        grant_key_id(&grant.token()),
    );
    assert_eq!(query, expected);
    assert_eq!(
        put.headers,
        [
            (
                "content-type".to_owned(),
                "application/octet-stream".to_owned()
            ),
            ("x-amz-meta-first-byte-at-ms".to_owned(), "17".to_owned()),
            ("host".to_owned(), "store.example.com".to_owned()),
        ]
    );
    assert_eq!(put.body, b"bytes");
    assert_eq!(rig.fake.send(put).unwrap().status, 200);
    assert_eq!(rig.fake.objects()[&object], b"bytes");
    let read = rig.admin().get_range(&object, 0, 5).unwrap().unwrap();
    assert_eq!(read.metadata, metadata);

    let delete = signer.delete(&object).unwrap();
    assert_eq!((delete.method, delete.body.len()), (Method::Delete, 0));
    assert_eq!(rig.fake.send(delete).unwrap().status, 204);
    assert!(rig.fake.objects().is_empty());
}

/// Asserts that the signer refuses, with a typed error and no request, a key outside the
/// prefix (another prefix, the sibling host's prefix, the prefix's parent, the prefix without
/// its `/`), a key that escapes it with `..` or holds a `.` segment, an empty segment, a
/// trailing `/` or a byte outside the layout's alphabet, and the prefix itself; and a request
/// the client never sends: a body over the put limit, bad metadata, a key over the key limit.
/// The signer holds no transport, so a refusal cannot reach the network. It catches a prefix
/// check by `starts_with` alone, which a `..` key passes.
#[test]
fn the_signer_refuses_a_key_outside_its_prefix_before_any_request() {
    let signer = signer(grant().credential("signing-secret"), at(START));
    let none = Metadata::new();
    for key in [
        "a/b/js-host2/job-1/stdout.log",
        "a/b/job-1/stdout.log",
        "a/b/js-host1",
        "a/b/js-host1x/job-1/stdout.log",
        "other/job-1/stdout.log",
    ] {
        assert_eq!(
            signer.put(key, b"x".to_vec(), &none),
            Err(GrantError::OutsidePrefix),
            "{key}"
        );
        assert_eq!(signer.delete(key), Err(GrantError::OutsidePrefix), "{key}");
    }
    for rest in [
        "../js-host2/job-1/x",
        "job-1/../../js-host2/x",
        "./x",
        "job-1//x",
        "job-1/",
        "",
        "job 1",
        "job*",
    ] {
        let key = format!("{PREFIX}{rest}");
        assert_eq!(
            signer.put(&key, b"x".to_vec(), &none),
            Err(GrantError::KeyNotPlain),
            "{key}"
        );
        assert_eq!(signer.delete(&key), Err(GrantError::KeyNotPlain), "{key}");
    }
    let key = format!("{PREFIX}job-1/stdout.log");
    let large = vec![0; js_logstore::MAX_PUT_BYTES + 1];
    assert!(matches!(
        signer.put(&key, large, &none),
        Err(GrantError::Invalid(StoreError::TooLarge { .. }))
    ));
    let bad = Metadata::from([("Bad Name".to_owned(), "v".to_owned())]);
    assert!(matches!(
        signer.put(&key, b"x".to_vec(), &bad),
        Err(GrantError::Invalid(StoreError::Invalid(_)))
    ));
    let long = format!("{PREFIX}{}", "k".repeat(1024));
    assert!(matches!(
        signer.delete(&long),
        Err(GrantError::Invalid(StoreError::Invalid(_)))
    ));
}

/// Asserts the expiry on an injected clock: one nanosecond before the grant's expiry the
/// signer presigns a put valid for 1 second, which the fake store accepts; at the expiry and
/// after it the signer refuses with `Expired` and makes no request. A grant whose expiry is more
/// than seven days off presigns for seven days, the most S3 takes. It catches an expiry off by
/// one and a request signed past it.
#[test]
fn the_signer_refuses_at_and_after_the_expiry_and_signs_a_tick_before() {
    let rig = Rig::new();
    let key = rig.fake.prefix_signing_key();
    let grant = PrefixGrant::new(&key.access_key_id, BUCKET, PREFIX, at(START + 3600)).unwrap();
    let credential = grant.credential(&key.secret_access_key);
    let object = format!("{PREFIX}job-1/stdout.log");
    let none = Metadata::new();
    let tick = Duration::from_nanos(1);
    rig.clock.advance(Duration::from_secs(3600) - tick);
    let signer =
        PrefixSigner::new(rig.endpoint.clone(), credential.clone(), rig.clock.clone()).unwrap();
    let put = signer.put(&object, b"x".to_vec(), &none).unwrap();
    assert!(put.url.contains("&X-Amz-Expires=1&"), "{}", put.url);
    assert_eq!(rig.fake.send(put).unwrap().status, 200);
    rig.clock.advance(tick);
    assert_eq!(
        signer.put(&object, b"x".to_vec(), &none),
        Err(GrantError::Expired)
    );
    assert_eq!(signer.delete(&object), Err(GrantError::Expired));
    rig.clock.advance(Duration::from_secs(1));
    assert_eq!(
        signer.put(&object, b"x".to_vec(), &none),
        Err(GrantError::Expired)
    );

    let far = PrefixGrant::new("K", BUCKET, PREFIX, at(START + 30 * 24 * 3600)).unwrap();
    let put = signer_at_start(far.credential("s")).put(&object, b"x".to_vec(), &none);
    assert!(put.unwrap().url.contains("&X-Amz-Expires=604800&"));
}

/// A signer of `credential` at [`START`].
fn signer_at_start(credential: Credential) -> PrefixSigner<ManualClock> {
    signer(credential, at(START))
}

/// The presigned put of `key` by a signer of `credential` at [`START`].
fn url_of(credential: Credential, key: &str) -> String {
    signer_at_start(credential)
        .put(key, b"x".to_vec(), &Metadata::new())
        .unwrap()
        .url
}

/// Asserts that a presigned request is a function of its inputs: the same grant, key and clock
/// give the same URL, and a different signing secret, key or expiry each give a different
/// signature. It catches a signer that ignores what it is given.
#[test]
fn a_signature_is_fixed_by_the_key_the_path_and_the_expiry() {
    let key = format!("{PREFIX}job-1/stdout.log");
    let signature = |url: String| url.split("X-Amz-Signature=").nth(1).unwrap()[..64].to_owned();
    let base = signature(url_of(grant().credential("signing-secret"), &key));
    assert_eq!(
        base,
        signature(url_of(grant().credential("signing-secret"), &key))
    );
    let other_secret = signature(url_of(grant().credential("other-secret"), &key));
    let other_path = signature(url_of(
        grant().credential("signing-secret"),
        &format!("{PREFIX}job-2/stdout.log"),
    ));
    let later = PrefixGrant::new("SIGNKEY1", BUCKET, PREFIX, at(START + 3601)).unwrap();
    let other_expiry = signature(url_of(later.credential("signing-secret"), &key));
    for other in [other_secret, other_path, other_expiry] {
        assert_ne!(other, base);
    }
}

/// Asserts that a signer is made only from a grant's credential for the endpoint's bucket: no
/// session token, a token that does not parse, an access key id or an expiry other than the
/// token's are `Malformed`, and a grant of another bucket is `WrongBucket`. It catches a
/// signer that trusts a credential the store would refuse.
#[test]
fn a_signer_takes_only_a_grant_for_its_bucket() {
    let good = grant().credential("signing-secret");
    let make = |credential: Credential| {
        PrefixSigner::new(endpoint(true), credential, ManualClock::new(at(START))).err()
    };
    let not_token = Some(GrantError::Malformed("the credential is not the token's"));
    let cases = [
        (
            Credential {
                session_token: None,
                ..good.clone()
            },
            Some(GrantError::Malformed("the credential has no token")),
        ),
        (
            Credential {
                session_token: Some("token".to_owned()),
                ..good.clone()
            },
            Some(GrantError::Malformed("the token")),
        ),
        (
            Credential {
                access_key_id: "JSPP0".to_owned(),
                ..good.clone()
            },
            not_token.clone(),
        ),
        (
            Credential {
                expires_at: at(START + 7200),
                ..good.clone()
            },
            not_token,
        ),
        (
            PrefixGrant::new("SIGNKEY1", "other", PREFIX, at(START + 3600))
                .unwrap()
                .credential("s"),
            Some(GrantError::WrongBucket),
        ),
    ];
    for (credential, error) in cases {
        assert_eq!(make(credential), error);
    }
    assert!(make(good).is_none());
    let shown = format!(
        "{:?}",
        signer_at_start(grant().credential("signing-secret"))
    );
    assert!(
        !shown.contains("signing-secret") && shown.contains(PREFIX),
        "{shown}"
    );
}

/// Asserts the query form against the published S3 presigned GET example: the same five
/// parameters before the signature, and exactly the published signature. It catches a
/// canonical request that differs from S3's in any part.
#[test]
fn the_query_form_matches_the_published_example() {
    let vector = &PRESIGNED_GET;
    let credential = Credential {
        access_key_id: vector.access_key_id.to_owned(),
        secret_access_key: vector.secret_access_key.to_owned(),
        session_token: None,
        expires_at: UNIX_EPOCH + Duration::from_secs(vector.unix_seconds + 1),
    };
    let unsigned = Unsigned {
        method: "GET",
        host: vector.host,
        path: vector.path,
        query: &[],
        headers: &[],
        sign_also: &[],
        body: b"ignored",
    };
    let scope = Scope {
        region: "us-east-1",
        service: "s3",
    };
    let now = UNIX_EPOCH + Duration::from_secs(vector.unix_seconds);
    let query = sigv4::presign(&unsigned, &credential, &scope, now, vector.expires_secs);
    let expected = [
        ("X-Amz-Algorithm", "AWS4-HMAC-SHA256"),
        ("X-Amz-Credential", vector.credential),
        ("X-Amz-Date", vector.stamp),
        ("X-Amz-Expires", "86400"),
        ("X-Amz-SignedHeaders", "host"),
        ("X-Amz-Signature", vector.signature),
    ];
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    assert_eq!(query, expected);
    let clamped = sigv4::presign(&unsigned, &credential, &scope, now, 0);
    assert_eq!(clamped[3].1, "1");
}
