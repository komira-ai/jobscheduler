//! The on-disk layout of a pack, its limits, and the parser for its footer.
//!
//! ```text
//! records   blob bytes, back to back, in the order they were pushed, from offset 0
//! fan-out   256 x u32: fanout[b] = number of index entries whose hash[0] <= b
//! index     N x 52 bytes, strictly ascending by (hash, size):
//!             hash [32] | size u64 | offset u64 | crc32c u32
//! trailer   32 bytes:
//!             index_offset u64 | entry_count u32 | digest_function u8 | reserved [3] = 0
//!             | footer_crc32c u32 | version u32 | magic [8]
//! ```
//!
//! All integers are little-endian. A record is the blob's bytes, uncompressed, so a
//! record's length is its digest's size. `index_offset` is where the fan-out starts,
//! which is also the total length of the records. `footer_crc32c` covers the fan-out,
//! the index and the first 16 bytes of the trailer; the version and magic are checked
//! by value. The footer (fan-out, index, trailer) is everything after the records, so a
//! reader that knows the object's length fetches the trailer, then the footer, then
//! exactly the records it needs.

use crate::error::PackError;
use crate::{Digest, PackEntry};

/// The last eight bytes of every pack.
pub const MAGIC: [u8; 8] = *b"KBFSEG\r\n";
/// The layout version this crate writes and reads.
pub const VERSION: u32 = 1;
/// The length of the fixed trailer at the end of every pack.
pub const TRAILER_LEN: usize = 32;
/// The length of one index entry.
pub const ENTRY_LEN: usize = 52;
/// The length of the fan-out table.
pub const FANOUT_LEN: usize = 256 * 4;
/// The trailer's code for SHA-256. Code 0 is never valid, so a zeroed trailer is refused.
pub const DIGEST_SHA256: u8 = 1;

/// The largest pack, footer included: 128 MiB.
pub const MAX_PACK_BYTES: u64 = 128 << 20;
/// The most records one pack holds.
pub const MAX_BLOBS_PER_PACK: u32 = 16_384;
/// A blob of this many bytes or more is written to a pack of its own: 16 MiB.
pub const OWN_PACK_BLOB_BYTES: u64 = 16 << 20;

/// What a pack may hold. The default is the farm's limits; a grant can lower them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackLimits {
    /// The most bytes in a pack, footer included.
    pub max_bytes: u64,
    /// The most records in a pack.
    pub max_blobs: u32,
    /// A blob of at least this many bytes shares its pack with no other blob. Use
    /// `u64::MAX` to turn the rule off.
    pub own_pack_bytes: u64,
}

impl Default for PackLimits {
    fn default() -> Self {
        Self {
            max_bytes: MAX_PACK_BYTES,
            max_blobs: MAX_BLOBS_PER_PACK,
            own_pack_bytes: OWN_PACK_BLOB_BYTES,
        }
    }
}

/// The footer length (fan-out, index and trailer) of a pack with `entries` records.
#[must_use]
pub const fn footer_len(entries: u64) -> u64 {
    (FANOUT_LEN + TRAILER_LEN) as u64 + entries * ENTRY_LEN as u64
}

/// One record as the index describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// The blob's digest; its size is the record's length.
    pub digest: Digest,
    /// Where the record starts, from the start of the pack.
    pub offset: u64,
    /// The CRC-32C of the record's bytes.
    pub crc32c: u32,
}

impl IndexEntry {
    /// Checks that `record` is this entry's blob: its length, then its CRC-32C (cheap,
    /// catches storage damage), then its digest (catches anything the CRC cannot).
    ///
    /// # Errors
    /// [`PackError::RecordLength`], [`PackError::RecordCrc`] or
    /// [`PackError::RecordDigest`], naming the digest.
    pub fn verify(&self, record: &[u8]) -> Result<(), PackError> {
        if record.len() as u64 != self.digest.size_bytes {
            return Err(PackError::RecordLength {
                digest: self.digest,
                actual: record.len() as u64,
            });
        }
        if ::crc32c::crc32c(record) != self.crc32c {
            return Err(PackError::RecordCrc {
                digest: self.digest,
            });
        }
        if Digest::of(record) != self.digest {
            return Err(PackError::RecordDigest {
                digest: self.digest,
            });
        }
        Ok(())
    }

