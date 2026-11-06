//! AWS Signature Version 4: the header form with a hashed payload ([`sign`]), and the query
//! form of a presigned request ([`presign`]).
//!
//! Both are pure: the time is an argument. [`sign`] It signs `host`, every `x-amz-*` header it is
//! given, the ones it adds (`x-amz-date`, `x-amz-content-sha256` for S3, and
//! `x-amz-security-token` when the credential has a session token) and any further header the
//! caller names. The payload is always hashed: `x-amz-content-sha256` is the body's SHA-256,
//! never `UNSIGNED-PAYLOAD`.
//!
//! The service decides the form. For `s3` the path is signed as it is sent (S3 encodes it once)
//! and `x-amz-content-sha256` is sent; for any other service (`sts`, the generic test suite's
//! `service`) the path as sent is encoded again, and no `x-amz-content-sha256` is added.
//!
//! [`presign`] puts the signature in the query instead (`X-Amz-Algorithm`, `X-Amz-Credential`,
//! `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-SignedHeaders`, `X-Amz-Security-Token` with a session
//! token, and `X-Amz-Signature`), as a presigned S3 URL does: it signs `host`, every `x-amz-*`
//! header it is given and any further header the caller names, and the payload is
//! `UNSIGNED-PAYLOAD`, since the URL is made before the body is known to the store.

use std::collections::BTreeMap;
use std::fmt;
use std::time::SystemTime;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::credential::Credential;

/// The algorithm's name in the string to sign and the `Authorization` header.
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// The payload hash of a presigned request: its body is not signed.
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// The longest a presigned request may be valid, in seconds (seven days), as S3 allows.
pub const MAX_PRESIGN_SECS: u64 = 604_800;

/// The block size of SHA-256: an HMAC key is padded (or hashed) to it.
const BLOCK: usize = 64;

/// A request to sign.
pub struct Unsigned<'a> {
    /// The method, upper case.
    pub method: &'a str,
    /// The host the request goes to, with the port when the URL has one.
    pub host: &'a str,
    /// The path as it is sent, percent-encoded.
    pub path: &'a str,
    /// The query's names and values, not encoded: the signer encodes them.
    pub query: &'a [(String, String)],
    /// The headers sent besides `host` and those the signer adds. A name may be in any case.
    pub headers: &'a [(String, String)],
    /// Further headers to sign besides `host` and the `x-amz-*` ones, by lower-case name (for
    /// example `range`, `content-type`). A name no header has is not signed.
    pub sign_also: &'a [&'a str],
    /// The body.
    pub body: &'a [u8],
}

/// The region and the service a request is signed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope<'a> {
    /// The region, for example `us-east-1`.
    pub region: &'a str,
    /// The service: `s3` or `sts`.
    pub service: &'a str,
}

/// What signing adds to a request: the headers to send with it.
#[derive(Clone, PartialEq, Eq)]
pub struct Signed {
    /// `x-amz-date`, `x-amz-content-sha256` (S3), `x-amz-security-token` (with a session token)
    /// and `authorization`, with their values, in that order.
    pub headers: Vec<(String, String)>,
}

impl Signed {
    /// The value of the added header `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(have, _)| have == name)
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for Signed {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        out.debug_struct("Signed")
            .field("headers", &names)
            .finish_non_exhaustive()
    }
}

/// Signs `request` with `credential` for `scope` at `now`.
pub fn sign(
    request: &Unsigned<'_>,
    credential: &Credential,
    scope: &Scope<'_>,
    now: SystemTime,
) -> Signed {
    let s3 = scope.service == "s3";
    let (date, stamp) = dates(now);
    let mut added = vec![("x-amz-date".to_owned(), stamp.clone())];
    let payload = hex(&Sha256::digest(request.body));
    if s3 {
        added.push(("x-amz-content-sha256".to_owned(), payload.clone()));
    }
    if let Some(token) = &credential.session_token {
        added.push(("x-amz-security-token".to_owned(), token.clone()));
    }

    let (signed_names, header_lines) = canonical_headers(request, &added);
    let canonical_request = format!(
        "{}\n{}\n{}\n{header_lines}\n{signed_names}\n{payload}",
        request.method,
        canonical_path(request.path, s3),
        canonical_query(request.query),
    );
    let credential_scope = credential_scope(&date, scope);
    let signature = signature(credential, scope, &date, &stamp, &canonical_request);
    added.push((
        "authorization".to_owned(),
        format!(
            "{ALGORITHM} Credential={}/{credential_scope}, SignedHeaders={signed_names}, Signature={signature}",
            credential.access_key_id
        ),
    ));
    Signed { headers: added }
}

