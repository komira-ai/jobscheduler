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