    /// The record's byte range in the pack.
    #[must_use]
    pub const fn range(&self) -> std::ops::Range<u64> {
        self.offset..self.offset + self.digest.size_bytes
    }
}

/// A parsed, checked pack footer: the index of every record in one pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Footer {
    records_len: u64,
    fanout: [u32; 256],
    entries: Vec<IndexEntry>,
}

impl Footer {
    /// How many bytes from the end of the pack hold the footer, read from the trailer
    /// alone. A range reader fetches the last [`TRAILER_LEN`] bytes, calls this, then
    /// fetches that many bytes for [`Footer::parse`].
    ///
    /// # Errors
    /// A short trailer, a wrong magic, an unsupported version or digest function, or
    /// nonzero reserved bytes.
    pub fn len_from_trailer(trailer: &[u8]) -> Result<u64, PackError> {
        let t = Trailer::parse(trailer)?;
        Ok(footer_len(u64::from(t.entry_count)))
    }

    /// Parses and checks the footer at the end of `tail`, the last bytes of a pack
    /// whose total length is `pack_len`. `tail` may be the whole pack.
    ///
    /// Refuses a footer that is truncated, whose CRC does not match, whose index is
    /// not strictly sorted, whose fan-out disagrees with its index, or whose records
    /// lie outside the record area. It does not read the records.
    ///
    /// # Errors
    /// The first check that fails, as a [`PackError`].
    pub fn parse(tail: &[u8], pack_len: u64) -> Result<Self, PackError> {
        let t = Trailer::parse(tail)?;
        let len = footer_len(u64::from(t.entry_count));
        if (tail.len() as u64) < len {
            return Err(PackError::Truncated {
                needed: len,
                available: tail.len() as u64,
            });
        }
        let recorded = t.index_offset.checked_add(len);
        if recorded != Some(pack_len) {
            return Err(PackError::LengthMismatch {
                recorded: t.index_offset.saturating_add(len),
                actual: pack_len,
            });
        }
        // `len <= tail.len()`, so it fits in usize.
        let footer = &tail[tail.len() - len as usize..];
        let crc_end = footer.len() - TRAILER_LEN + 16;
        let computed = ::crc32c::crc32c(&footer[..crc_end]);
        if computed != t.footer_crc32c {
            return Err(PackError::FooterCrc {
                recorded: t.footer_crc32c,
                computed,
            });
        }

        let mut fanout = [0u32; 256];
        for (slot, bytes) in fanout.iter_mut().zip(footer[..FANOUT_LEN].chunks_exact(4)) {
            *slot = u32::from_le_bytes(bytes.try_into().expect("chunks_exact(4)"));
        }
        let index = &footer[FANOUT_LEN..footer.len() - TRAILER_LEN];
        let mut entries = Vec::with_capacity(t.entry_count as usize);
        let mut counts = [0u32; 256];
        for (position, raw) in index.chunks_exact(ENTRY_LEN).enumerate() {
            let entry = decode_entry(raw);
            if entries
                .last()
                .is_some_and(|prev: &IndexEntry| prev.digest >= entry.digest)
            {
                return Err(PackError::IndexOrder { position });
            }
            if entry
                .offset
                .checked_add(entry.digest.size_bytes)
                .is_none_or(|end| end > t.index_offset)
            {
                return Err(PackError::RecordOutOfBounds {
                    digest: entry.digest,
                });
            }
            counts[usize::from(entry.digest.hash[0])] += 1;
            entries.push(entry);
        }
        let mut cumulative = 0u32;
        for (byte, (&count, &recorded)) in counts.iter().zip(&fanout).enumerate() {
            cumulative += count;
            if cumulative != recorded {
                return Err(PackError::FanOut {
                    byte: u8::try_from(byte).expect("256 slots"),
                });
            }
        }
        Ok(Self {
            records_len: t.index_offset,
            fanout,
            entries,
        })
    }

