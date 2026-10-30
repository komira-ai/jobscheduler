//! The per-node image store of the OCI backend: each image digest fetched from its
//! registry and unpacked once into `<images>/<hex>/rootfs`, read-only afterwards, shared
//! by every job that names it.
//!
//! **Acquisition is the caller's** ([`ImageSource`]): it must fetch the manifest by the
//! pinned digest and verify it hashes to it (and is one image, not an index), and write
//! the layer blobs, each verified against the manifest. No container tool takes part.
//! Then a helper container (`helper::unpack_job`) applies the layers with the host
//! binary's unpack mode ([`HelperModes`](super::helper::HelperModes)) inside a user namespace that has the same mapping as a job's, so every file ends up owned
//! by the host id the job sees as the image's own id (root is the first subordinate
//! id), with no shifting pass. After that a job needs one `stat` of
//! `<hex>/rootfs`.
//!
//! The first use of an image pays for the fetch and the unpack, on the lease's prepare
//! step or, for the images the daemon is given up front, at start; a second use
//! (and a second daemon on the same data directory) finds the directory and fetches
//! nothing.
//!
//! The unpack runs in a user namespace with the daemon user unmapped, the stage directory
//! the only writable path and the downloaded blobs read-only, so a hostile tar can write
//! nothing the daemon user owns.
//!
//! Single flight per digest: concurrent first uses of one image wait on one fetch and
//! unpack; a use cancelled midway leaves another to retry (a stage directory with a
//! different random name each time; the leftovers are removed by the sweep at the next
//! start).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use super::{JobError, Oci, at};

/// The directory of the rootfs under an image's directory.
pub const ROOTFS: &str = "rootfs";

/// An image named by the digest of its per-architecture manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedImage {
    /// `<repo>@sha256:<hex>`, as the source is asked for it.
    pub reference: String,
    /// `sha256:<hex>`: the digest alone.
    pub digest: String,
}

impl std::fmt::Display for PinnedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reference)
    }
}

/// Why this node cannot run an image named by digest.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImageCheckError {
    /// Its digest names an image index (a multi-architecture list).
    #[error("image {0} names an image index; name the per-architecture manifest digest")]
    Index(PinnedImage),
    /// The image could not be fetched or unpacked.
    #[error("{0}")]
    Failed(String),
}

/// Why a source could not fetch an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The digest names an image index, not one image.
    Index,
    /// Anything else, in words fit for an operator.
    Failed(String),
}

/// Where the layers of an image come from: the caller's registry client.
#[async_trait]
pub trait ImageSource: Send + Sync + std::fmt::Debug {
    /// Writes the layer blobs of `image` into the existing directory `blobs`, each as a
    /// file whose name is returned, base layer first. Every blob must be verified against
    /// the manifest and the manifest against `image.digest` before this returns.
    ///
    /// # Errors
    /// [`FetchError::Index`] if the digest names an index; otherwise the registry cannot
    /// be reached or refuses, or something does not hash to its digest.
    async fn fetch(&self, image: &PinnedImage, blobs: &Path) -> Result<Vec<String>, FetchError>;
}

/// The prefix of an unpack in progress.
pub const STAGE_PREFIX: &str = ".stage-";

/// The per-node store.
#[derive(Debug)]
pub(crate) struct Images {
    dir: PathBuf,
    /// One gate for each digest: whether the image was fetched and unpacked by this
    /// daemon (`true`), held while it is being fetched or removed, so the two never overlap.
    flights: Mutex<HashMap<String, Arc<AsyncMutex<bool>>>>,
}

/// The hex of an image's `sha256:<hex>` digest, the name of its directory.
fn hex_of(image: &PinnedImage) -> String {
    image
        .digest
        .strip_prefix("sha256:")
        .unwrap_or(&image.digest)
        .to_owned()
}

