//! The signer against the published vectors, and the rules the vectors do not show alone.

use std::time::{Duration, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use js_logstore::Credential;
use js_logstore::sigv4::{self, Scope, Signed, Unsigned};
use sha2::{Digest, Sha256};

use crate::vectors::{GET_VANILLA_CANONICAL_REQUEST, PUT_DATE_HEADER, VECTORS, Vector};

/// The pairs of `pairs`, owned.
fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// `vector` signed with `secret`, `path`, `service`, `headers` and `sign_also` in place of its
/// own.
fn sign_as(
    vector: &Vector,
    secret: &str,
    path: &str,
    service: &str,
    headers: &[(String, String)],
    sign_also: &[&str],
) -> Signed {
    let credential = Credential {
        access_key_id: vector.access_key_id.to_owned(),
        secret_access_key: secret.to_owned(),
        session_token: vector.session_token.map(str::to_owned),
        expires_at: UNIX_EPOCH + Duration::from_secs(vector.unix_seconds + 3600),
    };
    let query = owned(vector.query);
    let unsigned = Unsigned {
        method: vector.method,
        host: vector.host,
        path,
        query: &query,
        headers,
        sign_also,
        body: vector.body,
    };
    let scope = Scope {
        region: vector.region,
        service,
    };
    sigv4::sign(
        &unsigned,
        &credential,
        &scope,
        UNIX_EPOCH + Duration::from_secs(vector.unix_seconds),
    )
}

/// `vector` signed as published.
fn sign(vector: &Vector) -> Signed {
    let headers = owned(vector.headers);
    sign_as(
        vector,
        vector.secret_access_key,
        vector.path,
        vector.service,
        &headers,
        vector.sign_also,
    )
}

/// The vector named `name`.
fn vector(name: &str) -> &'static Vector {
    VECTORS.iter().find(|vector| vector.name == name).unwrap()
}

/// Asserts that every vector of the generic suite and of the S3 documentation signs to its
/// published `Authorization` header, with the published `x-amz-date`, the session token sent
/// when the vector has one, and `x-amz-content-sha256` (the body's hash) sent for S3 only.
/// Catches a wrong canonical request (header trimming, query order, path encoding), a header
/// left unsigned, a wrong date format and a key derivation error.
#[test]
fn signatures_match_the_published_vectors() {
    for vector in &VECTORS {
        let signed = sign(vector);
        assert_eq!(
            signed.header("authorization"),
            Some(vector.authorization().as_str()),
            "{}",
            vector.name
        );
        assert_eq!(
            signed.header("x-amz-date"),
            Some(vector.stamp),
            "{}",
            vector.name
        );
        assert_eq!(
            signed.header("x-amz-security-token"),
            vector.session_token,
            "{}",
            vector.name
        );
        let hash = sigv4::sha256_hex(vector.body);
        let expected_hash = (vector.service == "s3").then_some(hash.as_str());
        assert_eq!(
            signed.header("x-amz-content-sha256"),
            expected_hash,
            "{}",
            vector.name
        );
    }
}

/// Asserts that the S3 form signs the path as it is sent and the generic form encodes it
/// again: the PUT example's `/test%24file.text` signs as published for `s3` and differently
/// for another service, and the generic `get-utf8` vector given its path already encoded
/// signs differently from the published one.
#[test]
fn the_s3_form_signs_the_path_as_sent_and_the_generic_form_encodes_it_again() {
    let put = vector("s3-put-object");
    let headers = owned(put.headers);
    let as_generic = sign_as(
        put,
        put.secret_access_key,
        put.path,
        "service",
        &headers,
        put.sign_also,
    );
    let generic_signature = as_generic.header("authorization").unwrap();
    assert!(!generic_signature.ends_with(put.signature));

    let utf8 = vector("get-utf8");
    let once = sign_as(
        utf8,
        utf8.secret_access_key,
        "/%E1%88%B4",
        utf8.service,
        &[],
        &[],
    );
    assert!(
        !once
            .header("authorization")
            .unwrap()
            .ends_with(utf8.signature)
    );
    assert_eq!(
        sign_as(
            utf8,
            utf8.secret_access_key,
            utf8.path,
            utf8.service,
            &[],
            &[]
        )
        .header("authorization"),
        Some(utf8.authorization().as_str())
    );
}

