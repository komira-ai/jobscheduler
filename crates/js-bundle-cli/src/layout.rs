//! Writing an OCI image-layout directory: `oci-layout`, `index.json` and
//! `blobs/sha256/<hex>`.
//!
//! Every document is written from a typed value, so the field order is the order of the
//! structs and the same inputs give the same bytes.

use std::collections::BTreeMap;
use std::path::Path;

use js_bundle::media;
use js_oci::Digest;
use serde::Serialize;

use crate::error::{Error, at};

/// A blob to store: its digest and bytes.
pub(crate) struct Blob {
    pub(crate) digest: Digest,
    pub(crate) bytes: Vec<u8>,
}

impl Blob {
    /// A blob of `bytes`, hashing them.
    pub(crate) fn new(bytes: Vec<u8>) -> Blob {
        Blob {
            digest: Digest::of(&bytes),
            bytes,
        }
    }
}

#[derive(Serialize)]
struct Descriptor<'a> {
    #[serde(rename = "mediaType")]
    media_type: &'a str,
    digest: &'a str,
    size: u64,
}

fn describe<'a>(media_type: &'a str, blob: &'a Blob) -> Descriptor<'a> {
    Descriptor {
        media_type,
        digest: blob.digest.as_str(),
        size: blob.bytes.len() as u64,
    }
}

#[derive(Serialize)]
struct Manifest<'a> {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType")]
    media_type: &'a str,
    #[serde(rename = "artifactType")]
    artifact_type: &'a str,
    config: Descriptor<'a>,
    layers: Vec<Descriptor<'a>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    annotations: &'a BTreeMap<String, String>,
}

#[derive(Serialize)]
struct Entry<'a> {
    #[serde(rename = "mediaType")]
    media_type: &'a str,
    digest: &'a str,
    size: u64,
    #[serde(rename = "artifactType")]
    artifact_type: &'a str,
}

#[derive(Serialize)]
struct Index<'a> {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType")]
    media_type: &'a str,
    manifests: Vec<Entry<'a>>,
}

const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const MARKER: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;

fn put(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    std::fs::write(path, bytes).map_err(at(path))
}

/// Makes the layout directory `dir` (which must not exist) holding the bundle of `config`
/// and `layers`, and returns the digest of its manifest.
pub(crate) fn write(
    dir: &Path,
    config: &Blob,
    layers: &[Blob],
    annotations: &BTreeMap<String, String>,
) -> Result<Digest, Error> {
    let manifest = Manifest {
        schema_version: 2,
        media_type: media::MANIFEST,
        artifact_type: media::ARTIFACT_TYPE,
        config: describe(media::CONFIG, config),
        layers: layers
            .iter()
            .map(|layer| describe(media::LAYER_TAR_ZSTD, layer))
            .collect(),
        annotations,
    };
    let manifest = Blob::new(serde_json::to_vec(&manifest)?);
    let index = Index {
        schema_version: 2,
        media_type: INDEX_TYPE,
        manifests: vec![Entry {
            media_type: media::MANIFEST,
            digest: manifest.digest.as_str(),
            size: manifest.bytes.len() as u64,
            artifact_type: media::ARTIFACT_TYPE,
        }],
    };
    let index = serde_json::to_vec(&index)?;
    // `dir` is claimed first (an existing one is refused), the layout is built beside it
    // and renamed over the claim, so a failure at any point leaves neither.
    std::fs::create_dir(dir).map_err(at(dir))?;
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    let staged = dir.with_file_name(format!(".{name}.partial"));
    let built = fill(&staged, config, layers, &manifest, &index)
        .and_then(|()| std::fs::rename(&staged, dir).map_err(at(dir)));
    match built {
        Ok(()) => Ok(manifest.digest),
        Err(error) => {
            // Best effort: both are ours, made above.
            let _ = std::fs::remove_dir_all(&staged);
            let _ = std::fs::remove_dir(dir);
            Err(error)
        }
    }
}

/// Makes `staged` and writes the layout into it.
fn fill(
    staged: &Path,
    config: &Blob,
    layers: &[Blob],
    manifest: &Blob,
    index: &[u8],
) -> Result<(), Error> {
    std::fs::create_dir(staged).map_err(at(staged))?;
    let blobs = staged.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).map_err(at(&blobs))?;
    for blob in layers.iter().chain([config, manifest]) {
        put(&blobs.join(blob.digest.hex()), &blob.bytes)?;
    }
    put(&staged.join("oci-layout"), MARKER)?;
    put(&staged.join("index.json"), index)
}
