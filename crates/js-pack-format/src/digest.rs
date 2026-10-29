//! The digest of one blob, and the hex form the wire types use for a hash.

use std::fmt;

use sha2::{Digest as _, Sha256};

/// The identity of one blob: its SHA-256 and its size in bytes.
///
/// Ordering is by hash, then size, which is the order of a pack's index. Two digests
/// with the same hash and different sizes are different digests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest {
    /// The SHA-256 of the blob.
    pub hash: [u8; 32],
    /// The blob's length in bytes.
    pub size_bytes: u64,
}

impl Digest {
    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            hash: Sha256::digest(bytes).into(),
            size_bytes: bytes.len() as u64,
        }
    }
}

impl fmt::Display for Digest {
    /// `<64 lowercase hex digits>/<size>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", to_hex(&self.hash), self.size_bytes)
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// `hash` as 64 lowercase hex digits.
pub(crate) fn to_hex(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in hash {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 15)]));
    }
    out
}

/// The hash in 64 lowercase hex digits, or `None` for any other text.
pub(crate) fn from_hex(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        *slot = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}
