//! `verify`: a bundle, checked as an agent would take it, layer by layer.
//!
//! [`load`] reads the manifest and the config (digests, kind, media types, limits, the
//! config's rules). Each layer is then fetched with its length and digest checked, and
//! extracted by the same extractor an agent's helper runs into a scratch directory, under
//! the ceilings the config declares: that checks the layer's diff id and every path, mode,
//! symlink and entry rule of a tree. The scratch directory is made here and removed when
//! the command ends, whatever the result.

use std::io::Write;
use std::os::fd::AsFd as _;
use std::path::{Path, PathBuf};

use js_bundle::media::LayerEncoding;
use js_bundle::{DEFAULT_MODEL_BYTES, Limits, unpack_layer};
use js_oci::BundleSource as _;

use crate::bundle::load;
use crate::cli::VerifyArgs;
use crate::error::{Error, at};

/// A directory this command made and removes.
struct Scratch(PathBuf);

impl Scratch {
    fn create(path: &Path) -> Result<Scratch, Error> {
        std::fs::create_dir(path).map_err(at(path))?;
        Ok(Scratch(path.to_owned()))
    }

    /// A new empty directory for layer `index`, opened.
    fn tree(&self, index: usize) -> Result<std::fs::File, Error> {
        let dir = self.0.join(index.to_string());
        std::fs::create_dir(&dir).map_err(at(&dir))?;
        std::fs::File::open(&dir).map_err(at(&dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort: the directory is ours and holds only what the extractor wrote.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) async fn verify(args: &VerifyArgs, out: &mut dyn Write) -> Result<(), Error> {
    let loaded = load(&args.source).await?;
    let scratch = Scratch::create(&args.scratch)?;
    let layers = loaded.manifest.layers().iter().zip(&loaded.config.layers);
    for (index, (blob, layer)) in layers.enumerate() {
        let encoding = LayerEncoding::from_media_type(blob.media_type()).ok_or(Error::LayerType)?;
        let mut bytes = Vec::new();
        loaded.layout.fetch_blob(blob, &mut bytes).await?;
        let root = scratch.tree(index)?;
        let limits = Limits {
            entries: layer.entries,
            bytes: layer.uncompressed_bytes,
            model_bytes: DEFAULT_MODEL_BYTES,
        };
        unpack_layer(
            bytes.as_slice(),
            encoding,
            root.as_fd(),
            &layer.diff_id,
            &limits,
        )
        .map_err(|source| Error::Layer {
            index,
            tree: layer.tree.as_str().to_owned(),
            source,
        })?;
    }
    let summary = format!(
        "verified {} ({} layers)",
        loaded.manifest.digest(),
        loaded.manifest.layers().len()
    );
    writeln!(out, "{summary}")?;
    Ok(())
}
