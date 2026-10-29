//! A name pinned by content: `name[:tag]@sha256:<hex>`, the one parser behind
//! [`crate::ImageDigestRef`] and [`BundleRef`].
//!
//! A digest names exactly one manifest, so a value of these types cannot name a tag that
//! moves. A digest does not say whether the manifest it names is an image manifest or an
//! index; telling them apart needs the manifest, so the registry client refuses an index
//! where it fetches, not here.

/// The only digest algorithm a pinned reference may name.
const DIGEST_PREFIX: &str = "sha256:";
/// Hex digits in a sha256 digest.
const DIGEST_HEX_LEN: usize = 64;

/// Why text is not a reference with a digest. [`crate::ImageRefError`] and
/// [`BundleRefError`] carry the same three cases under their own names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DigestRefError {
    /// There is no `@` and so no digest.
    #[error("a reference needs a digest: name@sha256:<64 hex digits>")]
    NoDigest,
    /// The digest is not `sha256:` followed by 64 lower case hex digits.
    #[error("the digest must be sha256: followed by 64 lower case hex digits")]
    BadDigest,
    /// The name is empty or holds a character that is not printable ASCII without spaces.
    #[error("the name must be non-empty printable ASCII without spaces")]
    BadName,
}

/// A reference that pins its content: `name[:tag]@sha256:<hex>`. The full grammar of the
/// name (registry, path, tag) is checked where a run is started, not here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DigestRef {
    name: String,
    digest: String,
}

impl DigestRef {
    /// The reference `text`.
    ///
    /// # Errors
    ///
    /// [`DigestRefError`] naming what is missing or malformed.
    pub fn parse(text: &str) -> Result<Self, DigestRefError> {
        let (name, digest) = text.split_once('@').ok_or(DigestRefError::NoDigest)?;
        let hex = digest.strip_prefix(DIGEST_PREFIX).unwrap_or_default();
        let digest_ok = [
            (DIGEST_HEX_LEN - 1..=DIGEST_HEX_LEN).contains(&hex.len()),
            hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        ];
        if !digest_ok.into_iter().all(|ok| ok) {
            return Err(DigestRefError::BadDigest);
        }
        let name_ok = [!name.is_empty(), name.bytes().all(|b| b.is_ascii_graphic())];
        if !name_ok.into_iter().all(|ok| ok) {
            return Err(DigestRefError::BadName);
        }
        Ok(Self {
            name: name.to_owned(),
            digest: digest.to_owned(),
        })
    }

    /// The name, with its tag if it has one. The tag is kept and not interpreted.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The digest, as `sha256:<hex>`.
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// Why text is not a bundle reference with a digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BundleRefError {
    /// There is no `@` and so no digest.
    #[error("a bundle reference needs a digest: name@sha256:<64 hex digits>")]
    NoDigest,
    /// The digest is not `sha256:` followed by 64 lower case hex digits.
    #[error("the digest must be sha256: followed by 64 lower case hex digits")]
    BadDigest,
    /// The name before the `@` is empty or holds a character that is not printable ASCII
    /// without spaces.
    #[error("the bundle name must be non-empty printable ASCII without spaces")]
    BadName,
}

impl From<DigestRefError> for BundleRefError {
    fn from(fault: DigestRefError) -> Self {
        match fault {
            DigestRefError::NoDigest => Self::NoDigest,
            DigestRefError::BadDigest => Self::BadDigest,
            DigestRefError::BadName => Self::BadName,
        }
    }
}

/// A bundle a job pins: `name[:tag]@sha256:<manifest hex>`, the digest of one darwin
/// bundle manifest.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BundleRef(DigestRef);

impl BundleRef {
    /// The reference `text`.
    ///
    /// # Errors
    ///
    /// [`BundleRefError`] naming what is missing or malformed.
    pub fn parse(text: &str) -> Result<Self, BundleRefError> {
        Ok(Self(DigestRef::parse(text)?))
    }

    /// The name, with its tag if it has one.
    pub fn name(&self) -> &str {
        self.0.name()
    }

    /// The digest, as `sha256:<hex>`.
    pub fn digest(&self) -> &str {
        self.0.digest()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ImageDigestRef, ImageRefError};

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Verdict {
        Pinned,
        NoDigest,
        BadDigest,
        BadName,
    }

    fn core(text: &str) -> Verdict {
        match DigestRef::parse(text) {
            Ok(_) => Verdict::Pinned,
            Err(DigestRefError::NoDigest) => Verdict::NoDigest,
            Err(DigestRefError::BadDigest) => Verdict::BadDigest,
            Err(DigestRefError::BadName) => Verdict::BadName,
        }
    }

