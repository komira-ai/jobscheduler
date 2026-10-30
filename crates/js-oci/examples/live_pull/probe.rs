//! The steps of the live pull probe, shared by the `live_pull` example and its offline
//! test (`tests/live_pull.rs`), so the logic the test proves is the logic that runs
//! against a real registry.

use std::time::Instant;

use async_trait::async_trait;
use js_oci::{BlobSink, BundleSource, Client, Digest, Error, Kind, Location};
use sha2::{Digest as _, Sha256};

/// Why a probe stopped.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// js-oci refused or could not fetch something; its text says what.
    #[error("{0}")]
    Pull(#[from] Error),
    /// The smallest layer is larger than the probe was told to download.
    #[error("the smallest layer is {size} bytes, over --max-layer-bytes {max}")]
    LayerTooLarge {
        /// The layer's declared size.
        size: u64,
        /// The limit given.
        max: u64,
    },
}

/// A sink that only hashes and counts: nothing is written anywhere.
#[derive(Default)]
pub struct Hashing {
    hasher: Sha256,
    bytes: u64,
}

impl Hashing {
    fn sha256(&self) -> String {
        format!("sha256:{}", hex::encode(self.hasher.clone().finalize()))
    }
}

#[async_trait]
impl BlobSink for Hashing {
    async fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.hasher.update(chunk);
        self.bytes += chunk.len() as u64;
        Ok(())
    }

    async fn finish(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Pulls the manifest `pin`, its config and its smallest layer through `client`, and
/// returns one `key=value` line per step. The digests are verified by js-oci before each
/// step returns; the layer is also hashed independently by the sink, and both are
/// printed. Elapsed times are reported, never judged.
///
/// # Errors
/// Any js-oci error, or a smallest layer over `max_layer_bytes`.
pub async fn probe(
    client: &Client,
    location: Location,
    pin: &Digest,
    kind: &Kind,
    max_layer_bytes: u64,
) -> Result<Vec<String>, ProbeError> {
    let mut lines = vec![format!(
        "target registry={} repository={} auth=anonymous",
        location.registry(),
        location.repository()
    )];
    let repo = client.repository(location)?;

    let started = Instant::now();
    let manifest = repo.fetch_manifest(pin, kind).await?;
    lines.push(format!(
        "manifest digest={} bytes={} layers={} verified=yes token=anonymous-ok ms={}",
        manifest.digest(),
        manifest.bytes().len(),
        manifest.layers().len(),
        started.elapsed().as_millis()
    ));

    let started = Instant::now();
    let config = repo.fetch_config(&manifest, kind).await?;
    lines.push(format!(
        "config digest={} bytes={} verified=yes ms={}",
        manifest.config().digest(),
        config.len(),
        started.elapsed().as_millis()
    ));

    // A manifest js-oci accepted has at least one layer.
    let layer = manifest
        .layers()
        .iter()
        .min_by_key(|l| l.size())
        .expect("an accepted manifest names a layer");
    if layer.size() > max_layer_bytes {
        return Err(ProbeError::LayerTooLarge {
            size: layer.size(),
            max: max_layer_bytes,
        });
    }
    let started = Instant::now();
    let mut sink = Hashing::default();
    repo.fetch_blob(layer, &mut sink).await?;
    lines.push(format!(
        "layer digest={} media={} bytes={} sink_sha256={} verified=yes ms={}",
        layer.digest(),
        layer.media_type(),
        sink.bytes,
        sink.sha256(),
        started.elapsed().as_millis()
    ));
    Ok(lines)
}
