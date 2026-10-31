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
//! (and a daemon started later on the same data directory, once the first is gone) finds
//! the directory and fetches nothing. Daemons share a directory, and only one
//! that has it to itself sweeps stages ([`super::lock`]).
//!
//! The unpack runs in a user namespace with the daemon user unmapped, the stage directory
//! the only writable path and the downloaded blobs read-only, so a hostile tar can write
//! nothing the daemon user owns.
//!
//! The rename publishes the image, so the unpacked tree is flushed to disk (`syncfs`)
//! before it and the images directory (`fsync`) after it: a power loss leaves a published
//! image whole, or none.
//!
//! **The stage belongs to the helper's id and to nobody else may write it**: the daemon
//! makes it `0700`, opens it by descriptor (`O_NOFOLLOW`), sets `0755` and gives it to the
//! first id of the subordinate range (the id the helper's root has; see [`make_stage`]),
//! in a store directory (`0711`) in which a local account can neither list nor write. The
//! mode is `0755`, not `0700`, because the stage becomes the image directory by a rename
//! and a job's `crun` (not the helper's id) must search it; a rootless daemon cannot change
//! the mode of a directory it gave away, so it is final from the start. Others can read
//! the stage and cannot write it, move the root filesystem the helper makes in it, or
//! add to it.
//!
//! Single flight per digest: concurrent first uses of one image wait on one fetch and
//! unpack; a use cancelled midway leaves another to retry (a stage directory with a
//! different random name each time; the leftovers are removed by the sweep at the next
//! start).

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use rustix::fs::{Gid, Mode, Uid};

use super::{JobError, Mapping, Oci, at, helper};
use crate::walk::DIRECTORY;

/// The directory of the rootfs under an image's directory.
pub const ROOTFS: &str = "rootfs";

/// The directory of the image store under a backend's scratch directory.
pub const IMAGES_DIR: &str = "oci-images";

/// Where a backend whose scratch directory is `scratch` publishes the root filesystem of
/// the image whose digest is `digest` (`sha256:<hex>`): `<scratch>/oci-images/<hex>/rootfs`.
/// A directory there means the image is complete (the stage is renamed into place last).
#[must_use]
pub fn published_root(scratch: &Path, digest: &str) -> PathBuf {
    scratch
        .join(IMAGES_DIR)
        .join(digest.strip_prefix("sha256:").unwrap_or(digest))
        .join(ROOTFS)
}

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

/// The owner, in a setup helper's namespace ([`Mapping::Setup`]), of the id the unpack
/// helper runs as: the unpack helper's id 0 ([`Mapping::Action`]) is the first subordinate
/// id, which is id 1 there.
pub(super) const HELPER_OWNER: &str = "1:1";

/// The mode of a stage: the helper's id writes it; everyone else may search and read.
const STAGE_MODE: u32 = 0o755;

