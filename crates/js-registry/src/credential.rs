use std::fmt;

use js_agent_wire::InstanceIdentity;
use sha2::Digest;

/// What a host presented to be admitted, reduced to its SHA-256 digest.
///
/// The registry binds a host id to the first credential it was admitted with and compares
/// later hellos against that binding, so it keeps the digest and never the token. The
/// transport computes it from the join token it authenticated the request with.
///
/// Equality is on the digest, which is what the registry compares. The constant-time
/// comparison of the token itself, when the transport checks a request against the
/// pool's accepted tokens, belongs to the transport and is not done here.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential([u8; 32]);

impl Credential {
    /// The credential of a join token.
    pub fn of_token(token: &str) -> Self {
        Self(sha2::Sha256::digest(token.as_bytes()).into())
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential(..)")
    }
}

/// What a host id is bound to: the credential its first hello was admitted with and, when
/// that hello named the instance it runs on, that instance.
///
/// A later hello for the id must match the credential, and must match the instance when
/// the binding has one; any mismatch is a refusal as host-bound. A binding without an
/// instance holds at the level of the pool credential only: a holder of the pool's token
/// can say hello for such an id once the host is lost. Per-host tokens or client
/// certificates close that later (design D18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    credential: Credential,
    instance: Option<InstanceIdentity>,
}

impl Binding {
    /// A binding to `credential`, and to `instance` when there is one.
    pub fn new(credential: Credential, instance: Option<InstanceIdentity>) -> Self {
        Self {
            credential,
            instance,
        }
    }

    pub(crate) fn credential(&self) -> &Credential {
        &self.credential
    }

    /// Whether a hello presenting `credential` and `instance` comes from what this
    /// binding names.
    pub(crate) fn admits(
        &self,
        credential: &Credential,
        instance: Option<&InstanceIdentity>,
    ) -> bool {
        let same_credential = self.credential == *credential;
        let same_instance = self
            .instance
            .as_ref()
            .is_none_or(|bound| Some(bound) == instance);
        [same_credential, same_instance].into_iter().all(|ok| ok)
    }
}
