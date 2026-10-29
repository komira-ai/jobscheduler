//! Why a pack was refused, and why a blob could not be added to one.

use crate::Digest;

/// Why a pack, its footer or one of its records was refused.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PackError {
    /// Fewer bytes than the trailer or the footer it describes.
    #[error("pack footer truncated: needed {needed} bytes, had {available}")]
    Truncated {
        /// Bytes the trailer or footer needs.
        needed: u64,
        /// Bytes that were supplied.
        available: u64,
    },
    /// The last eight bytes are not the pack magic: not a pack, or cut short.
    #[error("not a pack: wrong magic")]
    BadMagic,
    /// A layout version this build does not read.
    #[error("unsupported pack version {0}")]
    UnsupportedVersion(u32),
    /// A digest function code this build does not read.
    #[error("unsupported digest function code {0}")]
    UnsupportedDigestFunction(u8),
    /// The trailer's reserved bytes are not zero.
    #[error("pack trailer reserved bytes are not zero")]
    ReservedNotZero,
    /// The trailer's record length plus the footer length is not the object's length.
    #[error("pack length is {actual} bytes but its trailer describes {recorded}")]
    LengthMismatch {
        /// The length the trailer implies.
        recorded: u64,
        /// The length of the object.
        actual: u64,
    },
    /// The footer's CRC-32C does not match its bytes.
    #[error("pack footer CRC mismatch: recorded {recorded:#010x}, computed {computed:#010x}")]
    FooterCrc {
        /// The CRC in the trailer.
        recorded: u32,
        /// The CRC of the footer bytes.
        computed: u32,
    },
    /// The index entry at `position` does not sort strictly after the one before it:
    /// the index is unsorted, or holds a duplicate.
    #[error("pack index out of order at entry {position}")]
    IndexOrder {
        /// The entry's position in the index.
        position: usize,
    },
    /// The fan-out count for `byte` disagrees with the index.
    #[error("pack fan-out disagrees with the index at byte {byte:#04x}")]
    FanOut {
        /// The first hash byte whose cumulative count is wrong.
        byte: u8,
    },
    /// A record's range ends beyond the record area.
    #[error("record {digest} lies outside the pack's records")]
    RecordOutOfBounds {
        /// The record's digest.
        digest: Digest,
    },
    /// A record's bytes are not as long as its digest's size.
    #[error("record {digest} has {actual} bytes")]
    RecordLength {
        /// The record's digest.
        digest: Digest,
        /// The length supplied.
        actual: u64,
    },
    /// A record's CRC-32C does not match its index entry: the bytes were damaged.
    #[error("record {digest} fails its CRC-32C")]
    RecordCrc {
        /// The record's digest.
        digest: Digest,
    },
    /// A record's CRC matches but its SHA-256 does not.
    #[error("record {digest} fails its digest check")]
    RecordDigest {
        /// The record's digest.
        digest: Digest,
    },
    /// The pack holds no records. The strict check refuses it: a host with no outputs
    /// reports `NothingToUpload`, not an empty pack.
    #[error("pack holds no records")]
    Empty,
    /// The object is longer than the pack limit.
    #[error("pack is {size} bytes, over the {limit}-byte limit")]
    ObjectTooLarge {
        /// The object's length.
        size: u64,
        /// The limit it was checked against.
        limit: u64,
    },
    /// The index holds more records than the pack limit.
    #[error("pack holds {count} records, over the limit of {limit}")]
    TooManyBlobs {
        /// The number of index entries.
        count: u64,
        /// The limit it was checked against.
        limit: u64,
    },
    /// Records do not start at offset 0 or leave a gap: the record at `position`, in
    /// offset order, does not start where the one before it ended (`position` is the
    /// record count when the last record ends before the footer starts).
    #[error("pack records leave a gap at record {position}")]
    Gap {
        /// The record's position in offset order.
        position: usize,
    },
    /// The record at `position`, in offset order, starts before the one before it ended.
    #[error("pack records overlap at record {position}")]
    Overlap {
        /// The record's position in offset order.
        position: usize,
    },
    /// The footer's entries are not the manifest, entry for entry.
    #[error("pack footer does not equal the reported manifest")]
    ManifestMismatch,
}

/// Why a blob was not added to a pack, or the pack could not be written.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    /// The blob does not fit in what is left of this pack: not enough bytes, the blob
    /// limit is reached, or the blob (or one already in the pack) is large enough to
    /// need a pack of its own. Finish this pack and push the blob into a new one.
    #[error("pack full: {size}-byte blob does not belong in this pack")]
    Full {
        /// The blob's size.
        size: u64,
    },
    /// The blob does not fit even in an empty pack: it is larger than the pack limit
    /// less its footer. Nothing can write it; the caller reports `TooLarge`.
    #[error("{size}-byte blob exceeds the {limit}-byte pack limit")]
    TooLarge {
        /// The blob's size.
        size: u64,
        /// The writer's pack size limit.
        limit: u64,
    },
    /// The sink refused a write. The pack is unusable: discard it.
    #[error("writing the pack failed: {0}")]
    Io(std::io::ErrorKind),
}
