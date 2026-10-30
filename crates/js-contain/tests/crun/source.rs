//! The image sources of the tests: layers a test made, and a registry's busybox.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_oci::{Allow, BundleSource as _, Client, Digest, Kind, Location, Platform, Policy};

/// The image the layer tests name: it exists only as far as the source serves it.
pub fn pinned() -> PinnedImage {
    let digest = format!("sha256:{}", "a".repeat(64));
    PinnedImage {
        reference: format!("example.invalid/library/layers@{digest}"),
        digest,
    }
}

/// Layers a test made, served for any image: file names (`0.tar`, `1.tar.gz`) and bytes.
/// What it serves can be replaced between fetches.
#[derive(Debug, Default)]
pub struct Layers(Mutex<Vec<(String, Vec<u8>)>>);

impl Layers {
    /// A source serving `layers`.
    pub fn of(layers: Vec<(String, Vec<u8>)>) -> Self {
        Self(Mutex::new(layers))
    }

    /// Serves `layers` from now on.
    pub fn serve(&self, layers: Vec<(String, Vec<u8>)>) {
        *self.0.lock().expect("layers") = layers;
    }
}

#[async_trait]
impl ImageSource for Layers {
    async fn fetch(&self, _: &PinnedImage, blobs: &Path) -> Result<Vec<String>, FetchError> {
        let layers = self.0.lock().expect("layers").clone();
        let mut names = Vec::new();
        for (name, bytes) in layers {
            std::fs::write(blobs.join(&name), bytes)
                .map_err(|e| FetchError::Failed(e.to_string()))?;
            names.push(name);
        }
        Ok(names)
    }
}

/// The busybox 1.37.0 manifest of this machine's architecture, pinned by digest (the
/// same pins as `cargo xtask crun-tests`, which names the one it chose in `JS_TEST_IMAGE`).
pub fn busybox() -> PinnedImage {
    let reference = std::env::var("JS_TEST_IMAGE").unwrap_or_else(|_| {
        panic!("JS_TEST_IMAGE is not set: run these tests through cargo xtask crun-tests")
    });
    let reference = reference.strip_prefix("docker://").unwrap_or(&reference);
    let digest = reference
        .split_once('@')
        .unwrap_or_else(|| panic!("JS_TEST_IMAGE {reference} names no digest"))
        .1
        .to_owned();
    PinnedImage {
        reference: reference.to_owned(),
        digest,
    }
}

/// Docker Hub, through the repository's registry client: every byte checked against its
/// digest.
#[derive(Debug)]
pub struct Registry {
    client: Client,
}

impl Registry {
    /// A client that reaches `docker.io/library/` and its token endpoint.
    pub fn new() -> Self {
        let policy = Policy::new(vec![Allow::parse("docker.io/library/").expect("an entry")])
            .allow_realm("auth.docker.io")
            .expect("a realm");
        Self {
            client: Client::new(policy),
        }
    }

    async fn pull(&self, image: &PinnedImage, blobs: &Path) -> Result<Vec<String>, js_oci::Error> {
        let name = image.reference.split_once('@').map_or("", |(name, _)| name);
        let name = name.strip_prefix("docker://").unwrap_or(name);
        let repository = self.client.repository(Location::parse(name)?)?;
        let kind = Kind::image(Platform::linux(std::env::consts::ARCH)?);
        let manifest = repository
            .fetch_manifest(&Digest::parse(&image.digest)?, &kind)
            .await?;
        let mut names = Vec::new();
        for (i, layer) in manifest.layers().iter().enumerate() {
            let zipped = layer.media_type().ends_with("gzip");
            let file = format!("{i}.tar{}", if zipped { ".gz" } else { "" });
            let created = tokio::fs::File::create(blobs.join(&file)).await;
            let mut sink = created.map_err(js_oci::Error::Sink)?;
            repository.fetch_blob(layer, &mut sink).await?;
            names.push(file);
        }
        Ok(names)
    }
}

#[async_trait]
impl ImageSource for Registry {
    async fn fetch(&self, image: &PinnedImage, blobs: &Path) -> Result<Vec<String>, FetchError> {
        self.pull(image, blobs)
            .await
            .map_err(|e| FetchError::Failed(e.to_string()))
    }
}
