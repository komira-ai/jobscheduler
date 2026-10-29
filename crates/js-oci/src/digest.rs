//! The one digest algorithm artifacts are pinned by.

use std::fmt;

use sha2::{Digest as _, Sha256};

use crate::Error;

const PREFIX: &str = "sha256:";
const HEX_LEN: usize = 64;

/// `sha256:` and 64 lowercase hex digits, so a value is safe as a path component and a
/// URL part.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(String);

impl Digest {
    /// The digest `text` spells.
    ///
    /// # Errors
    /// [`Error::Digest`] when `text` is not `sha256:` and 64 lowercase hex digits.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let hex = text.strip_prefix(PREFIX).unwrap_or_default();
        let lower_hex = hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        let ok = [hex.len() == HEX_LEN, lower_hex];
        if ok.into_iter().all(|ok| ok) {
            Ok(Self(text.to_owned()))
        } else {
            Err(Error::Digest(text.to_owned()))
        }
    }

    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self::finish(Sha256::digest(bytes))
    }

    /// The digest `hash` produced.
    pub(crate) fn finish(hash: impl AsRef<[u8]>) -> Self {
        Self(format!("{PREFIX}{}", hex::encode(hash)))
    }

    /// The digest as `sha256:<hex>`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// Catches a digest accepted that could break out of a path or URL, or a good one
    /// refused: only the exact `sha256:` form with 64 lowercase hex digits is a digest.
    #[test]
    fn only_sha256_with_64_lowercase_hex_digits_is_a_digest() {
        let parsed = Digest::parse(EMPTY).expect("good");
        assert_eq!(parsed.as_str(), EMPTY);
        assert_eq!(parsed.to_string(), EMPTY);
        let upper = EMPTY.to_uppercase().replace("SHA256", "sha256");
        let short = &EMPTY[..EMPTY.len() - 1];
        let long = format!("{EMPTY}0");
        let other = EMPTY.replace("sha256", "sha512");
        let dots = format!("sha256:{}", "../".repeat(22).get(..64).expect("64"));
        for bad in [
            "",
            "sha256:",
            &upper,
            short,
            &long,
            &other,
            &dots,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ] {
            assert!(
                matches!(Digest::parse(bad), Err(Error::Digest(_))),
                "{bad:?} was accepted"
            );
        }
    }

    /// Catches a wrong hash: the digest of nothing is the well-known constant.
    #[test]
    fn the_digest_of_bytes_is_their_sha256() {
        assert_eq!(Digest::of(b"").as_str(), EMPTY);
    }
}
