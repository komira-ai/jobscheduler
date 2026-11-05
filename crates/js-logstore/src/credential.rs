//! What a client needs to reach the store: the credential it signs with and the endpoint it
//! talks to.
//!
//! A [`Credential`]'s `Debug` prints the access key id only: the secret access key and the
//! session token appear in no `Debug` output, no error and no log line of this crate.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::SystemTime;

use crate::sigv4::uri_encode;

/// The longest endpoint URL, in bytes.
pub const MAX_ENDPOINT_LEN: usize = 512;

/// A key to sign requests with, and when it stops being valid.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    /// The access key id: not secret, it names the key.
    pub access_key_id: String,
    /// The secret access key. Secret.
    pub secret_access_key: String,
    /// The session token of a temporary credential, sent with every request. Secret.
    pub session_token: Option<String>,
    /// The first instant at which the credential is no longer valid.
    pub expires_at: SystemTime,
}

impl Credential {
    /// Whether the credential is no longer valid at `now`.
    pub fn is_expired(&self, now: SystemTime) -> bool {
        now >= self.expires_at
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("Credential")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// Why an endpoint was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    /// The URL is longer than [`MAX_ENDPOINT_LEN`] bytes.
    #[error("the endpoint URL is longer than {MAX_ENDPOINT_LEN} bytes")]
    TooLong,
    /// The scheme is neither `https://` nor `http://`.
    #[error("the endpoint URL must begin with https:// or http://")]
    Scheme,
    /// The URL names a user (`user@host`).
    #[error("the endpoint URL must not name a user")]
    Userinfo,
    /// The URL has a query.
    #[error("the endpoint URL must not have a query")]
    Query,
    /// The URL has a fragment.
    #[error("the endpoint URL must not have a fragment")]
    Fragment,
    /// The URL has a path other than `/`.
    #[error("the endpoint URL must not have a path")]
    Path,
    /// The host is empty or holds a space, a control character or a `\`.
    #[error("the endpoint URL's host is empty or holds a character a host cannot")]
    Host,
    /// A plain `http://` URL names a host that is not a loopback address literal.
    #[error("plain http is accepted for a loopback address only")]
    PlainHttpNotLoopback,
    /// The region is not 1 to 64 bytes of `[a-z0-9-]`.
    #[error("the region must be 1 to 64 bytes of [a-z0-9-]")]
    Region,
    /// The bucket is not 3 to 63 bytes of `[a-z0-9.-]`.
    #[error("the bucket must be 3 to 63 bytes of [a-z0-9.-]")]
    Bucket,
}

/// Where the store is: its URL, the region requests are signed for, the bucket, and whether the
/// bucket goes in the path (`scheme://host/bucket/key`) or in the host
/// (`scheme://bucket.host/key`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    https: bool,
    authority: String,
    region: String,
    bucket: String,
    path_style: bool,
}

impl Endpoint {
    /// The endpoint at `base_url` (`https://host[:port]`, or `http://` to a loopback address
    /// literal such as `http://127.0.0.1:9000`; a trailing `/` is allowed, nothing else after
    /// the authority), with `region` (1 to 64 bytes of `[a-z0-9-]`) and `bucket` (3 to 63 bytes
    /// of `[a-z0-9.-]`).
    ///
    /// # Errors
    ///
    /// [`EndpointError`] names the first rule the arguments break.
    pub fn parse(
        base_url: &str,
        region: &str,
        bucket: &str,
        path_style: bool,
    ) -> Result<Self, EndpointError> {
        if base_url.len() > MAX_ENDPOINT_LEN {
            return Err(EndpointError::TooLong);
        }
        let (https, rest) = match (
            base_url.strip_prefix("https://"),
            base_url.strip_prefix("http://"),
        ) {
            (Some(rest), _) => (true, rest),
            (None, Some(rest)) => (false, rest),
            (None, None) => return Err(EndpointError::Scheme),
        };
        let authority = authority(rest)?;
        if !https && !loopback(authority) {
            return Err(EndpointError::PlainHttpNotLoopback);
        }
        let region_ok = (1..=64).contains(&region.len())
            && region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !region_ok {
            return Err(EndpointError::Region);
        }
        let bucket_ok = (3..=63).contains(&bucket.len())
            && bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
        if !bucket_ok {
            return Err(EndpointError::Bucket);
        }
        Ok(Self {
            https,
            authority: authority.to_owned(),
            region: region.to_owned(),
            bucket: bucket.to_owned(),
            path_style,
        })
    }

    /// The region requests are signed for.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// The bucket.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Whether the bucket goes in the path rather than in the host.
    pub fn path_style(&self) -> bool {
        self.path_style
    }

    /// The host requests go to and are signed for: the URL's authority, or the bucket and a `.`
    /// before it for a virtual-host-style endpoint.
    pub fn host(&self) -> String {
        match self.path_style {
            true => self.authority.clone(),
            false => format!("{}.{}", self.bucket, self.authority),
        }
    }

    /// The path of the bucket itself (where a listing is asked): `/bucket` or `/`.
    pub fn bucket_path(&self) -> String {
        match self.path_style {
            true => format!("/{}", self.bucket),
            false => "/".to_owned(),
        }
    }

    /// The path of the object `key`, percent-encoded as S3 does (every byte outside
    /// `[A-Za-z0-9-._~]` except `/`).
    pub fn object_path(&self, key: &str) -> String {
        let encoded = uri_encode(key, true);
        match self.path_style {
            true => format!("/{}/{encoded}", self.bucket),
            false => format!("/{encoded}"),
        }
    }

    /// The URL of the path `path` (one of [`Endpoint::bucket_path`] or
    /// [`Endpoint::object_path`]), without a query.
    pub fn url(&self, path: &str) -> String {
        let scheme = match self.https {
            true => "https",
            false => "http",
        };
        format!("{scheme}://{}{path}", self.host())
    }
}

/// The authority of an endpoint URL, from the text after its scheme.
fn authority(rest: &str) -> Result<&str, EndpointError> {
    let trimmed = rest.strip_suffix('/').unwrap_or(rest);
    let checks = [
        ('@', EndpointError::Userinfo),
        ('?', EndpointError::Query),
        ('#', EndpointError::Fragment),
        ('/', EndpointError::Path),
    ];
    if let Some((_, refusal)) = checks.into_iter().find(|(c, _)| trimmed.contains(*c)) {
        return Err(refusal);
    }
    let odd = |c: char| c.is_ascii_control() || c == ' ' || c == '\\';
    match trimmed.is_empty() || trimmed.chars().any(odd) {
        true => Err(EndpointError::Host),
        false => Ok(trimmed),
    }
}

/// Whether `authority` is an IP address literal on the loopback interface, with or without a
/// port.
fn loopback(authority: &str) -> bool {
    authority
        .parse::<SocketAddr>()
        .map(|addr| addr.ip())
        .or_else(|_| authority.parse::<IpAddr>())
        .is_ok_and(|ip| ip.is_loopback())
}