    /// The index entry for `digest`, if this pack holds it. Uses the fan-out to narrow
    /// the search to blobs sharing the hash's first byte.
    #[must_use]
    pub fn find(&self, digest: &Digest) -> Option<&IndexEntry> {
        let first = usize::from(digest.hash[0]);
        let start = if first == 0 {
            0
        } else {
            self.fanout[first - 1]
        } as usize;
        let end = self.fanout[first] as usize;
        let bucket = &self.entries[start..end];
        bucket
            .binary_search_by(|e| e.digest.cmp(digest))
            .ok()
            .map(|i| &bucket[i])
    }

    /// Every index entry, in digest order.
    #[must_use]
    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    /// The length of the record area, which is also where the footer starts.
    #[must_use]
    pub const fn records_len(&self) -> u64 {
        self.records_len
    }

    /// The entries as a manifest, in index order.
    #[must_use]
    pub fn manifest(&self) -> Vec<PackEntry> {
        self.entries.iter().map(PackEntry::from).collect()
    }
}

/// The footer (fan-out, index, trailer) of a pack whose records fill its first
/// `records_len` bytes, indexing `entries`.
///
/// `entries` must be sorted strictly ascending by digest, each lying within the record
/// area, as [`Footer::parse`] demands; [`Footer::entries`] of a parsed footer is. This
/// is the one builder: [`crate::PackWriter`] gets its footer from it.
#[must_use]
pub fn encode_footer(records_len: u64, entries: &[IndexEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FANOUT_LEN + entries.len() * ENTRY_LEN + TRAILER_LEN);
    let mut fanout = [0u32; 256];
    for e in entries {
        fanout[usize::from(e.digest.hash[0])] += 1;
    }
    let mut cumulative = 0u32;
    for count in fanout {
        cumulative += count;
        out.extend_from_slice(&cumulative.to_le_bytes());
    }
    let entry_count = u32::try_from(entries.len()).expect("a pack holds fewer than 2^32 blobs");
    for e in entries {
        out.extend_from_slice(&e.digest.hash);
        out.extend_from_slice(&e.digest.size_bytes.to_le_bytes());
        out.extend_from_slice(&e.offset.to_le_bytes());
        out.extend_from_slice(&e.crc32c.to_le_bytes());
    }
    out.extend_from_slice(&records_len.to_le_bytes());
    out.extend_from_slice(&entry_count.to_le_bytes());
    out.extend_from_slice(&[DIGEST_SHA256, 0, 0, 0]);
    let crc = ::crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&MAGIC);
    out
}

fn decode_entry(raw: &[u8]) -> IndexEntry {
    let u64_at = |at: usize| u64::from_le_bytes(raw[at..at + 8].try_into().expect("8 bytes"));
    IndexEntry {
        digest: Digest {
            hash: raw[..32].try_into().expect("32 bytes"),
            size_bytes: u64_at(32),
        },
        offset: u64_at(40),
        crc32c: u32::from_le_bytes(raw[48..52].try_into().expect("4 bytes")),
    }
}

struct Trailer {
    index_offset: u64,
    entry_count: u32,
    footer_crc32c: u32,
}

impl Trailer {
    /// Parses the last [`TRAILER_LEN`] bytes of `tail`.
    fn parse(tail: &[u8]) -> Result<Self, PackError> {
        let Some(at) = tail.len().checked_sub(TRAILER_LEN) else {
            return Err(PackError::Truncated {
                needed: TRAILER_LEN as u64,
                available: tail.len() as u64,
            });
        };
        let t: &[u8; TRAILER_LEN] = tail[at..].try_into().expect("TRAILER_LEN bytes");
        let u32_at = |at: usize| u32::from_le_bytes(t[at..at + 4].try_into().expect("4 bytes"));
        if t[24..] != MAGIC {
            return Err(PackError::BadMagic);
        }
        let version = u32_at(20);
        if version != VERSION {
            return Err(PackError::UnsupportedVersion(version));
        }
        if t[12] != DIGEST_SHA256 {
            return Err(PackError::UnsupportedDigestFunction(t[12]));
        }
        if t[13..16] != [0, 0, 0] {
            return Err(PackError::ReservedNotZero);
        }
        Ok(Self {
            index_offset: u64::from_le_bytes(t[..8].try_into().expect("8 bytes")),
            entry_count: u32_at(8),
            footer_crc32c: u32_at(16),
        })
    }
}