/// The query parameters that presign `request` with `credential` for `scope` at `now`, valid
/// for `expires_secs` seconds from `now` (clamped to 1 to [`MAX_PRESIGN_SECS`]): the
/// `X-Amz-*` parameters in the order of the module docs, `X-Amz-Signature` last. The request
/// is sent with its own query and these, its headers, and `host`; `request.body` is not
/// signed.
pub fn presign(
    request: &Unsigned<'_>,
    credential: &Credential,
    scope: &Scope<'_>,
    now: SystemTime,
    expires_secs: u64,
) -> Vec<(String, String)> {
    let s3 = scope.service == "s3";
    let (date, stamp) = dates(now);
    let (signed_names, header_lines) = canonical_headers(request, &[]);
    let mut added = vec![
        ("X-Amz-Algorithm".to_owned(), ALGORITHM.to_owned()),
        (
            "X-Amz-Credential".to_owned(),
            format!(
                "{}/{}",
                credential.access_key_id,
                credential_scope(&date, scope)
            ),
        ),
        ("X-Amz-Date".to_owned(), stamp.clone()),
        (
            "X-Amz-Expires".to_owned(),
            expires_secs.clamp(1, MAX_PRESIGN_SECS).to_string(),
        ),
        ("X-Amz-SignedHeaders".to_owned(), signed_names.clone()),
    ];
    if let Some(token) = &credential.session_token {
        added.push(("X-Amz-Security-Token".to_owned(), token.clone()));
    }
    let query: Vec<(String, String)> = request.query.iter().chain(&added).cloned().collect();
    let canonical_request = format!(
        "{}\n{}\n{}\n{header_lines}\n{signed_names}\n{UNSIGNED_PAYLOAD}",
        request.method,
        canonical_path(request.path, s3),
        canonical_query(&query),
    );
    let signature = signature(credential, scope, &date, &stamp, &canonical_request);
    added.push(("X-Amz-Signature".to_owned(), signature));
    added
}

/// The signed header names (`;`-joined) and the canonical header lines of `request` with the
/// signer's own `added` headers: `host`, every `x-amz-*` header and those `sign_also` names.
fn canonical_headers(request: &Unsigned<'_>, added: &[(String, String)]) -> (String, String) {
    let mut canonical: BTreeMap<String, Vec<String>> = BTreeMap::new();
    canonical.insert("host".to_owned(), vec![trim(request.host)]);
    for (name, value) in request.headers.iter().chain(added) {
        let name = name.to_ascii_lowercase();
        if name.starts_with("x-amz-") || request.sign_also.contains(&name.as_str()) {
            canonical.entry(name).or_default().push(trim(value));
        }
    }
    let signed_names = canonical.keys().cloned().collect::<Vec<_>>().join(";");
    let header_lines: String = canonical
        .iter()
        .map(|(name, values)| format!("{name}:{}\n", values.join(",")))
        .collect();
    (signed_names, header_lines)
}

/// The path as it is signed: as sent for `s3`, encoded again for any other service.
fn canonical_path(path: &str, s3: bool) -> String {
    match s3 {
        true => path.to_owned(),
        false => uri_encode(path, true),
    }
}

/// The credential scope of a signature made on `date` (`YYYYMMDD`) for `scope`.
fn credential_scope(date: &str, scope: &Scope<'_>) -> String {
    format!("{date}/{}/{}/aws4_request", scope.region, scope.service)
}

/// The hex signature of `canonical_request`, made at `stamp` on `date` for `scope` with
/// `credential`'s secret.
fn signature(
    credential: &Credential,
    scope: &Scope<'_>,
    date: &str,
    stamp: &str,
    canonical_request: &str,
) -> String {
    let string_to_sign = format!(
        "{ALGORITHM}\n{stamp}\n{}\n{}",
        credential_scope(date, scope),
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let secret = format!("AWS4{}", credential.secret_access_key);
    let key = [scope.region, scope.service, "aws4_request"]
        .iter()
        .fold(hmac(secret.as_bytes(), date.as_bytes()), |key, part| {
            hmac(&key, part.as_bytes())
        });
    hex(&hmac(&key, string_to_sign.as_bytes()))
}

/// `text` percent-encoded as Signature Version 4 requires: every byte outside
/// `[A-Za-z0-9-._~]` becomes `%XX` (upper-case hex), and `/` stays as it is when `keep_slash`.
pub fn uri_encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        let plain = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && byte == b'/');
        match plain {
            true => out.push(char::from(byte)),
            false => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The query string of `pairs`: each name and value encoded, sorted, joined by `&`. It is the
/// canonical query of the signature and the query a request is sent with.
pub fn canonical_query(pairs: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| (uri_encode(name, false), uri_encode(value, false)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// The SHA-256 of `bytes`, as lower-case hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The date (`YYYYMMDD`) and the time stamp (`YYYYMMDDTHHMMSSZ`) of `now`, in UTC.
fn dates(now: SystemTime) -> (String, String) {
    let at = OffsetDateTime::from(now);
    let (year, month, day) = at.to_calendar_date();
    let (hour, minute, second) = at.to_hms();
    let date = format!("{year:04}{:02}{day:02}", u8::from(month));
    let stamp = format!("{date}T{hour:02}{minute:02}{second:02}Z");
    (date, stamp)
}

/// A header value as it is signed: no space at either end, and each run of spaces one space.
fn trim(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// HMAC-SHA256 of `data` under `key`. A key longer than a block is hashed first and a shorter
/// one is padded with zeros, as the HMAC definition does, so the key always fills one block.
pub(crate) fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0_u8; BLOCK];
    let hashed;
    let key = match key.len() > BLOCK {
        true => {
            hashed = Sha256::digest(key);
            hashed.as_slice()
        }
        false => key,
    };
    block[..key.len()].copy_from_slice(key);
    let mut mac = <Hmac<Sha256> as KeyInit>::new(&block.into());
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// `bytes` as lower-case hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    text
}
