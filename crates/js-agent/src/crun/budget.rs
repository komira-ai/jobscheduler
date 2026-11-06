//! The image budget of the crun runtime (`--image-budget-gib`): the most bytes the unpacked
//! images of the store may take together, held by evicting the image used least recently.
//!
//! [`Book`] is what the budget knows, and decides from: each image the store holds with the
//! bytes it takes and when it was last used, the images that are kept (never evicted), and
//! the leases that hold an image. "When" is a counter, not a clock: every use takes the next
//! value, so the order of uses is the order of eviction and no time is read. An image is
//! used when a lease takes it and again when the lease lets it go (the end of a run is its
//! last use), and when a pull asks for it.
//!
//! [`admit`] runs at the end of each pull, once the image is in the store: it counts the
//! image (measured the first time the budget meets it), and while the store is over its
//! budget it evicts the images used least recently that no lease holds and that are not
//! kept, oldest first, through the store's removal ([`Store::remove`],
//! `js_contain::oci::Oci::remove_image`), telling the prefetcher of each ([`Evicted`]) so
//! that it pulls the image again the next time it is asked for. When the store cannot be
//! brought within its budget by evicting every image that may be evicted, nothing is
//! evicted: the pull fails with [`NoCapacity`], and the image it brought is removed again
//! (unless a lease holds it), so the store holds what it held before. A pull of an image
//! the store held already adds no bytes and never fails for the budget: it evicts what the
//! store is over by when that can be done, and nothing when it cannot.
//!
//! Admissions take turns (one gate for the whole store), so two pulls that end together do
//! not both evict for the room of one. The book itself is locked only between the steps of
//! an admission, never across a removal, so a lease taken while an admission removes is not
//! held up by it; a lease taken on an image just chosen for eviction pins it in the store
//! first (`Oci::pin_image`), and the store then refuses its removal: that image is kept, and
//! the next one in order is evicted instead. Only then can an admission end with
//! [`NoCapacity`] after it evicted something.
//!
//! The images the store held before this agent started are counted when the first pull
//! starts ([`load`], before its fetch), all used before anything since, in the order of
//! their digests. What a
//! budget does not see: the images another agent on the same store pulls are counted only
//! once a pull of this one meets them, and the bytes an unpack under way takes before it is
//! published.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use async_trait::async_trait;
use js_contain::oci::image::PinnedImage;

/// The store the budget evicts from.
#[async_trait]
pub(super) trait Store: Send + Sync {
    /// Makes sure `image` is in the store: fetched and unpacked when it is not.
    async fn ensure(&self, image: &PinnedImage) -> Result<(), String>;

    /// The bytes `image` takes in the store.
    fn bytes(&self, image: &PinnedImage) -> io::Result<u64>;

    /// Every image the store holds, by digest (`sha256:<hex>`), with the bytes it takes.
    fn held(&self) -> io::Result<Vec<(String, u64)>>;

    /// Removes `image` from the store. Refused when a lease pins it.
    async fn remove(&self, image: &PinnedImage) -> io::Result<()>;
}

/// What is told of each image the budget evicts: its digest (`sha256:<hex>`).
pub(super) type Evicted = Box<dyn Fn(&str) + Send + Sync>;

/// Why a pull does not fit the budget.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "no-capacity: image {image} takes {bytes} bytes, and the image budget of {budget} bytes \
     holds {fixed} bytes of images that a run holds, that are kept or that the store refused to remove"
)]
pub(super) struct NoCapacity {
    /// The image pulled.
    pub image: String,
    /// The bytes it takes.
    pub bytes: u64,
    /// The budget.
    pub budget: u64,
    /// The bytes of the other images that may not be evicted, or whose removal the store
    /// refused.
    pub fixed: u64,
}

/// One image the store holds, as the budget counts it.
#[derive(Debug, Clone)]
struct Held {
    image: PinnedImage,
    bytes: u64,
    used: u64,
}

/// What the budget knows of the store. See the [module](self) documentation.
#[derive(Debug)]
pub(super) struct Book {
    budget: u64,
    keep: BTreeSet<String>,
    held: BTreeMap<String, Held>,
    leases: BTreeMap<String, String>,
    uses: u64,
    /// Whether the images the store held before the first admission are counted.
    loaded: bool,
}

impl Book {
    /// A book of `budget` bytes that never evicts the digests in `keep`.
    pub(super) fn new(budget: u64, keep: BTreeSet<String>) -> Self {
        Self {
            budget,
            keep,
            held: BTreeMap::new(),
            leases: BTreeMap::new(),
            uses: 0,
            loaded: false,
        }
    }

    /// The next use.
    fn tick(&mut self) -> u64 {
        self.uses += 1;
        self.uses
    }

    /// `digest` is used now, when the store holds it.
    fn touch(&mut self, digest: &str) {
        let now = self.tick();
        if let Some(held) = self.held.get_mut(digest) {
            held.used = now;
        }
    }

    /// The lease `name` holds the image `digest` until [`Book::release`].
    pub(super) fn lease(&mut self, name: &str, digest: &str) {
        self.leases.insert(name.to_owned(), digest.to_owned());
        self.touch(digest);
    }

    /// The lease `name` lets its image go: its last use. A name that holds none is nothing.
    pub(super) fn release(&mut self, name: &str) {
        if let Some(digest) = self.leases.remove(name) {
            self.touch(&digest);
        }
    }