impl Images {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            flights: Mutex::new(HashMap::new()),
        }
    }

    /// The directory of the image whose digest is `digest` (`sha256:<hex>`), which holds its
    /// root filesystem.
    pub(crate) fn dir_of(&self, digest: &str) -> PathBuf {
        self.dir
            .join(digest.strip_prefix("sha256:").unwrap_or(digest))
    }

    /// The root filesystem of the image whose digest is `digest` (`sha256:<hex>`).
    pub(crate) fn rootfs_of(&self, digest: &str) -> PathBuf {
        self.dir
            .join(digest.strip_prefix("sha256:").unwrap_or(digest))
            .join(ROOTFS)
    }

    /// Whether `image` is unpacked.
    pub(crate) fn has(&self, image: &PinnedImage) -> bool {
        self.rootfs_of(&image.digest).is_dir()
    }

    /// Fetches and unpacks `image` unless it is, waiting for a fetch already under way.
    ///
    /// # Errors
    /// The registry cannot be reached or refuses, the digest names an index or a
    /// manifest or blob that does not hash to its digest, or the layers cannot be
    /// unpacked.
    pub(crate) async fn ensure(
        &self,
        oci: &Oci,
        image: &PinnedImage,
    ) -> Result<(), ImageCheckError> {
        if self.has(image) {
            return Ok(());
        }
        let flight = self.gate(image);
        // Concurrent first uses wait here for one fetch and unpack; a use that finds it
        // done is done. A fetch that failed or was cancelled leaves the gate open for the
        // next use to try again.
        let mut fetched = flight.lock().await;
        if !*fetched {
            self.acquire(oci, image).await?;
            *fetched = true;
        }
        Ok(())
    }

    /// The gate of `image`.
    fn gate(&self, image: &PinnedImage) -> Arc<AsyncMutex<bool>> {
        self.gate_of(&hex_of(image))
    }

    /// The gate of the digest whose hex is `hex`.
    pub(super) fn gate_of(&self, hex: &str) -> Arc<AsyncMutex<bool>> {
        let mut flights = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(flights.entry(hex.to_owned()).or_default())
    }

    /// The directory of the store.
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Removes the image's directory (an image already gone is not an error) and forgets
    /// that it was fetched, so that the next use fetches it again. It waits for a fetch of
    /// the same image under way, and a fetch started meanwhile waits for it, so the two
    /// never work in the directory together. The caller must see to it that no container
    /// still runs on the image (its root is the lower layer of a container's mounts).
    ///
    /// # Errors
    /// The directory could not be removed.
    pub(crate) async fn remove(&self, image: &PinnedImage) -> std::io::Result<()> {
        let flight = self.gate(image);
        let mut fetched = flight.lock().await;
        *fetched = false;
        match crate::remove::remove_tree(&self.dir_of(&image.digest)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Fetches the layers into a directory of blobs, unpacks them into a stage directory,
    /// then renames the stage to the image's directory: presence of that name means
    /// complete. The blobs and the stage are removed whatever happens.
    async fn acquire(&self, oci: &Oci, image: &PinnedImage) -> Result<(), ImageCheckError> {
        let hex = hex_of(image);
        let id = unique();
        let stage = self.dir.join(format!("{STAGE_PREFIX}{hex}-{id}"));
        let blobs = self.dir.join(format!("{STAGE_PREFIX}{hex}-{id}-blobs"));
        at(&self.dir, tokio::fs::create_dir_all(&self.dir).await).map_err(failed)?;
        // Searched by the container's ids on the way to an image's root, whatever umask
        // the daemon runs under.
        let open = std::fs::Permissions::from_mode(0o755);
        at(&self.dir, tokio::fs::set_permissions(&self.dir, open).await).map_err(failed)?;
        let made = self.fetch_and_unpack(oci, image, &stage, &blobs).await;
        // The blobs are the daemon's own files.
        let _ = tokio::fs::remove_dir_all(&blobs).await;
        if made.is_err() && stage.exists() {
            // The stage holds files only the container's ids own.
            let _ = oci.remove_dir_async(&stage).await;
        }
        made?;
        // The stage was open to the container's root; the image directory is not.
        let closed = std::fs::Permissions::from_mode(0o755);
        at(&stage, tokio::fs::set_permissions(&stage, closed).await).map_err(failed)?;
        let finished = self.dir.join(&hex);
        at(&finished, tokio::fs::rename(&stage, &finished).await).map_err(failed)
    }

    /// Downloads the image's layers into `blobs` (a directory the container's ids can
    /// read), makes `stage` (open to the container's root, which is not the daemon user:
    /// it makes the image's root inside, owned by itself) and unpacks the layers into it.
    async fn fetch_and_unpack(
        &self,
        oci: &Oci,
        image: &PinnedImage,
        stage: &Path,
        blobs: &Path,
    ) -> Result<(), ImageCheckError> {
        at(blobs, tokio::fs::create_dir(blobs).await).map_err(failed)?;
        let open = std::fs::Permissions::from_mode(0o755);
        at(blobs, tokio::fs::set_permissions(blobs, open).await).map_err(failed)?;
        let names = oci
            .source()
            .fetch(image, blobs)
            .await
            .map_err(|e| match e {
                FetchError::Index => ImageCheckError::Index(image.clone()),
                FetchError::Failed(why) => ImageCheckError::Failed(why),
            })?;
        for name in &names {
            let readable = std::fs::Permissions::from_mode(0o644);
            let path = blobs.join(name);
            at(&path, tokio::fs::set_permissions(&path, readable).await).map_err(failed)?;
        }
        at(stage, tokio::fs::create_dir(stage).await).map_err(failed)?;
        let open = std::fs::Permissions::from_mode(0o777);
        at(stage, tokio::fs::set_permissions(stage, open).await).map_err(failed)?;
        oci.unpack_layers(stage, blobs, &names)
            .await
            .map_err(failed)
    }

    /// Removes what an unpack that was killed left: the stage directories and blobs.
    /// Blocking; the files in a stage are owned by subordinate ids, so it needs `oci`.
    ///
    /// # Errors
    /// What could not be removed, in words that name the sweep.
    pub(crate) fn sweep(&self, oci: &Oci) -> Result<(), String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Ok(());
        };
        let mut errors = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(STAGE_PREFIX)
            {
                continue;
            }
            let removed = if path.is_dir() {
                oci.remove_dir_blocking(&path)
            } else {
                at(&path, std::fs::remove_file(&path))
            };
            errors.extend(removed.err().map(|e| e.to_string()));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "removing unpacks a previous daemon left: {}",
                errors.join("; ")
            ))
        }
    }
}

/// A step of the fetch or the unpack that failed, as the check of an image reports it.
fn failed(error: JobError) -> ImageCheckError {
    ImageCheckError::Failed(error.to_string())
}

/// A name part unique to this process and call.
fn unique() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