/// Asserts that a header neither `x-amz-*` nor named is sent unsigned, that a named header the
/// request lacks is not signed, and that header names are signed in lower case whatever their
/// case: each leaves the published signature as it is.
#[test]
fn only_the_named_and_amz_headers_are_signed() {
    let vanilla = vector("get-vanilla");
    let extra = owned(&[("User-Agent", "js-logstore-test")]);
    let signed = sign_as(
        vanilla,
        vanilla.secret_access_key,
        "/",
        vanilla.service,
        &extra,
        &["range"],
    );
    assert_eq!(
        signed.header("authorization"),
        Some(vanilla.authorization().as_str())
    );

    let put = vector("s3-put-object");
    let shouting = owned(&[
        ("Date", PUT_DATE_HEADER),
        ("X-Amz-Storage-Class", "REDUCED_REDUNDANCY"),
    ]);
    let signed = sign_as(
        put,
        put.secret_access_key,
        put.path,
        put.service,
        &shouting,
        &["date"],
    );
    assert_eq!(
        signed.header("authorization"),
        Some(put.authorization().as_str())
    );
}

/// HMAC-SHA256 of `data` under `key`, by the `hmac` crate's own variable-length constructor.
fn reference_hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Asserts that a secret long enough that `AWS4` and it pass HMAC's block (64 bytes) still signs
/// as the HMAC definition does, compared with an independent derivation over the suite's
/// published canonical request. Catches a long key cut or padded rather than hashed.
#[test]
fn a_secret_longer_than_a_block_derives_the_published_way() {
    let vanilla = vector("get-vanilla");
    for secret in ["k".repeat(60), "k".repeat(61), "s".repeat(128)] {
        let signed = sign_as(vanilla, &secret, "/", vanilla.service, &[], &[]);
        let scope = format!(
            "{}/{}/{}/aws4_request",
            vanilla.date, vanilla.region, vanilla.service
        );
        let hashed: String = Sha256::digest(GET_VANILLA_CANONICAL_REQUEST.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let to_sign = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{hashed}", vanilla.stamp);
        let mut key = reference_hmac(format!("AWS4{secret}").as_bytes(), vanilla.date.as_bytes());
        for part in [vanilla.region, vanilla.service, "aws4_request"] {
            key = reference_hmac(&key, part.as_bytes());
        }
        let signature: String = reference_hmac(&key, to_sign.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let authorization = signed.header("authorization").unwrap();
        assert!(
            authorization.ends_with(&format!("Signature={signature}")),
            "{secret}"
        );
    }
}

/// Asserts the S3 encoding of a path and of a query: unreserved bytes kept, `/` kept only in a
/// path, every other byte `%XX` in upper case; a query's pairs encoded and sorted.
#[test]
fn encoding_is_the_signature_version_4_one() {
    assert_eq!(
        sigv4::uri_encode("a b/c~-._*$\u{e9}", true),
        "a%20b/c~-._%2A%24%C3%A9"
    );
    assert_eq!(sigv4::uri_encode("a b/c", false), "a%20b%2Fc");
    let pairs = owned(&[("b", "2"), ("a", "x y"), ("a", "1"), ("start-after", "p/q")]);
    assert_eq!(
        sigv4::canonical_query(&pairs),
        "a=1&a=x%20y&b=2&start-after=p%2Fq"
    );
    assert_eq!(sigv4::canonical_query(&[]), "");
}

/// Asserts that `Debug` of what signing adds names its headers and shows none of their values
/// (the signature and the session token among them), and that asking for a header it did not
/// add is `None`.
#[test]
fn signed_headers_debug_shows_names_only() {
    let signed = sign(vector("post-sts-header-before"));
    assert_eq!(
        format!("{signed:?}"),
        "Signed { headers: [\"x-amz-date\", \"x-amz-security-token\", \"authorization\"], .. }"
    );
    assert_eq!(signed.header("range"), None);
}
