//! A prefix grant: an upload credential for a store that has no STS, and the signer that
//! presigns requests with it (`docs/design/live-logs.md`, section 4.1).
//!
//! **The grant.** The server holds a long-lived signing key (an access key id and a secret the
//! store also holds). For a host it builds a [`PrefixGrant`]: the signing key's id, the bucket,
//! a prefix and an absolute expiry, written as one token,
//!
//! ```text
//! jspp1:<signing key id>:<bucket>:<expiry, whole seconds since the Unix epoch>:<prefix>
//! ```
//!
//! and hands the host [`PrefixGrant::credential`]: a [`Credential`] whose session token is that
//! token, whose secret is the lower-case hex HMAC-SHA256 of the token under the signing secret,
//! whose access key id is `JSPP` and the first 32 hex digits of the token's SHA-256, and whose
//! expiry is the grant's. The host never sees the signing secret, and cannot change the prefix
//! or the expiry without changing the token, and so the secret it would need.
//!
//! **What the store checks** (and [`crate::fake::FakeStore`] does): the token parses; its
//! signing key is one the store holds; the access key id is the one the token gives; the
//! request is signed with Signature Version 4, header or query form, with the secret derived
//! from the token; the bucket is the token's; the time is before the expiry; and the call is a
//! put or a delete of a key under the prefix. A grant allows nothing else: no read, no list.
//!
//! **The signer.** [`PrefixSigner`] presigns puts and deletes with a grant's credential. It
//! refuses a key outside the prefix, a key that is not plain under it (an empty, `.` or `..`
//! segment, or a byte outside `[A-Za-z0-9._-]`), and any request at or after the expiry, each
//! with a [`GrantError`] and before any request exists. It holds no transport: the caller sends
//! what it returns.
//!
//! The prefix is non-empty, ends in `/`, and is `/`-separated segments of the layout's alphabet
//! ([`crate::layout`]), so a grant's prefix `a/b/` never covers `a/bc/`.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::client::{Metadata, check_key, put_headers};
use crate::clock::Clock;
use crate::credential::{Credential, Endpoint};
use crate::error::StoreError;
use crate::http::{HttpRequest, Method};
use crate::layout::{MAX_PREFIX_LEN, segment_ok};
use crate::sigv4::{MAX_PRESIGN_SECS, Scope, Unsigned, canonical_query, hex, hmac, presign};

/// The first field of every grant token: the version of its form.
pub const GRANT_TOKEN_VERSION: &str = "jspp1";

/// What the access key id of every grant's credential begins with.
pub const GRANT_KEY_ID_PREFIX: &str = "JSPP";

/// The hex digits of the token's SHA-256 a grant's access key id keeps.
const KEY_ID_DIGITS: usize = 32;

/// The longest signing key id, in bytes.
pub const MAX_SIGNING_KEY_ID_LEN: usize = 128;

/// Why a grant could not be built or read, or why the signer refused a request. It never
/// repeats a key, a token or a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// A field of the grant breaks its rule, or a credential is not a grant's.
    #[error("not a prefix grant: {0}")]
    Malformed(&'static str),
    /// The grant is for another bucket than the endpoint's.
    #[error("the grant is for another bucket")]
    WrongBucket,
    /// The key does not begin with the grant's prefix.
    #[error("the key is outside the grant's prefix")]
    OutsidePrefix,
    /// The key begins with the prefix, but what follows has an empty, `.` or `..` segment, or
    /// a byte outside `[A-Za-z0-9._-]`.
    #[error("the key is not a plain key under the grant's prefix")]
    KeyNotPlain,
    /// The time is at or after the grant's expiry.
    #[error("the grant has expired")]
    Expired,
    /// The request is one the client never sends (a body over the put limit, bad metadata).
    #[error("{0}")]
    Invalid(StoreError),
}

/// What a grant allows: puts and deletes under `prefix` of `bucket` until `expires_at`, signed
/// with a secret derived from the signing key `signing_key_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixGrant {
    signing_key_id: String,
    bucket: String,
    prefix: String,
    expires_at_secs: u64,
}

