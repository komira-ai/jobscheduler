//! Finding a bundle in a layout and reading it the way an agent would: the manifest and
//! the config come through [`Layout`], which checks digests, media types and limits.

use std::io::{self, Read as _};
use std::path::Path;

use js_bundle::BundleConfig;
use js_oci::{BundleSource as _, Digest, Kind, Layout, Manifest};
use rustix::fs::{FileType, Mode, OFlags};
use serde::Deserialize;

use crate::cli::LayoutArgs;
use crate::error::{Error, at};

/// The most bytes of `index.json` read, the same cap the layout reader puts on it.
const MAX_INDEX_BYTES: u64 = 4 << 20;

/// A bundle read from a layout.
pub(crate) struct Loaded {
    pub(crate) layout: Layout,
    pub(crate) manifest: Manifest,
    pub(crate) config: BundleConfig,
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<Listed>,
}

#[derive(Deserialize)]
struct Listed {
    digest: String,
}

/// The manifest digest to read: the one given, or the only one `index.json` lists.
fn select(layout: &Path, digest: Option<&str>) -> Result<Digest, Error> {
    if let Some(text) = digest {
        return Ok(Digest::parse(text)?);
    }
    let path = layout.join("index.json");
    // Opened without following a link and without blocking, and checked to be a regular
    // file through the descriptor, like every other read of a layout.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::open(&path, flags, Mode::empty())
        .map_err(io::Error::from)
        .map_err(at(&path))?;
    let kind = FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode);
    if kind != FileType::RegularFile {
        return Err(Error::NotRegular {
            path: path.display().to_string(),
        });
    }
    let file = std::fs::File::from(fd);
    let mut bytes = Vec::new();
    file.take(MAX_INDEX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(at(&path))?;
    let index: Index = serde_json::from_slice(&bytes)?;
    match index.manifests.as_slice() {
        [only] => Ok(Digest::parse(&only.digest)?),
        many => Err(Error::Ambiguous { count: many.len() }),
    }
}

/// Reads the bundle `args` names: its manifest (of the bundle kind) and its config
/// (parsed and validated), which must describe as many layers as the manifest lists.
pub(crate) async fn load(args: &LayoutArgs) -> Result<Loaded, Error> {
    let layout = Layout::open(&args.layout).await?;
    let digest = select(&args.layout, args.digest.as_deref())?;
    let kind = Kind::bundle();
    let manifest = layout.fetch_manifest(&digest, &kind).await?;
    let config = layout.fetch_config(&manifest, &kind).await?;
    let config = BundleConfig::parse(&config)?;
    if manifest.layers().len() != config.layers.len() {
        return Err(Error::LayerCount {
            manifest: manifest.layers().len(),
            config: config.layers.len(),
        });
    }
    Ok(Loaded {
        layout,
        manifest,
        config,
    })
}