    /// Counts `image`, of `bytes`, as held and used now.
    fn record(&mut self, image: PinnedImage, bytes: u64) {
        let used = self.tick();
        self.held
            .insert(image.digest.clone(), Held { image, bytes, used });
    }

    /// The bytes of every image counted.
    pub(super) fn total(&self) -> u64 {
        self.held.values().map(|held| held.bytes).sum()
    }

    /// Whether `digest` may not be evicted: it is kept, or a lease holds it.
    fn fixed(&self, digest: &str) -> bool {
        self.keep.contains(digest) || self.leases.values().any(|held| held == digest)
    }

    /// The images to evict, oldest first, for the store to be within its budget with
    /// `incoming` (counted already) in it, passing over those in `stuck`; or why it cannot
    /// be.
    fn plan(
        &self,
        incoming: &str,
        stuck: &BTreeSet<String>,
    ) -> Result<Vec<PinnedImage>, NoCapacity> {
        let mut over = self.total().saturating_sub(self.budget);
        let mut evictable: Vec<&Held> = self
            .held
            .values()
            .filter(|held| held.image.digest != incoming)
            .filter(|held| !self.fixed(&held.image.digest) && !stuck.contains(&held.image.digest))
            .collect();
        evictable.sort_by_key(|held| held.used);
        let mut chosen = Vec::new();
        for held in evictable {
            if over == 0 {
                break;
            }
            over = over.saturating_sub(held.bytes);
            chosen.push(held.image.clone());
        }
        if over > 0 {
            let bytes = self.held.get(incoming).map_or(0, |held| held.bytes);
            let free: u64 = chosen
                .iter()
                .filter_map(|image| self.held.get(&image.digest))
                .map(|held| held.bytes)
                .sum();
            return Err(NoCapacity {
                image: incoming.to_owned(),
                bytes,
                budget: self.budget,
                fixed: self.total() - bytes - free,
            });
        }
        Ok(chosen)
    }
}

/// The budget of one store: its book, the gate admissions take turns at, and who is told of
/// an eviction.
pub(super) struct Budget {
    book: Mutex<Book>,
    turn: tokio::sync::Mutex<()>,
    evicted: OnceLock<Evicted>,
}

impl Budget {
    /// The budget of `book`, which tells nobody of an eviction until [`Budget::on_evict`].
    pub(super) fn new(book: Book) -> Self {
        Self {
            book: Mutex::new(book),
            turn: tokio::sync::Mutex::new(()),
            evicted: OnceLock::new(),
        }
    }

    /// The book, locked.
    pub(super) fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// From now on each eviction is told to `evicted`; the first one given stays.
    pub(super) fn on_evict(&self, evicted: Evicted) {
        let _ = self.evicted.set(evicted);
    }

    /// The image `digest` is out of the store: no longer counted, and told.
    fn gone(&self, digest: &str) {
        self.book().held.remove(digest);
        if let Some(evicted) = self.evicted.get() {
            evicted(digest);
        }
    }
}

/// Counts the images `store` holds, the first time it is called for `budget`: before the
/// first pull's fetch, so that the image it brings is not taken for one that was there.
///
/// # Errors
/// The store cannot be listed ([`Store::held`]); the next call tries again.
pub(super) async fn load(budget: &Budget, store: &dyn Store) -> Result<(), String> {
    let _turn = budget.turn.lock().await;
    if budget.book().loaded {
        return Ok(());
    }
    let held = store
        .held()
        .map_err(|why| format!("the image budget: reading the image store: {why}"))?;
    let mut book = budget.book();
    for (digest, bytes) in held {
        let pinned = PinnedImage {
            reference: digest.clone(),
            digest,
        };
        book.record(pinned, bytes);
    }
    book.loaded = true;
    Ok(())
}

/// Counts `image`, which `store` holds now, against `budget`, and evicts what must go for it
/// to fit. See the [module](self) documentation. [`load`] ran first.
///
/// # Errors
/// The image cannot be measured ([`Store::bytes`]), or does not fit ([`NoCapacity`], in its
/// words).
pub(super) async fn admit(
    budget: &Budget,
    store: &dyn Store,
    image: &PinnedImage,
) -> Result<(), String> {
    let _turn = budget.turn.lock().await;
    // Held already: the image was in the store before this pull (or counted at the start).
    let before = budget.book().held.contains_key(&image.digest);
    if before {
        budget.book().touch(&image.digest);
    } else {
        let bytes = store
            .bytes(image)
            .map_err(|why| format!("the image budget: measuring {image}: {why}"))?;
        budget.book().record(image.clone(), bytes);
    }
    let mut stuck = BTreeSet::new();
    loop {
        let plan = budget.book().plan(&image.digest, &stuck);
        let victims = match plan {
            Ok(victims) => victims,
            // An image held before adds no bytes: its pull does not take the store over, so
            // it is not refused for a store already over (a smaller budget than at the last
            // start, or images that may not go).
            Err(_) if before => return Ok(()),
            Err(full) => {
                let fixed = budget.book().fixed(&image.digest);
                if !before && !fixed && store.remove(image).await.is_ok() {
                    budget.book().held.remove(&image.digest);
                }
                return Err(full.to_string());
            }
        };
        let Some(victim) = victims.first() else {
            return Ok(());
        };
        match store.remove(victim).await {
            Ok(()) => budget.gone(&victim.digest),
            Err(why) => {
                tracing::warn!(image = %victim, "the image budget could not evict it: {why}");
                stuck.insert(victim.digest.clone());
            }
        }
    }
}
