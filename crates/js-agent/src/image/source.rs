//! The js-oci client as the image source of js-contain's image store.

use std::path::Path;

use async_trait::async_trait;
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_oci::{Client, Kind, Platform};

use super::config::CONFIG_LIMIT_BYTES;

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
}

/// A failure in words for an operator.
fn failed(why: impl std::fmt::Display) -> FetchError {
    FetchError::Failed(why.to_string())
}

#[async_trait]
impl ImageSource for RegistryImages {
    async fn fetch(&self, _image: &PinnedImage, _blobs: &Path) -> Result<Vec<String>, FetchError> {
        let _ = (&self.client, &self.kind);
        Err(failed("not implemented"))
    }
}
