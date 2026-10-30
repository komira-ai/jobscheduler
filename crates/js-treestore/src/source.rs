//! Where layer blobs come from: a narrow, blocking trait.
//!
//! The shape is that of `js-oci`'s `BundleSource::fetch_blob`: a blob named by digest and
//! size goes into a sink the caller supplies, and the source finishes the sink only when it
//! sent exactly the declared bytes. The tree store does not trust that: its sink hashes
//! what it receives and the tree store checks size and digest before any byte is parsed.

use std::fmt;

/// A text that is not a digest, or a run id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {what}: {text:?}")]
pub struct LabelError {
    /// What was being parsed.
    pub what: &'static str,
    /// The text, as given.
    pub text: String,
}

/// A SHA-256 digest: `sha256:` and 64 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(String);

impl Digest {
    /// Parses `sha256:<64 lowercase hex>`.
    ///
    /// # Errors
    /// The text is any other form.
    pub fn parse(text: &str) -> Result<Digest, LabelError> {
        let hex = text.strip_prefix("sha256:").unwrap_or("");
        let ok = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if ok {
            Ok(Digest(text.to_owned()))
        } else {
            Err(LabelError {
                what: "digest",
                text: text.to_owned(),
            })
        }
    }

    /// The digest as text, with its prefix.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A blob to fetch: its digest and exact size, which is also the cap on what is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    /// The blob's digest.
    pub digest: Digest,
    /// Its size in bytes.
    pub size: u64,
}

/// Where fetched bytes go.
pub trait BlobSink {
    /// Takes the next bytes.
    ///
    /// # Errors
    /// The sink refuses (the blob is over its cap, or a write failed); the source stops.
    fn write(&mut self, data: &[u8]) -> std::io::Result<()>;
}

/// A fetch failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FetchError(pub String);

/// A place layer blobs can be fetched from.
pub trait LayerSource: Send + Sync + 'static {
    /// Sends the blob's bytes to `sink` in order. It need not verify them; the tree store
    /// does.
    ///
    /// # Errors
    /// The source cannot be reached or refuses, or the sink fails.
    fn fetch(&self, blob: &BlobRef, sink: &mut dyn BlobSink) -> Result<(), FetchError>;
}
