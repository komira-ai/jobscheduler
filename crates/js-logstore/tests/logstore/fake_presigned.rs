//! The fake store's check of prefix grants, driven by raw requests in both Signature Version 4
//! forms: what it accepts, and each refusal with its code, in the order a store checks.

use std::time::{Duration, SystemTime};

use js_logstore::fake::Action;
use js_logstore::presigned::grant_key_id;
use js_logstore::sigv4::{self, Scope, Unsigned, canonical_query};
use js_logstore::{Credential, HttpRequest, Metadata, Method, PrefixGrant, StoreError, Transport};

use crate::support::{BUCKET, REGION, Rig, START, at, code, policy};

/// A presigned request's query parameters.
type Query = Vec<(String, String)>;

/// The prefix every grant here covers.
const PREFIX: &str = "a/b/js-host1/";

/// A key under [`PREFIX`].
const KEY: &str = "a/b/js-host1/job-1/stdout.log";

/// The path of `key` on the rig's path-style endpoint.
fn path(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

/// A put of `key` presigned with `credential` at `now`, valid for `expires` seconds, its query
/// parameters changed by `edit` after signing.
fn presigned_put(
    credential: &Credential,
    key: &str,
    now: SystemTime,
    expires: u64,
    edit: impl Fn(&mut Query),
) -> HttpRequest {
    let path = path(key);
    let unsigned = Unsigned {
        method: "PUT",
        host: "store.example.com",
        path: &path,
        query: &[],
        headers: &[],
        sign_also: &[],
        body: &[],
    };
    let scope = Scope {
        region: REGION,
        service: "s3",
    };
    let mut query = sigv4::presign(&unsigned, credential, &scope, now, expires);
    edit(&mut query);
    HttpRequest {
        method: Method::Put,
        url: format!(
            "https://store.example.com{path}?{}",
            canonical_query(&query)
        ),
        headers: vec![("host".to_owned(), "store.example.com".to_owned())],
        body: b"bytes".to_vec(),
    }
}

/// The rig, a signing key of its fake store, and the credential of a grant of [`PREFIX`]
/// derived from it, valid for an hour.
fn granted() -> (Rig, Credential, Credential) {
    let rig = Rig::new();
    let key = rig.fake.prefix_signing_key();
    let grant = PrefixGrant::new(&key.access_key_id, BUCKET, PREFIX, at(START + 3600)).unwrap();
    let credential = grant.credential(&key.secret_access_key);
    (rig, key, credential)
}

/// The code of the fake's answer to `request`, or `ok` for a 2xx.
fn answer(rig: &Rig, request: HttpRequest) -> String {
    let response = rig.fake.send(request).unwrap();
    match response.status {
        200..=299 => "ok".to_owned(),
        _ => code(&response),
    }
}

/// Asserts what a grant may do in the fake store in the header form, as the agent's own
/// client signs: put and delete under the prefix, and nothing else (a put under the sibling
/// host's prefix, a get and a list are `AccessDenied`); and that the signing key itself signs
/// nothing (`AccessDenied`). A credential the fake issued itself is still checked as before in
/// the query form. It catches a grant that reads, a grant that writes beside its prefix, and a
/// signing key that is a credential.
#[test]
fn a_grant_puts_and_deletes_under_its_prefix_in_either_form_and_does_nothing_else() {
    let (rig, key, credential) = granted();
    let none = Metadata::new();
    let client = rig.client(credential);
    client.put(KEY, b"bytes".to_vec(), &none).unwrap();
    client.delete(KEY).unwrap();
    let denied = |answer: Result<(), StoreError>| matches!(answer, Err(StoreError::Denied { ref code, .. }) if code == "AccessDenied");
    assert!(denied(client.put(
        "a/b/js-host2/job-1/stdout.log",
        b"x".to_vec(),
        &none
    )));
    assert!(denied(client.get_range(KEY, 0, 5).map(drop)));
    assert!(denied(client.list(PREFIX, None, 10).map(drop)));
    assert!(denied(rig.client(key).put(KEY, b"x".to_vec(), &none)));

    let issued = rig.fake.issue(policy(&[Action::Put], "a/"));
    let request = presigned_put(&issued, KEY, at(START), 60, |_| ());
    assert_eq!(answer(&rig, request), "ok");
    assert_eq!(rig.fake.objects()[KEY], b"bytes");
}

/// Asserts each refusal of a presigned put, in the order the fake checks: a signature
/// parameter missing or malformed (the credential without its scope, the date, an expiry that is not 1 to 604,800 seconds, the
/// signed headers) is `SignatureDoesNotMatch`; a token that does not parse, that names a key
/// the fake never issued or a key that is not a signing key, that is for another bucket, or
/// whose access key id is not the token's is `InvalidAccessKeyId`; an expired grant is
/// `ExpiredToken`; a request past its own `X-Amz-Expires` is `AccessDenied`; and a signature,
/// an expiry or an extra parameter changed after signing is `SignatureDoesNotMatch`. Nothing
/// refused is stored. It catches a check skipped and a grant trusted for what it claims.
#[test]
fn a_presigned_request_is_refused_for_each_fault() {
    let (rig, key, credential) = granted();
    let now = at(START);
    let put = |credential: &Credential, edit: &dyn Fn(&mut Query)| {
        answer(&rig, presigned_put(credential, KEY, now, 60, edit))
    };
    let set = |name: &'static str, value: &'static str| {
        move |query: &mut Vec<(String, String)>| {
            query.retain(|(have, _)| have != name);
            query.push((name.to_owned(), value.to_owned()));
        }
    };
    let drop_param = |name: &'static str| {
        move |query: &mut Vec<(String, String)>| query.retain(|(have, _)| have != name)
    };
    let mismatch = "SignatureDoesNotMatch";
    assert_eq!(
        put(&credential, &set("X-Amz-Credential", "noscope")),
        mismatch
    );
    for name in [
        "X-Amz-Credential",
        "X-Amz-Date",
        "X-Amz-Expires",
        "X-Amz-SignedHeaders",
    ] {
        assert_eq!(put(&credential, &drop_param(name)), mismatch, "{name}");
    }
    assert_eq!(put(&credential, &set("X-Amz-Date", "yesterday")), mismatch);
    for expires in ["soon", "0", "604801", "18446744073709551615"] {
        assert_eq!(
            put(&credential, &set("X-Amz-Expires", expires)),
            mismatch,
            "{expires}"
        );
    }

    let invalid = "InvalidAccessKeyId";
    let token = |text: &str| Credential {
        access_key_id: grant_key_id(text),
        session_token: Some(text.to_owned()),
        ..credential.clone()
    };
    let minting = rig.fake.minting_key();
    let grant_of = |signing: &str, bucket: &str| {
        let grant = PrefixGrant::new(signing, bucket, PREFIX, at(START + 3600)).unwrap();
        grant.credential(&key.secret_access_key)
    };
    let strangers = [
        token("not a token"),
        Credential {
            session_token: None,
            ..credential.clone()
        },
        grant_of("NOSUCHKEY", BUCKET),
        grant_of(&minting.access_key_id, BUCKET),
        grant_of(&key.access_key_id, "other"),
        Credential {
            access_key_id: "JSPP0".to_owned(),
            ..credential.clone()
        },
    ];
    for stranger in &strangers {
        assert_eq!(put(stranger, &|_| ()), invalid);
    }

    let late = presigned_put(&credential, KEY, now, 7200, |_| ());
    let stale = presigned_put(&credential, KEY, now, 60, |_| ());
    rig.clock.advance(Duration::from_secs(60));
    assert_eq!(answer(&rig, stale), "AccessDenied");
    rig.clock.advance(Duration::from_secs(3600));
    assert_eq!(answer(&rig, late), "ExpiredToken");

    let (rig, _, credential) = granted();
    let put =
        |edit: &dyn Fn(&mut Query)| answer(&rig, presigned_put(&credential, KEY, now, 60, edit));
    let zeros = "0000000000000000000000000000000000000000000000000000000000000000";
    assert_eq!(put(&set("X-Amz-Signature", zeros)), mismatch);
    assert_eq!(put(&set("X-Amz-Expires", "61")), mismatch);
    assert_eq!(put(&set("X-Amz-Extra", "1")), mismatch);
    assert_eq!(put(&|_| ()), "ok");
    assert_eq!(rig.fake.objects().len(), 1);
}
