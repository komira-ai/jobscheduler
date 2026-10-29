//! Writing a pack: records in, then the footer and trailer at the end.

use std::collections::BTreeMap;
use std::io::Write;

use crate::error::WriteError;
use crate::layout::{ENTRY_LEN, IndexEntry, PackLimits, encode_footer, footer_len};
use crate::{Digest, PackEntry, PackReport};

/// Packs blobs into one pack within [`PackLimits`], writing each record to a sink as
/// it is pushed.
///
/// The writer holds the index (52 bytes a record) and never the records, so a pack of
/// the largest size costs a few hundred kilobytes of memory beside the blob being
/// pushed. The sink is the caller's, passed to every call, and may be a file, a
/// buffer or a request body. Each blob is hashed as it is pushed, so a record can never
/// sit under the wrong digest. Pushing a blob the pack already holds is a no-op.
///
/// After an [`WriteError::Io`] the sink holds a partial record: discard the pack.
/// After any other error nothing was written and the writer is unchanged.
#[derive(Debug)]
pub struct PackWriter {
    limits: PackLimits,
    written: u64,
    has_own_pack_blob: bool,
    index: BTreeMap<Digest, IndexEntry>,
}

/// A finished pack: its length and the manifest to report for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    /// The pack's total length: records, then footer.
    pub size: u64,
    /// The index, ascending by (hash, size): what [`crate::verify_pack`] compares the
    /// stored footer with.
    pub manifest: Vec<PackEntry>,
}

impl Finished {
    /// The report for this pack once it is stored under `key`.
    #[must_use]
    pub fn into_report(self, key: String) -> PackReport {
        PackReport {
            key,
            size: self.size,
            manifest: self.manifest,
        }
    }
}

impl PackWriter {
    /// A writer for a pack within `limits`.
    #[must_use]
    pub const fn new(limits: PackLimits) -> Self {
        Self {
            limits,
            written: 0,
            has_own_pack_blob: false,
            index: BTreeMap::new(),
        }
    }

    /// Adds `blob`, writing it to `out`, and returns its digest.
    ///
    /// # Errors
    /// [`WriteError::TooLarge`] if the blob would not fit in an empty pack;
    /// [`WriteError::Full`] if it does not fit in the space left, the pack holds the
    /// most blobs it may, or the blob (or one already here) must have a pack of its own
    /// (finish this pack and start another); [`WriteError::Io`] if `out` fails.
    pub fn push(&mut self, out: &mut dyn Write, blob: &[u8]) -> Result<Digest, WriteError> {
        let digest = Digest::of(blob);
        if self.index.contains_key(&digest) {
            return Ok(digest);
        }
        let size = digest.size_bytes;
        if size.saturating_add(footer_len(1)) > self.limits.max_bytes {
            return Err(WriteError::TooLarge {
                size,
                limit: self.limits.max_bytes,
            });
        }
        let own = size >= self.limits.own_pack_bytes;
        let shares = (own || self.has_own_pack_blob) && !self.index.is_empty();
        let no_bytes = self.len().saturating_add(size + ENTRY_LEN as u64) > self.limits.max_bytes;
        let no_slot = self.index.len() >= self.limits.max_blobs as usize;
        if shares || no_bytes || no_slot {
            return Err(WriteError::Full { size });
        }
        out.write_all(blob).map_err(|e| WriteError::Io(e.kind()))?;
        self.index.insert(
            digest,
            IndexEntry {
                digest,
                offset: self.written,
                crc32c: ::crc32c::crc32c(blob),
            },
        );
        self.written += size;
        self.has_own_pack_blob |= own;
        Ok(digest)
    }

    /// The length the pack would have if finished now.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.written + footer_len(self.index.len() as u64)
    }

    /// Whether no blob has been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// How many blobs have been pushed.
    #[must_use]
    pub fn blob_count(&self) -> usize {
        self.index.len()
    }

    /// Writes the footer and trailer to `out`, after the records.
    ///
    /// # Errors
    /// [`WriteError::Io`] if `out` fails.
    pub fn finish(self, out: &mut dyn Write) -> Result<Finished, WriteError> {
        let entries: Vec<IndexEntry> = self.index.into_values().collect();
        let footer = encode_footer(self.written, &entries);
        out.write_all(&footer)
            .map_err(|e| WriteError::Io(e.kind()))?;
        Ok(Finished {
            size: self.written + footer.len() as u64,
            manifest: entries.iter().map(PackEntry::from).collect(),
        })
    }
}
