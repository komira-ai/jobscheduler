//! Reading a pack, and the strict check of a pack that came from an untrusted host.

use crate::error::PackError;
use crate::layout::{Footer, IndexEntry, PackLimits};
use crate::{Digest, PackEntry};

/// Checks the footer of a stored pack against what the host reported, as the frontend
/// does before it adopts the pack.
///
/// `tail` is the last bytes of the stored object (at least the footer; the whole object
/// is fine) and `object_len` is the object's length **as the store reports it**, never
/// the host's claim. `manifest` is the host's report. In order, this refuses:
///
/// - an object longer than `limits.max_bytes`;
/// - a bad trailer: wrong magic, version, digest function or nonzero reserved bytes;
/// - a footer cut short, or `index_offset` plus the footer length not equal to
///   `object_len`;
/// - a `footer_crc32c` that does not match the fan-out, the index and the trailer prefix;
/// - an index that is not strictly ascending by (hash, size), which also refuses a
///   duplicate, or whose fan-out disagrees with it;
/// - a record that ends past `index_offset`;
/// - more than `limits.max_blobs` records, or none;
/// - records that are not contiguous from offset 0 (a gap, or an overlap), or that do
///   not end exactly at `index_offset`;
/// - a footer that is not the manifest, entry for entry, in index order.
///
/// It reads no record: the caller hashes every byte against the returned footer's
/// entries ([`IndexEntry::verify`]) as it reads them.
///
/// # Errors
/// The first check that fails, as a [`PackError`].
pub fn verify_pack(
    tail: &[u8],
    object_len: u64,
    manifest: &[PackEntry],
    limits: &PackLimits,
) -> Result<Footer, PackError> {
    if object_len > limits.max_bytes {
        return Err(PackError::ObjectTooLarge {
            size: object_len,
            limit: limits.max_bytes,
        });
    }
    let footer = Footer::parse(tail, object_len)?;
    let count = footer.entries().len() as u64;
    if count > u64::from(limits.max_blobs) {
        return Err(PackError::TooManyBlobs {
            count,
            limit: u64::from(limits.max_blobs),
        });
    }
    if count == 0 {
        return Err(PackError::Empty);
    }
    // `Footer::parse` proved every record ends within the record area, so the sums
    // below do not overflow. Ties on offset (an empty record beside the next one)
    // order by size, so the empty record comes first.
    let mut by_offset: Vec<&IndexEntry> = footer.entries().iter().collect();
    by_offset.sort_by_key(|e| (e.offset, e.digest.size_bytes));
    let mut next = 0;
    for (position, entry) in by_offset.iter().enumerate() {
        if entry.offset < next {
            return Err(PackError::Overlap { position });
        }
        if entry.offset > next {
            return Err(PackError::Gap { position });
        }
        next = entry.offset + entry.digest.size_bytes;
    }
    if next != footer.records_len() {
        return Err(PackError::Gap {
            position: by_offset.len(),
        });
    }
    if footer.manifest() != manifest {
        return Err(PackError::ManifestMismatch);
    }
    Ok(footer)
}

/// A whole pack held in memory, with its footer checked.
#[derive(Debug)]
pub struct PackReader<'a> {
    bytes: &'a [u8],
    footer: Footer,
}

impl<'a> PackReader<'a> {
    /// Checks the footer of `bytes`, a complete pack.
    ///
    /// # Errors
    /// Any refusal of [`Footer::parse`].
    pub fn open(bytes: &'a [u8]) -> Result<Self, PackError> {
        let footer = Footer::parse(bytes, bytes.len() as u64)?;
        Ok(Self { bytes, footer })
    }

    /// The blob for `digest`, checked against its CRC-32C and digest, or `None` if the
    /// pack does not hold it.
    ///
    /// # Errors
    /// [`PackError::RecordCrc`] or [`PackError::RecordDigest`] if the record's bytes
    /// are damaged.
    pub fn get(&self, digest: &Digest) -> Result<Option<&'a [u8]>, PackError> {
        let Some(entry) = self.footer.find(digest) else {
            return Ok(None);
        };
        // The footer checked that every range lies inside the records, which lie inside
        // `bytes`, so these casts and the slice are in bounds.
        let range = entry.range();
        let record = &self.bytes[range.start as usize..range.end as usize];
        entry.verify(record)?;
        Ok(Some(record))
    }

    /// The pack's checked footer.
    #[must_use]
    pub const fn footer(&self) -> &Footer {
        &self.footer
    }
}
