//! The check every blob passes, wherever its bytes come from: the length and the digest
//! the manifest declares, applied while the bytes arrive.

use sha2::{Digest as _, Sha256};

use crate::client::BlobSink;
use crate::manifest::Descriptor;
use crate::{Digest, Error};

/// A blob being received. The sink gets each chunk as it comes, so it holds unverified
/// bytes until [`Receiving::finish`] has returned `Ok`: only then has the sink been
/// told the blob is whole. A chunk that takes the blob past its declared length is not
/// passed on.
pub(crate) struct Receiving<'b> {
    blob: &'b Descriptor,
    hasher: Sha256,
    got: u64,
}

impl<'b> Receiving<'b> {
    pub(crate) fn new(blob: &'b Descriptor) -> Self {
        Self {
            blob,
            hasher: Sha256::new(),
            got: 0,
        }
    }

    fn size_error(&self) -> Error {
        Error::BlobSize {
            digest: self.blob.digest().to_string(),
            declared: self.blob.size(),
            got: self.got,
        }
    }

    /// Counts, hashes and stores `data`.
    ///
    /// # Errors
    /// The blob is longer than declared, or the sink fails.
    pub(crate) async fn accept(
        &mut self,
        data: &[u8],
        sink: &mut dyn BlobSink,
    ) -> Result<(), Error> {
        self.got += data.len() as u64;
        if self.got > self.blob.size() {
            return Err(self.size_error());
        }
        self.hasher.update(data);
        sink.write(data).await.map_err(Error::Sink)
    }

    /// Compares the length and the digest with the declared ones and, if both agree,
    /// finishes `sink`.
    ///
    /// # Errors
    /// The blob is shorter than declared or hashes to another digest, or the sink fails.
    pub(crate) async fn finish(self, sink: &mut dyn BlobSink) -> Result<(), Error> {
        if self.got != self.blob.size() {
            return Err(self.size_error());
        }
        let actual = Digest::finish(self.hasher.finalize());
        if actual != *self.blob.digest() {
            return Err(Error::BlobDigest {
                digest: self.blob.digest().to_string(),
                actual: actual.to_string(),
            });
        }
        sink.finish().await.map_err(Error::Sink)
    }
}
