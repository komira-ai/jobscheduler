//! SHA-256 as lower-case hex, for the one name that is a hash of several byte strings.
//!
//! It lives outside `sched` because the scheduler's modules name no build-system type
//! and the hashing trait of the `sha2` crate carries a build-system name.

use sha2::{Digest, Sha256};

/// The SHA-256 of `parts` fed in order, as 64 lower-case hex digits.
pub(crate) fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
