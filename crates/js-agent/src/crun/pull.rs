//! The pulls of the crun runtime: an image made sure of in the store within a time limit,
//! and counted against the image budget when there is one (the module `budget`).

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use js_contain::oci::Oci;
use js_contain::oci::image::{IMAGES_DIR, PinnedImage};
use tokio::task::JoinHandle;

use super::budget::{Book, Budget, Evicted, Store, admit, load};
use super::spec::pinned;
use crate::disk::{held_images, tree_bytes};
use crate::lifecycle::PullFuture;

/// `image` in the store, fetched and unpacked if it is not, within `limit` for the whole.
pub(super) async fn ensure_image(
    oci: &Oci,
    image: &PinnedImage,
    limit: Duration,
) -> Result<(), String> {
    let ensured = tokio::time::timeout(limit, oci.check_pinned(image)).await;
    ensured
        .map_err(|_| format!("pulling {} took longer than {limit:?}", image.reference))?
        .map_err(|why| why.to_string())
}

/// The image store of an [`Oci`], whose images are in `images`.
struct OciStore {
    oci: Arc<Oci>,
    images: PathBuf,
    limit: Duration,
}

#[async_trait]
impl Store for OciStore {
    async fn ensure(&self, image: &PinnedImage) -> Result<(), String> {
        ensure_image(&self.oci, image, self.limit).await
    }

    fn bytes(&self, image: &PinnedImage) -> io::Result<u64> {
        let hex = image
            .digest
            .strip_prefix("sha256:")
            .unwrap_or(&image.digest);
        tree_bytes(&self.images.join(hex))
    }

    fn held(&self) -> io::Result<Vec<(String, u64)>> {
        held_images(&self.images)
    }

    async fn remove(&self, image: &PinnedImage) -> io::Result<()> {
        self.oci.remove_image(image).await
    }
}

/// The images of a runtime: its store, and the budget the store is held to, if any.
pub(super) struct Images {
    store: Arc<dyn Store>,
    budget: Option<Budget>,
}

impl Images {
    /// The images of the store of `oci` (whose scratch directory is `scratch`), each pull
    /// within `limit`, held to `budget` bytes when there is one.
    pub(super) fn of(
        oci: &Arc<Oci>,
        scratch: &std::path::Path,
        limit: Duration,
        budget: Option<u64>,
    ) -> Arc<Self> {
        let store = OciStore {
            oci: Arc::clone(oci),
            images: scratch.join(IMAGES_DIR),
            limit,
        };
        Arc::new(Self::new(Arc::new(store), budget, BTreeSet::new()))
    }

    /// The images of `store`, held to `budget` bytes when there is one, never evicting the
    /// digests in `keep`.
    pub(super) fn new(store: Arc<dyn Store>, budget: Option<u64>, keep: BTreeSet<String>) -> Self {
        let budget = budget.map(|bytes| Budget::new(Book::new(bytes, keep)));
        Self { store, budget }
    }

    /// From now on each image the budget evicts is told to `evicted`, by digest.
    pub(super) fn on_evict(&self, evicted: Evicted) {
        if let Some(budget) = &self.budget {
            budget.on_evict(evicted);
        }
    }

    /// `image` in the store and within the budget: fetched and unpacked if it is not, and
    /// then counted, evicting what must go for it to fit.
    ///
    /// # Errors
    /// The pull failed or took too long, or the image does not fit the budget.
    pub(super) async fn ensure(&self, image: &PinnedImage) -> Result<(), String> {
        let Some(budget) = &self.budget else {
            return self.store.ensure(image).await;
        };
        load(budget, &*self.store).await?;
        self.store.ensure(image).await?;
        admit(budget, &*self.store, image).await
    }

    /// `image` in the store for a start: the use of an image the store holds must not wait
    /// for another image's measure or eviction, nor for the listing of the store.
    ///
    /// Without a budget this is the store's `ensure`. With a budget that is loaded and counts
    /// the image, it is the store's `ensure` (a stat when the image is there) and then a use
    /// of the image in the book, which counts for the order of eviction; no turn is taken.
    /// With a budget that is loaded and does not count the image, it is [`Images::ensure`]
    /// in full. With a budget that is not loaded yet (the store is being listed, or could
    /// not be), it is the store's `ensure` and a use the book keeps for the listing, which
    /// counts the image: no turn is taken either, unless the listing ended in between, and
    /// then the image is counted as a pull counts it. An image this fetched while the
    /// listing was under way, and that the listing missed, is counted once a pull meets it.
    ///
    /// # Errors
    /// As [`Images::ensure`].
    pub(super) async fn use_held(&self, image: &PinnedImage) -> Result<(), String> {
        let Some(budget) = &self.budget else {
            return self.store.ensure(image).await;
        };
        if budget.holds(&image.digest) {
            self.store.ensure(image).await?;
            budget.book().touch(&image.digest);
            return Ok(());
        }
        if budget.loaded() {
            return self.ensure(image).await;
        }
        self.store.ensure(image).await?;
        if budget.book().early_use(&image.digest) {
            return Ok(());
        }
        admit(budget, &*self.store, image).await
    }

    /// Counts the images the store holds now against the budget (the module `budget`'s
    /// `load`), on a blocking thread of `rt`, so that neither a start nor a pull waits for
    /// the walk of the store when the agent has just started. Nothing without a budget. A
    /// store that cannot be listed is logged, and the first pull lists it again.
    pub(super) fn count_held(self: &Arc<Self>, rt: &tokio::runtime::Runtime) -> JoinHandle<()> {
        let images = Arc::clone(self);
        let calls = rt.handle().clone();
        rt.spawn_blocking(move || {
            let Some(budget) = &images.budget else {
                return;
            };
            if let Err(why) = calls.block_on(load(budget, &*images.store)) {
                tracing::warn!("{why}: the first pull lists the store again");
            }
        })
    }

    /// The pull and unpack of `image` (a reference pinned by digest), counted against the
    /// budget: what the prefetcher runs.
    pub(super) fn pull(self: &Arc<Self>, image: String) -> PullFuture {
        let images = Arc::clone(self);
        Box::pin(async move {
            let pinned = pinned(&image)?;
            images.ensure(&pinned).await
        })
    }

    /// The lease `name` holds the image `digest` until [`Images::release`]: the budget
    /// evicts it no more, and counts it as used now.
    pub(super) fn lease(&self, name: &str, digest: &str) {
        if let Some(budget) = &self.budget {
            budget.book().lease(name, digest);
        }
    }

    /// The lease `name` lets its image go (its last use); a lease that holds none is
    /// nothing.
    pub(super) fn release(&self, name: &str) {
        if let Some(budget) = &self.budget {
            budget.book().release(name);
        }
    }

    /// The bytes of the images the budget counts: 0 without a budget.
    pub(super) fn counted(&self) -> u64 {
        self.budget
            .as_ref()
            .map_or(0, |budget| budget.book().total())
    }
}

impl super::CrunRuntime {
    /// The bytes of the unpacked images the image budget counts now: 0 without a budget.
    /// For a test that sets a budget by the size of the images it pulls.
    #[doc(hidden)]
    #[must_use]
    pub fn image_bytes(&self) -> u64 {
        self.images.counted()
    }
}
