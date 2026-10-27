//! A stand-in for a build's action digest, for the tests that need a stored-result
//! identity and a dedup text: a hash and a size, whose text form is `<lowercase hex>/<size>`.
//!
//! The scheduler never sees a digest: it keys on a `WorkKey` and stores a `ResultRef`.
//! These tests name their work by a digest because the dedup text and the result
//! identity are what a digest bridges to (the bytes of the simulations' trace hashes
//! depend on that bridge), so the stand-in makes them exactly as the bridge does.

use std::fmt;

use js_types::sched::{ResultRef, WorkKey};

/// The hash function of a digest; there is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DigestFunction {
    /// SHA-256: 32 bytes.
    Sha256,
}

/// A hash and the size of what it hashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest {
    /// The function that made `hash`.
    pub function: DigestFunction,
    /// The hash.
    pub hash: [u8; 32],
    /// The size in bytes.
    pub size_bytes: u64,
}

impl Digest {
    /// The digest of `size_bytes` bytes that hash to `hash`.
    pub const fn new(function: DigestFunction, hash: [u8; 32], size_bytes: u64) -> Self {
        Self {
            function,
            hash,
            size_bytes,
        }
    }

    /// Reads the text form `<64 lowercase hex digits>/<decimal size>`.
    pub fn parse(function: DigestFunction, text: &str) -> Result<Self, String> {
        let (hex, size) = text
            .split_once('/')
            .ok_or_else(|| format!("{text:?} has no `/`"))?;
        let bytes = hex.as_bytes();
        if bytes.len() != 64 {
            return Err(format!("{hex:?} is not 64 hex digits"));
        }
        let nibble = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(format!("{hex:?} has a character outside 0-9a-f")),
        };
        let mut hash = [0u8; 32];
        for (out, pair) in hash.iter_mut().zip(bytes.chunks(2)) {
            *out = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        let size_bytes = size
            .parse()
            .map_err(|_| format!("{size:?} is not a size"))?;
        Ok(Self::new(function, hash, size_bytes))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.hash {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "/{}", self.size_bytes)
    }
}

impl From<Digest> for ResultRef {
    fn from(digest: Digest) -> Self {
        Self::new(digest.hash, digest.size_bytes)
    }
}

/// The key of the action `action` on the instance `instance`: the instance is the key's
/// class and the digest's text form its dedup text.
pub fn work_key(instance: &str, action: Digest) -> WorkKey {
    WorkKey::new(instance, action.to_string())
}
