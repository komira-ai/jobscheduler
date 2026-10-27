use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::Digest;
use subtle::ConstantTimeEq;

/// How many random bytes make a session secret.
pub(crate) const SECRET_BYTES: usize = 32;

/// Where the registry gets the bytes of a session secret.
///
/// The registry asks once per admitted hello. [`OsSessions`] is the production source;
/// [`SequentialSessions`] is a deterministic one for tests. A source that cannot produce
/// bytes answers `None`, and the hello is then refused (`session-unavailable`) rather
/// than admitted with a secret anyone could guess.
pub trait SessionSource: Send + Sync {
    /// Fresh bytes for one session secret, or `None` when none can be made.
    fn fresh(&self) -> Option<[u8; SECRET_BYTES]>;
}

/// Session secrets from the operating system's random source.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsSessions;

impl SessionSource for OsSessions {
    fn fresh(&self) -> Option<[u8; SECRET_BYTES]> {
        let mut bytes = [0; SECRET_BYTES];
        getrandom::getrandom(&mut bytes).ok().map(|()| bytes)
    }
}

/// A deterministic session source: the n-th call returns the number n in big-endian
/// bytes. **Not secret; for tests only.** Two registries given separate sources issue the
/// same sequence.
#[derive(Debug, Default)]
pub struct SequentialSessions {
    next: AtomicU64,
}

impl SequentialSessions {
    /// A source whose first call returns the number 1.
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionSource for SequentialSessions {
    fn fresh(&self) -> Option<[u8; SECRET_BYTES]> {
        let n = self.next.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        let mut bytes = [0; SECRET_BYTES];
        bytes[SECRET_BYTES - 8..].copy_from_slice(&n.to_be_bytes());
        Some(bytes)
    }
}

/// The secret text that goes in a welcome: the bytes in lower-case hex, which is inside
/// the alphabet and the length cap the wire holds a session to.
pub(crate) fn secret_text(bytes: &[u8; SECRET_BYTES]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(SECRET_BYTES * 2);
    for byte in bytes {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 15)]));
    }
    text
}

/// The SHA-256 digest of a session secret. The registry keeps this and never the secret,
/// so a copy of its state cannot be replayed as a heartbeat.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SessionDigest([u8; 32]);

impl SessionDigest {
    pub(crate) fn of(secret: &str) -> Self {
        Self(sha2::Sha256::digest(secret.as_bytes()).into())
    }

    /// Whether `presented` is the secret this is the digest of. The two digests are
    /// compared in constant time, so the time taken does not tell how many leading bytes
    /// of a guess were right.
    pub(crate) fn matches(&self, presented: &str) -> bool {
        self.0.ct_eq(&Self::of(presented).0).into()
    }
}

impl fmt::Debug for SessionDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionDigest(..)")
    }
}
