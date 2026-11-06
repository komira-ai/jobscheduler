//! The minter of a store with no STS: a prefix grant per host, built here with no call to the
//! store (`docs/design/live-logs.md`, section 4.1).
//!
//! For a [`MintRequest`] it builds a [`PrefixGrant`] of the host's own prefix,
//! [`host_prefix`] of the request's log prefix and host (the same host name
//! [`crate::StsMinter`] gives the host's session), in the request's bucket, expiring at the
//! clock's now plus the request's lifetime (cut down to the whole second), derived from the
//! signing key the minter holds; and returns [`PrefixGrant::credential`]. The store holds the
//! same signing key and checks every request against the grant
//! ([`js_logstore::presigned`]).
//!
//! **What the credential can do**: put and delete any object under the host's prefix until it
//! expires. **What it cannot do**: read or list any object, or touch any object outside the
//! host's prefix, another host's included.
//!
//! A mint makes no request, so it is never [`MintError::Unavailable`]. It is
//! [`MintError::Refused`] only for a request no grant can be built for: a host prefix longer
//! than the object layout allows, or an expiry past what the clock can hold. The configuration
//! is checked at start so that neither happens.

use std::fmt;

use js_logstore::{Clock, Credential, PrefixGrant};

use crate::{MintError, MintRequest, Minter, Refusal};

/// Why a mint is refused when no grant can be built for it.
const NO_GRANT: &str = "no grant can be built for the expiry or the signing key";

/// The minter that derives a prefix grant per host from one signing key.
pub struct PresignedPrefixMinter<C> {
    clock: C,
    key: Credential,
}

impl<C> fmt::Debug for PresignedPrefixMinter<C> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("PresignedPrefixMinter")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl<C: Clock> PresignedPrefixMinter<C> {
    /// A minter that derives grants from the signing key `key` (its access key id and secret;
    /// its session token and expiry are not read), at the times `clock` reads.
    pub fn new(clock: C, key: Credential) -> Self {
        Self { clock, key }
    }
}

impl<C: Clock> Minter for PresignedPrefixMinter<C> {
    fn mint(&self, request: &MintRequest) -> Result<Credential, MintError> {
        let refused = |why| MintError::Refused(Refusal::Answer(why));
        let prefix = request.host_prefix()?;
        let grant = self
            .clock
            .now()
            .checked_add(request.ttl)
            .and_then(|expires_at| {
                PrefixGrant::new(
                    &self.key.access_key_id,
                    request.bucket.as_str(),
                    prefix.as_str(),
                    expires_at,
                )
                .ok()
            })
            .ok_or_else(|| refused(NO_GRANT))?;
        Ok(grant.credential(&self.key.secret_access_key))
    }
}
