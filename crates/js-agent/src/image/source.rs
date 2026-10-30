//! The js-oci client as the image source of js-contain's image store.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_oci::{BundleSource as _, Client, Digest, Kind, Location, Platform, media};

use super::config::{CONFIG_LIMIT_BYTES, ImageConfig};

/// Pulls images from the registries the client's policy allows, for the platform of this
/// machine.
///
/// [`ImageSource::fetch`] writes the layers one at a time, in manifest order, as
/// `<n>.tar` or `<n>.tar.gz` (`n` from 0, the base layer first, the names the unpack mode
/// takes), each hashed while it arrives and kept only when it is exactly the bytes the
/// manifest names. When anything fails, the layers this call wrote are removed, the
/// partial one included. The manifest is checked against the pin, an index is refused as
/// [`FetchError::Index`], and the config's platform and layer count are checked against
/// the manifest; the config is not otherwise used here (see [`ImageConfig`]).
pub struct RegistryImages {
    client: Client,
    kind: Kind,
}

impl std::fmt::Debug for RegistryImages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryImages")
            .field("platform", &self.kind.platform)
            .finish_non_exhaustive()
    }
}

impl RegistryImages {
    /// An image source over `client` for the Linux platform of the Rust architecture
    /// `arch` (`std::env::consts::ARCH`: `x86_64` is `amd64`, `aarch64` is `arm64`).
    ///
    /// # Errors
    /// [`js_oci::Error::Arch`] when `arch` has no image platform name.
    pub fn new(client: Client, arch: &str) -> Result<Self, js_oci::Error> {
        let mut kind = Kind::image(Platform::linux(arch)?);
        kind.limits.config = CONFIG_LIMIT_BYTES;
        Ok(Self { client, kind })
    }

    /// The layers of `image` into `blobs`, appending the path of each file it created to
    /// `written` (so the caller can remove them); returns their names.
    async fn pull(
        &self,
        image: &PinnedImage,
        blobs: &Path,
        written: &mut Vec<PathBuf>,
    ) -> Result<Vec<String>, FetchError> {
        let (repository, pinned) = image
            .reference
            .split_once('@')
            .filter(|(_, pinned)| *pinned == image.digest)
            .ok_or_else(|| {
                failed(format!(
                    "{} is not <repository>@{}",
                    image.reference, image.digest
                ))
            })?;
        let digest = Digest::parse(pinned).map_err(failed)?;
        let location = Location::parse(repository).map_err(failed)?;
        let source = self.client.repository(location).map_err(failed)?;
        let manifest = source
            .fetch_manifest(&digest, &self.kind)
            .await
            .map_err(|e| match e {
                js_oci::Error::Index(_) => FetchError::Index,
                other => failed(other),
            })?;
        let config = source
            .fetch_config(&manifest, &self.kind)
            .await
            .map_err(failed)?;
        ImageConfig::parse(&config)
            .and_then(|config| config.check_layers(manifest.layers().len()))
            .map_err(failed)?;
        let mut names = Vec::new();
        for (n, layer) in manifest.layers().iter().enumerate() {
            let extension = if layer.media_type() == media::LAYER_TAR {
                "tar"
            } else {
                "tar.gz"
            };
            let name = format!("{n}.{extension}");
            let path = blobs.join(&name);
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                // Unverified until `fetch_blob` returns; the image store opens it after.
                .mode(0o600)
                .open(&path)
                .await
                .map_err(failed)?;
            written.push(path);
            source.fetch_blob(layer, &mut file).await.map_err(failed)?;
            names.push(name);
        }
        Ok(names)
    }
}

/// A failure in words for an operator.
fn failed(why: impl std::fmt::Display) -> FetchError {
    FetchError::Failed(why.to_string())
}

#[async_trait]
impl ImageSource for RegistryImages {
    async fn fetch(&self, image: &PinnedImage, blobs: &Path) -> Result<Vec<String>, FetchError> {
        let mut written = Vec::new();
        let pulled = self.pull(image, blobs, &mut written).await;
        if pulled.is_err() {
            for path in written {
                // A layer that was never created is not in `written`; one that is gone
                // already needs no removal.
                let _ = tokio::fs::remove_file(path).await;
            }
        }
        pulled
    }
}
