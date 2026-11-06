//! The host upload credential of live logs: one temporary credential per host (agent), minted
//! by the object store's STS `AssumeRole` call with an inline session policy, or, for a store
//! with no STS, derived as a prefix grant (`docs/design/live-logs.md`, sections 4 and 4.1).
//!
//! - [`policy`] builds the session policy: one statement that allows `s3:PutObject` and
//!   `s3:DeleteObject` on `arn:aws:s3:::BUCKET/LH*`, where `LH` is the host's prefix
//!   ([`MintRequest::host_prefix`]): the configured log prefix `L` with its trailing `/` (empty
//!   for none) and the host's segment `js-<32 hex>/`.
//! - [`Minter`] turns a [`MintRequest`] (the bucket, the log prefix, the host and the lifetime)
//!   into a [`Credential`]. [`StsMinter`] is the production one: one signed `AssumeRole` call
//!   over a [`js_logstore::Transport`]. The `fake` feature's [`fake::FakeMinter`] issues
//!   credentials from [`js_logstore::fake::FakeStore`] with the same policy text, and can be
//!   scripted.
//! - [`PresignedPrefixMinter`] is the minter of a store with no STS (`credentials =
//!   "presigned_prefix"`): it makes no call, and derives from a signing key a
//!   [`js_logstore::PrefixGrant`] of the host's own prefix, [`host_prefix`], until an absolute
//!   expiry. [`host_segment`] names the host in both kinds: the STS session and the prefix.
//!
//! **What a credential can do**, of either kind: put and delete any object under its host's
//! prefix, for any job of that host, until it expires. **What it cannot do**: read or list any
//! object, or touch any object outside the host's prefix, another host's jobs included. A
//! session policy can only narrow what the role allows, so the STS credential can do no more
//! than this whatever the role grants.
//!
//! **What the operator configures in the store.** The role (`role_arn`) must allow at least
//! `s3:PutObject` and `s3:DeleteObject` on `arn:aws:s3:::BUCKET/L*` (every host's prefix), so
//! that each host's session policy narrows it to exactly that host's prefix; its trust policy must let the mint key call
//! `sts:AssumeRole` on it; and its maximum session duration must be at least the lifetime asked
//! (STS refuses a longer one, which is a [`MintError::Refused`]). A store that does not serve
//! `AssumeRole` takes [`PresignedPrefixMinter`] instead, and must check prefix grants.
//!
//! **Errors** ([`MintError`]) are told apart by transport kind, HTTP status and STS error code,
//! never by message text: `Unavailable` (retrying later may succeed) and `Refused` (it will not,
//! with the same configuration). No error, `Debug` output or log line of this crate holds a
//! secret access key, a session token or a signature.
//!
//! **What the crate never does.** It never retries (the caller owns retry and backoff), never
//! reads an environment variable or a file, and starts no thread or async runtime.

use std::time::Duration;

use js_logstore::{Credential, Unavailable};

#[cfg(feature = "fake")]
pub mod fake;
pub mod names;
pub mod policy;
pub mod presigned;
pub mod sts;

pub use names::{Bucket, LogPrefix, NameError, RoleArn, host_prefix, host_segment};
pub use policy::{POLICY_VERSION, policy};
pub use presigned::PresignedPrefixMinter;
pub use sts::{STS_API_VERSION, StsEndpoint, StsMinter};

/// What to mint: a credential for `host` that may put and delete under the host's own prefix
/// ([`MintRequest::host_prefix`]) under the log prefix `prefix` of `bucket`, for `ttl`. The
/// credential is for the host, not for a job: every log object of the host's jobs is under the
/// host's prefix and in its scope; no other host's is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintRequest {
    /// The bucket of the log store.
    pub bucket: Bucket,
    /// The configured log prefix.
    pub prefix: LogPrefix,
    /// The host's agent id. [`host_segment`] names the host by it: the STS session
    /// (`RoleSessionName`), so the store's audit trail names the host, and the host's own
    /// prefix of a prefix grant.
    pub host: String,
    /// How long the credential is asked to live.
    pub ttl: Duration,
}

/// Why a mint is refused when the host's prefix is longer than the object layout allows.
const PREFIX_TOO_LONG: &str = "the host's prefix is longer than the object layout allows";

impl MintRequest {
    /// The host's own prefix, [`host_prefix`] of the log prefix and the host: what every
    /// minter scopes the credential to, and where each job of the host is placed.
    ///
    /// # Errors
    ///
    /// [`MintError::Refused`] when the log prefix leaves no room for the host's segment (the
    /// configuration is checked at start so that it never does).
    pub fn host_prefix(&self) -> Result<LogPrefix, MintError> {
        host_prefix(&self.prefix, &self.host)
            .map_err(|_| MintError::Refused(Refusal::Answer(PREFIX_TOO_LONG)))
    }
}

/// Mints host credentials.
pub trait Minter: Send + Sync {
    /// A credential for `request`'s host, scoped to the host's own prefix
    /// ([`MintRequest::host_prefix`]): by session policy ([`StsMinter`]) or by prefix grant
    /// ([`PresignedPrefixMinter`]).
    ///
    /// # Errors
    ///
    /// [`MintError::Unavailable`] when retrying later may succeed, [`MintError::Refused`] when
    /// it will not.
    fn mint(&self, request: &MintRequest) -> Result<Credential, MintError>;
}

/// Why a mint failed. A failure is told apart by the transport's kind, the HTTP status and the
/// STS error code, never by a message text; it carries no secret, token or signature.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MintError {
    /// The store's STS could not be reached or asked to be called later: a transport failure,
    /// a timeout, a 5xx, a 429 or a 400 `Throttling`. Retrying later may succeed.
    #[error("the STS is unavailable: {0}")]
    Unavailable(Unavailable),
    /// The store's STS refused the call (a 403, or any 400 other than `Throttling`, such as
    /// `AccessDenied`, `InvalidClientTokenId` or `MalformedPolicyDocument`), or answered a
    /// credential that cannot be used; or no prefix grant can be built for the request.
    /// Retrying with the same configuration does not help.
    #[error("the STS refused the mint: {0}")]
    Refused(Refusal),
}

/// What made a mint refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The STS answered an error.
    #[error("status {status}, code `{code}`")]
    Status {
        /// The status.
        status: u16,
        /// The STS error code (1 to 64 ASCII letters and digits), or empty.
        code: String,
    },
    /// The STS answered success with a credential that cannot be used (a field missing or
    /// malformed, or a lifetime too short), or no prefix grant can be built for the request.
    #[error("{0}")]
    Answer(&'static str),
}
