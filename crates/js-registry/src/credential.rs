use std::fmt;

use sha2::Digest;

/// What a host presented to be admitted, reduced to its SHA-256 digest.
///
/// The registry binds a host id to the first credential it was admitted with and compares
/// later hellos against that binding, so it keeps the digest and never the token. The
/// transport computes it from the join token it authenticated the request with.
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