/// The mode of the store directory: others search it (a job's `crun` reaches an image's
/// root through it) but cannot list it, so a stage's name is not for the finding.
const STORE_MODE: u32 = 0o711;

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

    /// Moves the directory of the digest `hex` to a name of its own in the store
    /// (`.stage-gc-<hex>-<id>`) and returns it: a rename within the store, so the image is
    /// gone at once (`has` is false) and what is left to remove is a leftover the start
    /// sweep takes if the removal fails midway. The caller holds the digest's gate.
    ///
    /// # Errors
    /// There is no such directory, or the rename failed.
    pub(super) fn set_aside(&self, hex: &str) -> std::io::Result<PathBuf> {
        let aside = self
            .dir
            .join(format!("{STAGE_PREFIX}gc-{hex}-{}", unique()));
        std::fs::rename(self.dir.join(hex), &aside)?;
        Ok(aside)
    }

    /// Removes the image's directory (an image already gone is not an error) and forgets
    /// that it was fetched, so that the next use fetches it again. It waits for a fetch of
    /// the same image under way, and a fetch started meanwhile waits for it, so the two
    /// never work in the directory together. The directory is first set aside by a rename
    /// ([`Images::set_aside`]), so a removal that fails midway leaves no image that still
    /// looks complete. The caller must see to it that no container still runs on the image
    /// (its root is the lower layer of a container's mounts).
    ///
    /// # Errors
    /// The directory could not be set aside (the image is kept), or what was set aside
    /// could not be removed (the image is gone; the start sweep takes the rest).
    pub(crate) async fn remove(&self, image: &PinnedImage) -> std::io::Result<()> {
        let flight = self.gate(image);
        let mut fetched = flight.lock().await;
        *fetched = false;
        match self.set_aside(&hex_of(image)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
            Ok(aside) => crate::remove::remove_tree(&aside),
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
        // Searched, not listed, by the container's ids on the way to an image's root,
        // whatever umask the daemon runs under.
        let open = std::fs::Permissions::from_mode(STORE_MODE);
        at(&self.dir, tokio::fs::set_permissions(&self.dir, open).await).map_err(failed)?;
        let made = self.fetch_and_unpack(oci, image, &stage, &blobs).await;
        // The blobs are the daemon's own files.
        let _ = tokio::fs::remove_dir_all(&blobs).await;
        if made.is_err() && stage.exists() {
            // The stage holds files only the container's ids own.
            let _ = oci.remove_dir_async(&stage).await;
        }
        made?;
        // The stage is already the mode an image directory has (0755), made by make_stage.
        // The rename is what says the image is complete, so everything it makes visible is
        // on disk first: after a power loss a published directory is whole or absent.
        at(&stage, blocking(&stage, flush_fs).await).map_err(failed)?;
        let finished = self.dir.join(&hex);
        at(&finished, tokio::fs::rename(&stage, &finished).await).map_err(failed)?;
        // And the name itself, so that "pulled" is not said of one a crash can take back.
        at(&self.dir, blocking(&self.dir, flush_dir).await).map_err(failed)
    }

    /// Downloads the image's layers into `blobs` (a directory the container's ids can
    /// read), makes `stage` (given to the container's root, which is not the daemon user:
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
        oci.make_stage(stage).await.map_err(failed)?;
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

/// Writes everything on the file system `path` is on to disk (`syncfs`): the files and
/// directories of an unpack, made by ids this process does not own, in one call.
pub(crate) fn flush_fs(path: &Path) -> std::io::Result<()> {
    Ok(rustix::fs::syncfs(std::fs::File::open(path)?)?)
}

/// Writes the entries of the directory `path` to disk (`fsync` of the directory).
pub(crate) fn flush_dir(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

/// Runs `work` on `path` on a thread that may block.
async fn blocking(path: &Path, work: fn(&Path) -> std::io::Result<()>) -> std::io::Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || work(&path))
        .await
        .map_err(std::io::Error::from)
        .and_then(|done| done)
}

impl Oci {
    /// Makes the stage directory `stage` and gives it to the unpack helper's id: by
    /// [`make_stage`] when the daemon is root (which can give any id a directory), else
    /// by a setup helper container, which holds the daemon user and the subordinate range
    /// both.
    async fn make_stage(&self, stage: &Path) -> Result<(), JobError> {
        let helper_ids = (
            Uid::from_raw(self.ids.sub_uid),
            Gid::from_raw(self.ids.sub_gid),
        );
        let owner = rustix::process::geteuid().is_root().then_some(helper_ids);
        at(stage, make_stage(stage, owner, &mut fchown_fd))?;
        // A daemon that could not give the stage itself has it given.
        let given_by_helper = owner
            .is_none()
            .then(|| helper::give_job(&self.modes, &self.daemon, HELPER_OWNER, None, &[stage]));
        let jobs: Vec<helper::Job> = given_by_helper.into_iter().collect();
        self.run_jobs("give", Mapping::Setup, &jobs).await
    }
}

/// Gives the directory `fd` to `uid` and `gid`.
pub(super) fn fchown_fd(fd: &OwnedFd, uid: Uid, gid: Gid) -> std::io::Result<()> {
    Ok(rustix::fs::fchown(fd, Some(uid), Some(gid))?)
}

/// Makes the directory `stage` (not following a symlink at its name, and failing if the
/// name exists), `0700` while it is the daemon's alone, then, by its descriptor, `0755`
/// ([`STAGE_MODE`]) and, if `owner` is given, owned by it through `chown`.
///
/// # Errors
/// The directory exists or could not be made, opened, changed or given.
pub(super) fn make_stage(
    stage: &Path,
    owner: Option<(Uid, Gid)>,
    chown: &mut dyn FnMut(&OwnedFd, Uid, Gid) -> std::io::Result<()>,
) -> std::io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(stage)?;
    let fd = rustix::fs::open(stage, DIRECTORY, Mode::empty())?;
    rustix::fs::fchmod(&fd, Mode::from_raw_mode(STAGE_MODE))?;
    if let Some((uid, gid)) = owner {
        chown(&fd, uid, gid)?;
    }
    Ok(())
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