impl PrefixGrant {
    /// The grant of `prefix` in `bucket`, derived from the signing key `signing_key_id`, until
    /// `expires_at` cut down to the whole second.
    ///
    /// # Errors
    ///
    /// [`GrantError::Malformed`] when the key id is not 1 to [`MAX_SIGNING_KEY_ID_LEN`] ASCII
    /// letters and digits, the bucket is not 3 to 63 bytes of `[a-z0-9.-]`, the prefix is not
    /// as the module docs say, or the expiry is not after the Unix epoch.
    pub fn new(
        signing_key_id: &str,
        bucket: &str,
        prefix: &str,
        expires_at: SystemTime,
    ) -> Result<Self, GrantError> {
        let key_ok = (1..=MAX_SIGNING_KEY_ID_LEN).contains(&signing_key_id.len())
            && signing_key_id.bytes().all(|b| b.is_ascii_alphanumeric());
        if !key_ok {
            return Err(GrantError::Malformed("the signing key id"));
        }
        let bucket_ok = (3..=63).contains(&bucket.len())
            && bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
        if !bucket_ok {
            return Err(GrantError::Malformed("the bucket"));
        }
        prefix
            .strip_suffix('/')
            .filter(|inner| inner.len() <= MAX_PREFIX_LEN && inner.split('/').all(segment_ok))
            .ok_or(GrantError::Malformed("the prefix"))?;
        let expires_at_secs = expires_at
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs())
            .ok()
            .filter(|secs| *secs > 0)
            .ok_or(GrantError::Malformed("the expiry"))?;
        Ok(Self {
            signing_key_id: signing_key_id.to_owned(),
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
            expires_at_secs,
        })
    }

    /// The grant a token writes, exactly as [`PrefixGrant::token`] writes it.
    ///
    /// # Errors
    ///
    /// [`GrantError::Malformed`] when `token` is not five `:`-separated fields beginning with
    /// [`GRANT_TOKEN_VERSION`], a field breaks its rule, or the token is not the one the grant
    /// it holds writes (a leading zero in the expiry, say).
    pub fn parse(token: &str) -> Result<Self, GrantError> {
        let malformed = || GrantError::Malformed("the token");
        let mut fields = token.splitn(5, ':');
        let mut next = || fields.next().ok_or_else(malformed);
        let (version, key, bucket, expiry, prefix) = (next()?, next()?, next()?, next()?, next()?);
        if version != GRANT_TOKEN_VERSION || !expiry.bytes().all(|b| b.is_ascii_digit()) {
            return Err(malformed());
        }
        let secs: u64 = expiry.parse().map_err(|_| malformed())?;
        let at = UNIX_EPOCH
            .checked_add(Duration::from_secs(secs))
            .ok_or_else(malformed)?;
        let grant = Self::new(key, bucket, prefix, at)?;
        match grant.token() == token {
            true => Ok(grant),
            false => Err(malformed()),
        }
    }

    /// The token: the module docs' form.
    pub fn token(&self) -> String {
        format!(
            "{GRANT_TOKEN_VERSION}:{}:{}:{}:{}",
            self.signing_key_id, self.bucket, self.expires_at_secs, self.prefix
        )
    }

    /// The credential a host signs with under this grant, derived with `signing_secret`, the
    /// secret of the signing key the grant names.
    pub fn credential(&self, signing_secret: &str) -> Credential {
        let token = self.token();
        Credential {
            access_key_id: grant_key_id(&token),
            secret_access_key: hex(&hmac(signing_secret.as_bytes(), token.as_bytes())),
            session_token: Some(token),
            expires_at: self.expires_at(),
        }
    }

    /// The id of the signing key the grant's secret is derived from.
    pub fn signing_key_id(&self) -> &str {
        &self.signing_key_id
    }

    /// The bucket.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// The prefix, ending in `/`.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The first instant at which the grant is no longer valid.
    pub fn expires_at(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.expires_at_secs)
    }

    /// Whether the grant allows a request for `key` at `now`.
    ///
    /// # Errors
    ///
    /// [`GrantError::Expired`] at or after the expiry; [`GrantError::OutsidePrefix`] for a key
    /// that does not begin with the prefix; [`GrantError::KeyNotPlain`] for one whose rest is
    /// not plain segments.
    pub fn check(&self, key: &str, now: SystemTime) -> Result<(), GrantError> {
        if now >= self.expires_at() {
            return Err(GrantError::Expired);
        }
        let rest = key
            .strip_prefix(self.prefix.as_str())
            .ok_or(GrantError::OutsidePrefix)?;
        match rest.split('/').all(segment_ok) {
            true => Ok(()),
            false => Err(GrantError::KeyNotPlain),
        }
    }

    /// The seconds a request presigned at `now` stays valid: to the expiry, at most
    /// [`MAX_PRESIGN_SECS`].
    fn seconds_left(&self, now: SystemTime) -> u64 {
        let now_secs = now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        self.expires_at_secs
            .saturating_sub(now_secs)
            .min(MAX_PRESIGN_SECS)
    }
}

