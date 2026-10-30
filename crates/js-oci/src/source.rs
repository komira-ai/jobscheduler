//! Where an artifact's bytes come from, as far as the code that unpacks it is concerned.

use async_trait::async_trait;

use crate::client::BlobSink;
use crate::manifest::{self, Descriptor, Kind, Manifest};
use crate::{Digest, Error};

/// A place an artifact can be read from, by manifest digest: a registry
/// ([`crate::Repository`]) or an OCI image-layout directory ([`crate::Layout`]).
///
/// Every source applies the same checks, so a caller cannot tell which one served an
/// artifact, and a source cannot be made to accept what the other refuses:
/// - [`BundleSource::fetch_manifest`] returns a [`Manifest`] only if its bytes hash to
///   the pin, it is not an index, and it is of the [`Kind`] asked for (media types,
///   `artifactType`, layer count, sizes and limits);
/// - [`BundleSource::fetch_blob`] finishes the sink only if the blob is exactly as long
///   as its descriptor says and hashes to its digest. The sink receives the bytes while
///   they are read, before that check, so it must treat them as unverified (a private
///   file the caller removes on error) until the call has returned `Ok`;
/// - [`BundleSource::fetch_config`] is a verified blob that also names the platform of
///   the [`Kind`].
#[async_trait]
pub trait BundleSource: Sync {
    /// The manifest pinned by `digest`.
    ///
    /// # Errors
    /// The source cannot be read, or the manifest is not an acceptable manifest of
    /// `kind` with that digest.
    async fn fetch_manifest(&self, digest: &Digest, kind: &Kind) -> Result<Manifest, Error>;

    /// The blob `blob` into `sink`, hashed as it is read. The sink is finished only when
    /// the blob is exactly `blob.size()` bytes and hashes to `blob.digest()`; on any
    /// error the caller discards what the sink received.
    ///
    /// A [`Descriptor`] can only come from a [`Manifest`] that passed its [`Kind`]'s
    /// checks, so the kind's media types and size limits hold for `blob`.
    ///
    /// # Errors
    /// The source cannot be read, the blob is longer or shorter than the manifest says
    /// or does not hash to its digest, or the sink fails.
    async fn fetch_blob(&self, blob: &Descriptor, sink: &mut dyn BlobSink) -> Result<(), Error>;

    /// The config blob of `manifest`, verified, and checked to be for `kind`'s platform.
    ///
    /// # Errors
    /// The blob fails its digest or size, is not JSON, or states another platform.
    async fn fetch_config(&self, manifest: &Manifest, kind: &Kind) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        self.fetch_blob(manifest.config(), &mut bytes).await?;
        manifest::check_platform(kind, &bytes)?;
        Ok(bytes)
    }
}