    fn image(text: &str) -> Verdict {
        match ImageDigestRef::parse(text) {
            Ok(_) => Verdict::Pinned,
            Err(ImageRefError::NoDigest) => Verdict::NoDigest,
            Err(ImageRefError::BadDigest) => Verdict::BadDigest,
            Err(ImageRefError::BadName) => Verdict::BadName,
        }
    }

    fn bundle(text: &str) -> Verdict {
        match BundleRef::parse(text) {
            Ok(_) => Verdict::Pinned,
            Err(BundleRefError::NoDigest) => Verdict::NoDigest,
            Err(BundleRefError::BadDigest) => Verdict::BadDigest,
            Err(BundleRefError::BadName) => Verdict::BadName,
        }
    }

    /// One table of pins, run through the core, the image type and the bundle type. It
    /// catches a rule changed for one type and not the other: the three must give the same
    /// verdict on every row.
    #[test]
    fn images_and_bundles_accept_and_refuse_the_same_pins() {
        let short = format!("t@sha256:{}", &HEX[..63]);
        let long = format!("t@sha256:{HEX}0");
        let upper = format!("t@sha256:{}", HEX.to_uppercase());
        let sha512 = format!("t@sha512:{HEX}{HEX}");
        let algorithm_case = format!("t@SHA256:{HEX}");
        let no_algorithm = format!("t@{HEX}");
        let trailing_newline = format!("t@sha256:{HEX}\n");
        let leading_space = format!("t@ sha256:{HEX}");
        let second_at = format!("t@x@sha256:{HEX}");
        let table: [(String, Verdict); 24] = [
            (format!("toolchain@sha256:{HEX}"), Verdict::Pinned),
            (format!("toolchain:1.2@sha256:{HEX}"), Verdict::Pinned),
            (
                format!("ghcr.io/o/p/toolchain:v1@sha256:{HEX}"),
                Verdict::Pinned,
            ),
            (
                format!("localhost:5000/x:1@sha256:{}", "f".repeat(64)),
                Verdict::Pinned,
            ),
            ("toolchain".into(), Verdict::NoDigest),
            ("toolchain:latest".into(), Verdict::NoDigest),
            (String::new(), Verdict::NoDigest),
            ("toolchain@".into(), Verdict::BadDigest),
            ("toolchain@sha256:".into(), Verdict::BadDigest),
            ("toolchain@sha256:abcd".into(), Verdict::BadDigest),
            (short, Verdict::BadDigest),
            (long, Verdict::BadDigest),
            (upper, Verdict::BadDigest),
            (sha512, Verdict::BadDigest),
            (algorithm_case, Verdict::BadDigest),
            (no_algorithm, Verdict::BadDigest),
            (trailing_newline, Verdict::BadDigest),
            (leading_space, Verdict::BadDigest),
            (second_at, Verdict::BadDigest),
            (format!("@sha256:{HEX}"), Verdict::BadName),
            (format!("two words@sha256:{HEX}"), Verdict::BadName),
            (format!("tab\t@sha256:{HEX}"), Verdict::BadName),
            (format!("caf\u{e9}@sha256:{HEX}"), Verdict::BadName),
            (format!("nul\0@sha256:{HEX}"), Verdict::BadName),
        ];
        for (text, want) in &table {
            assert_eq!(core(text), *want, "core: {text:?}");
            assert_eq!(image(text), *want, "image: {text:?}");
            assert_eq!(bundle(text), *want, "bundle: {text:?}");
        }
    }

    /// A bundle keeps the name (tag included) and the digest apart and does not read the
    /// tag, so `name:tag@sha256:...` and `name@sha256:...` pin the same digest.
    #[test]
    fn a_bundle_keeps_its_tag_and_its_digest_apart() {
        let tagged = BundleRef::parse(&format!("reg.example/b/tool:1.2@sha256:{HEX}")).unwrap();
        assert_eq!(tagged.name(), "reg.example/b/tool:1.2");
        assert_eq!(tagged.digest(), format!("sha256:{HEX}"));
        let bare = BundleRef::parse(&format!("reg.example/b/tool@sha256:{HEX}")).unwrap();
        assert_eq!(bare.digest(), tagged.digest());
        assert_ne!(bare, tagged);
    }

    /// Each refusal says what to fix, in words of its own type.
    #[test]
    fn a_bundle_refusal_names_the_bundle() {
        assert!(BundleRefError::NoDigest.to_string().contains("bundle"));
        assert!(BundleRefError::BadName.to_string().contains("bundle"));
        assert!(BundleRefError::BadDigest.to_string().contains("sha256"));
        assert!(DigestRefError::NoDigest.to_string().contains("digest"));
        assert!(DigestRefError::BadName.to_string().contains("name"));
        assert!(DigestRefError::BadDigest.to_string().contains("sha256"));
    }
}
