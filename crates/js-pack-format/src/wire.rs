//! The upload grant a frontend gives a host, and the report the host sends back.
//!
//! These are serde types, carried as JSON like the rest of the agent wire. A digest is
//! 64 lowercase hex digits on the wire. Unknown fields are ignored on decode, so a field
//! added later is additive.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::layout::{IndexEntry, MAX_BLOBS_PER_PACK, MAX_PACK_BYTES, OWN_PACK_BLOB_BYTES};
use crate::{PackLimits, digest};

/// Where a host may PUT one pack, and what a pack may hold. Minted by the frontend at
/// lease grant.
///
/// **This is a secret.** Every `put_url` is a presigned PUT: whoever holds it can write
/// that key until it expires. Never log it, never put it in an error, an argument, an
/// environment variable or a job mount, and never show it to the job. `Debug` prints
/// `<upload grant: N slots>`; the type has no `Display`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadGrant {
    /// One presigned PUT per pack the host may upload, used in order.
    pub slots: Vec<UploadSlot>,
    /// The most bytes in a pack, footer included. At most [`MAX_PACK_BYTES`].
    pub max_pack_bytes: u64,
    /// The most blobs in a pack. At most [`MAX_BLOBS_PER_PACK`].
    pub max_blobs_per_pack: u32,
    /// When the URLs stop working, in Unix milliseconds. Advisory on the host: the
    /// store's refusal is the authority.
    pub expires_unix_ms: u64,
}

impl UploadGrant {
    /// The pack limits this grant allows, each capped at the farm's ceiling so a
    /// grant cannot ask for a larger pack than a store accepts.
    #[must_use]
    pub fn limits(&self) -> PackLimits {
        PackLimits {
            max_bytes: self.max_pack_bytes.min(MAX_PACK_BYTES),
            max_blobs: self.max_blobs_per_pack.min(MAX_BLOBS_PER_PACK),
            own_pack_bytes: OWN_PACK_BLOB_BYTES,
        }
    }
}

impl fmt::Debug for UploadGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<upload grant: {} slots>", self.slots.len())
    }
}

/// One key the host may write, and the presigned PUT for exactly that key.
///
/// **`put_url` is a secret**; see [`UploadGrant`]. `Debug` shows the key and hides the
/// URL.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadSlot {
    /// The object key, `cas/b/<build>/<nonce>-<i>`.
    pub key: String,
    /// A presigned PUT for exactly `key`. A secret.
    pub put_url: String,
}

impl fmt::Debug for UploadSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadSlot")
            .field("key", &self.key)
            .field("put_url", &"<redacted>")
            .finish()
    }
}

/// What the host says one uploaded pack holds. A claim: the frontend trusts none of it
/// until [`crate::verify_pack`] and a hash of every byte agree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackReport {
    /// The slot's key the pack was stored under.
    pub key: String,
    /// The pack's length in bytes.
    pub size: u64,
    /// The pack's records, ascending by (digest, size): the footer's index.
    pub manifest: Vec<PackEntry>,
}

/// One record of a pack: where the blob with this digest and size lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackEntry {
    /// The blob's SHA-256, as 64 lowercase hex digits on the wire.
    #[serde(with = "hex32")]
    pub digest: [u8; 32],
    /// The blob's length, which is the record's length.
    pub size: u64,
    /// Where the record starts, from the start of the pack.
    pub offset: u64,
}

impl From<&IndexEntry> for PackEntry {
    fn from(entry: &IndexEntry) -> Self {
        Self {
            digest: entry.digest.hash,
            size: entry.digest.size_bytes,
            offset: entry.offset,
        }
    }
}

/// What the host's upload step ended with. A result with no outcome when the job
/// carried a grant is a failed action, never "no outputs": [`UploadOutcome::NothingToUpload`]
/// is the only way to say there were none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadOutcome {
    /// Every pack was stored; one report per slot used, in slot order.
    Uploaded(Vec<PackReport>),
    /// The job produced no outputs. Nothing was written.
    NothingToUpload,
    /// The outputs need more than the grant allows. Nothing was written, not even a
    /// partial pack.
    TooLarge {
        /// How many packs the outputs need.
        needed_slots: u32,
        /// How many bytes of blobs the outputs hold.
        needed_bytes: u64,
    },
    /// A PUT was refused because the grant had expired. The frontend mints a new one.
    GrantExpired,
    /// A PUT was refused for another reason.
    UploadRefused {
        /// The HTTP status of the refusal.
        status: u16,
    },
}

/// Serde for a 32-byte hash as 64 lowercase hex digits.
mod hex32 {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(hash: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::digest::to_hex(hash))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(deserializer)?;
        super::digest::from_hex(&text)
            .ok_or_else(|| D::Error::custom("expected 64 lowercase hex digits"))
    }
}
