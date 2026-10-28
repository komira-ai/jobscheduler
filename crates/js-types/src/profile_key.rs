//! The name an action key's learned profile is kept under.
//!
//! It lives outside `sched`, whose modules name no build-system type, because the hashing
//! trait of the `sha2` crate carries one.

use sha2::{Digest, Sha256};

use crate::sched::WorkKey;

/// The byte between a key's class and its dedup text (as in `sched::usage::key_hash`).
const KEY_SEPARATOR: u8 = 0xff;

/// The name an action key's learned profile is kept under: 64 lower-case hex digits, the
/// SHA-256 of the class bytes, one `0xff` byte and the dedup bytes (the framing of
/// [`crate::sched::usage::key_hash`]; `0xff` is never in UTF-8 text, so no two keys feed the same bytes).
///
/// Unlike [`crate::sched::usage::key_hash`] it is a name, not a count key: two keys do not share one. The
/// result always fits the grammar of a profile key (1 to 128 bytes of `0-9a-z._-`), and
/// the server that stores profiles and admission that reads their curves must both use
/// this function, so it is pinned by test vectors and never changes.
#[must_use]
pub fn profile_key(key: &WorkKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.class.as_bytes());
    hasher.update([KEY_SEPARATOR]);
    hasher.update(key.dedup.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts three pinned vectors of the profile name: an ordinary class and a dedup
    /// text with a slash, a class with a slash and capitals, and an empty dedup. They are
    /// the contract between the server that stores profiles and admission that reads
    /// them. It catches a change of framing, of hash or of hex case, and a name that
    /// leaves the profile key grammar.
    #[test]
    fn the_profile_name_is_pinned_by_vectors() {
        let vectors = [
            (
                "kbf-builds",
                "ab12cd/345",
                "d105477ad34feacfa79ab9da7147b115a094f0ad9022e0297c44db1a94b8c681",
            ),
            (
                "Remote/Cache-1",
                "0f/9",
                "b278f3b0204f211ea5ef94fdcd8d65e96b3dbd655fb32ae9e29732ac4f640c59",
            ),
            (
                "x",
                "",
                "41add8152e6530cd1f5030084f5c4fd3a28b735897c8c8801e3f4eec08c62028",
            ),
        ];
        for (class, dedup, hex) in vectors {
            let name = profile_key(&WorkKey::new(class, dedup));
            assert_eq!(name, hex);
            assert_eq!(name.len(), 64);
            assert!(name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        }
        let a = profile_key(&WorkKey::new("ab", "c"));
        let b = profile_key(&WorkKey::new("a", "bc"));
        assert_ne!(a, b);
    }
}