/// The access key id of the credential of the grant whose token is `token`.
pub fn grant_key_id(token: &str) -> String {
    let digest = crate::sigv4::sha256_hex(token.as_bytes());
    format!("{GRANT_KEY_ID_PREFIX}{}", &digest[..KEY_ID_DIGITS])
}

/// Presigns puts and deletes under one grant, at the times its clock reads.
pub struct PrefixSigner<C> {
    clock: C,
    endpoint: Endpoint,
    credential: Credential,
    grant: PrefixGrant,
}

impl<C> fmt::Debug for PrefixSigner<C> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("PrefixSigner")
            .field("endpoint", &self.endpoint)
            .field("grant", &self.grant)
            .finish_non_exhaustive()
    }
}

impl<C: Clock> PrefixSigner<C> {
    /// A signer for `endpoint` with the grant credential `credential`, reading time from
    /// `clock`.
    ///
    /// # Errors
    ///
    /// [`GrantError::Malformed`] when `credential` is not a grant's (no token, a token that
    /// does not parse, or an access key id or expiry other than the token gives);
    /// [`GrantError::WrongBucket`] when the grant is for another bucket than `endpoint`'s.
    pub fn new(endpoint: Endpoint, credential: Credential, clock: C) -> Result<Self, GrantError> {
        let token = credential
            .session_token
            .as_deref()
            .ok_or(GrantError::Malformed("the credential has no token"))?;
        let grant = PrefixGrant::parse(token)?;
        if credential.access_key_id != grant_key_id(token)
            || credential.expires_at != grant.expires_at()
        {
            return Err(GrantError::Malformed("the credential is not the token's"));
        }
        if grant.bucket() != endpoint.bucket() {
            return Err(GrantError::WrongBucket);
        }
        Ok(Self {
            clock,
            endpoint,
            credential,
            grant,
        })
    }

    /// The grant.
    pub fn grant(&self) -> &PrefixGrant {
        &self.grant
    }

    /// A presigned put of `body` at `key` with `metadata` and the content type
    /// `application/octet-stream`, which the store accepts until the grant expires.
    ///
    /// # Errors
    ///
    /// What [`PrefixGrant::check`] says of `key` now, and [`GrantError::Invalid`] for a body
    /// over the put limit, a key over the key limit or bad metadata.
    pub fn put(
        &self,
        key: &str,
        body: Vec<u8>,
        metadata: &Metadata,
    ) -> Result<HttpRequest, GrantError> {
        let headers = put_headers(key, body.len(), metadata).map_err(GrantError::Invalid)?;
        self.presigned(Method::Put, key, headers, &["content-type"], body)
    }

    /// A presigned delete of `key`, which the store accepts until the grant expires.
    ///
    /// # Errors
    ///
    /// What [`PrefixGrant::check`] says of `key` now, and [`GrantError::Invalid`] for a key
    /// over the key limit.
    pub fn delete(&self, key: &str) -> Result<HttpRequest, GrantError> {
        check_key(key).map_err(GrantError::Invalid)?;
        self.presigned(Method::Delete, key, Vec::new(), &[], Vec::new())
    }

    /// The presigned request, after the grant's check of `key` at the clock's time.
    fn presigned(
        &self,
        method: Method,
        key: &str,
        mut headers: Vec<(String, String)>,
        sign_also: &[&str],
        body: Vec<u8>,
    ) -> Result<HttpRequest, GrantError> {
        let now = self.clock.now();
        self.grant.check(key, now)?;
        let host = self.endpoint.host();
        let path = self.endpoint.object_path(key);
        let unsigned = Unsigned {
            method: method.as_str(),
            host: &host,
            path: &path,
            query: &[],
            headers: &headers,
            sign_also,
            body: &[],
        };
        let scope = Scope {
            region: self.endpoint.region(),
            service: "s3",
        };
        let query = presign(
            &unsigned,
            &self.credential,
            &scope,
            now,
            self.grant.seconds_left(now),
        );
        let url = format!("{}?{}", self.endpoint.url(&path), canonical_query(&query));
        headers.push(("host".to_owned(), host));
        Ok(HttpRequest {
            method,
            url,
            headers,
            body,
        })
    }
}
