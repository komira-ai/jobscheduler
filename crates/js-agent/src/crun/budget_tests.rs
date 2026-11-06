//! The image budget over a store held in memory: what a pull evicts, what it never evicts,
//! and what it does when the image cannot fit. Uses are counted, not timed: no clock is read
//! and nothing waits.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use js_contain::oci::image::PinnedImage;

use super::budget::Store;
use super::pull::Images;
use crate::lifecycle::{Prefetcher, Prepared};

/// The bytes each image of these tests takes.
const SIZE: u64 = 10;

/// The image named by the letter `c`.
fn image(c: char) -> PinnedImage {
    let digest = format!("sha256:{}", c.to_string().repeat(64));
    PinnedImage {
        reference: format!("registry.example/{c}@{digest}"),
        digest,
    }
}

fn digest(c: char) -> String {
    image(c).digest
}

/// What the fake store holds and what was done to it.
#[derive(Default)]
struct State {
    /// The images in the store, by digest, with their bytes.
    held: BTreeMap<String, u64>,
    /// Each fetch, by digest, in order.
    fetched: Vec<String>,
    /// Each removal that was done, by digest, in order.
    removed: Vec<String>,
}

/// A store in memory. An image fetched takes [`SIZE`] bytes, or what `sizes` says.
#[derive(Default)]
struct Fake {
    state: Mutex<State>,
    sizes: BTreeMap<String, u64>,
    /// Digests whose removal is refused, as a lease of another agent's refuses it.
    refused: BTreeSet<String>,
    /// Digests whose fetch fails.
    unfetchable: BTreeSet<String>,
    /// The store cannot be listed.
    unlisted: bool,
    /// No image can be measured.
    unmeasured: bool,
}

impl Fake {
    fn with_held(held: &[(char, u64)]) -> Self {
        let fake = Self::default();
        fake.state.lock().unwrap().held = held.iter().map(|(c, n)| (digest(*c), *n)).collect();
        fake
    }

    fn held(&self) -> Vec<String> {
        self.state.lock().unwrap().held.keys().cloned().collect()
    }

    fn removed(&self) -> Vec<String> {
        self.state.lock().unwrap().removed.clone()
    }

    fn fetched(&self) -> Vec<String> {
        self.state.lock().unwrap().fetched.clone()
    }
}

#[async_trait]
impl Store for Fake {
    async fn ensure(&self, image: &PinnedImage) -> Result<(), String> {
        if self.unfetchable.contains(&image.digest) {
            return Err(format!("no registry for {image}"));
        }
        let mut state = self.state.lock().unwrap();
        if !state.held.contains_key(&image.digest) {
            let bytes = self.sizes.get(&image.digest).copied().unwrap_or(SIZE);
            state.held.insert(image.digest.clone(), bytes);
            state.fetched.push(image.digest.clone());
        }
        Ok(())
    }

    fn bytes(&self, image: &PinnedImage) -> io::Result<u64> {
        if self.unmeasured {
            return Err(io::Error::other("unreadable"));
        }
        let state = self.state.lock().unwrap();
        Ok(state.held.get(&image.digest).copied().unwrap_or(0))
    }

    fn held(&self) -> io::Result<Vec<(String, u64)>> {
        if self.unlisted {
            return Err(io::Error::other("unlistable"));
        }
        let state = self.state.lock().unwrap();
        Ok(state.held.iter().map(|(d, n)| (d.clone(), *n)).collect())
    }

    async fn remove(&self, image: &PinnedImage) -> io::Result<()> {
        if self.refused.contains(&image.digest) {
            return Err(io::Error::new(io::ErrorKind::ResourceBusy, "pinned"));
        }
        let mut state = self.state.lock().unwrap();
        state.held.remove(&image.digest);
        state.removed.push(image.digest.clone());
        Ok(())
    }
}

/// The images of `fake` held to `budget` bytes, never evicting `keep`, and the digests told
/// of each eviction, in order.
fn images(fake: &Arc<Fake>, budget: u64, keep: &[char]) -> (Images, Arc<Mutex<Vec<String>>>) {
    let keep = keep.iter().map(|c| digest(*c)).collect();
    let store: Arc<dyn Store> = Arc::clone(fake) as Arc<dyn Store>;
    let images = Images::new(store, Some(budget), keep);
    let told = Arc::new(Mutex::new(Vec::new()));
    let noted = Arc::clone(&told);
    images.on_evict(Box::new(move |digest| {
        noted.lock().unwrap().push(digest.to_owned())
    }));
    (images, told)
}

fn block<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

fn pull(images: &Images, c: char) -> Result<(), String> {
    block(images.ensure(&image(c)))
}

/// A lease that takes the image `c` and lets it go: one use of it.
fn using(images: &Images, c: char) {
    images.lease("js-host-use", &digest(c));
    images.release("js-host-use");
}

/// Proves the least recently used image is the one evicted: with room for two, A and B are
/// pulled, A is used, and the pull of C evicts B (the older use), keeping A and C. Catches an
/// eviction in the order of pulls rather than of uses, and one that evicts more than it must.
#[test]
fn a_pull_over_the_budget_evicts_the_image_used_least_recently() {
    let fake = Arc::new(Fake::default());
    let (images, told) = images(&fake, 2 * SIZE, &[]);
    pull(&images, 'a').unwrap();
    pull(&images, 'b').unwrap();
    using(&images, 'a');
    // A lease name that holds nothing lets nothing go: A stays the latest use.
    images.release("js-host-none");
    pull(&images, 'c').unwrap();
    assert_eq!(fake.held(), [digest('a'), digest('c')]);
    assert_eq!(fake.removed(), [digest('b')]);
    assert_eq!(*told.lock().unwrap(), [digest('b')]);
    assert_eq!(images.counted(), 2 * SIZE);
}

/// Proves a kept image is never evicted though it is the one used least recently: A is kept
/// and oldest, and the pull of C evicts B, the next oldest. Catches a budget that ignores the
/// images it was told to keep.
#[test]
fn a_kept_image_is_not_evicted_though_it_is_the_oldest() {
    let fake = Arc::new(Fake::default());
    let (images, told) = images(&fake, 2 * SIZE, &['a']);
    pull(&images, 'a').unwrap();
    pull(&images, 'b').unwrap();
    pull(&images, 'c').unwrap();
    assert_eq!(fake.held(), [digest('a'), digest('c')]);
    assert_eq!(*told.lock().unwrap(), [digest('b')]);
}

/// Proves a leased image is never evicted, and that when only leased and kept images are
/// left the pull fails with the no-capacity error, word for word, and the store holds just
/// what it held: the image the pull brought is removed again, nothing else is, and nobody is
/// told of an eviction. Then, the lease let go, the same pull fits by evicting B. Catches a
/// budget that is silently exceeded, a pull that evicts some images and then fails anyway,
/// and a lease that keeps its image after it ended.
#[test]
fn a_pull_that_cannot_fit_beside_leased_and_kept_images_fails_and_changes_nothing() {
    let fake = Arc::new(Fake::default());
    let (images, told) = images(&fake, 2 * SIZE, &['a']);
    pull(&images, 'a').unwrap();
    pull(&images, 'b').unwrap();
    images.lease("js-host-r1", &digest('b'));
    let refused = pull(&images, 'c').unwrap_err();
    assert_eq!(
        refused,
        format!(
            "no-capacity: image {} takes 10 bytes, and the image budget of 20 bytes holds 20 \
             bytes of images that a run holds, that are kept or that the store refused to remove",
            digest('c')
        )
    );
    assert_eq!(fake.held(), [digest('a'), digest('b')]);
    assert_eq!(fake.removed(), [digest('c')]);
    assert!(told.lock().unwrap().is_empty());
    assert_eq!(images.counted(), 2 * SIZE);
    images.release("js-host-r1");
    pull(&images, 'c').unwrap();
    assert_eq!(fake.held(), [digest('a'), digest('c')]);
    assert_eq!(*told.lock().unwrap(), [digest('b')]);
}

/// Proves an image larger than the whole budget fails with nothing else in the store to
/// evict, and is not kept: the boundary where the incoming image alone is over.
#[test]
fn an_image_larger_than_the_budget_fails_alone() {
    let fake = Arc::new(Fake {
        sizes: [(digest('a'), 3 * SIZE)].into(),
        ..Fake::default()
    });
    let (images, _) = images(&fake, 2 * SIZE, &[]);
    let refused = pull(&images, 'a').unwrap_err();
    assert!(refused.starts_with("no-capacity: "), "{refused}");
    assert!(fake.held().is_empty());
    assert_eq!(images.counted(), 0);
}

/// Proves an image the budget evicted is pulled again when it is asked for, and that the
/// pull evicts the image then used least recently: after B goes for C, B is fetched again
/// and A (used before C was pulled) goes. And that an image exactly at the budget fits: no
/// eviction when the store holds exactly the budget.
#[test]
fn an_evicted_image_is_pulled_again_and_evicts_the_then_oldest() {
    let fake = Arc::new(Fake::default());
    let (images, told) = images(&fake, 2 * SIZE, &[]);
    pull(&images, 'a').unwrap();
    pull(&images, 'b').unwrap();
    assert!(
        told.lock().unwrap().is_empty(),
        "at the budget, nothing goes"
    );
    using(&images, 'a');
    pull(&images, 'c').unwrap();
    pull(&images, 'b').unwrap();
    assert_eq!(
        fake.fetched(),
        [digest('a'), digest('b'), digest('c'), digest('b')]
    );
    assert_eq!(fake.held(), [digest('b'), digest('c')]);
    assert_eq!(*told.lock().unwrap(), [digest('b'), digest('a')]);
}

/// Proves each eviction is told exactly once, by digest, and only evictions are: a pull that
/// evicts two images to fit tells each once, and pulls that evict nothing tell nothing.
#[test]
fn each_eviction_is_told_once() {
    let fake = Arc::new(Fake {
        sizes: [(digest('c'), 2 * SIZE)].into(),
        ..Fake::default()
    });
    let (images, told) = images(&fake, 2 * SIZE, &[]);
    pull(&images, 'a').unwrap();
    pull(&images, 'b').unwrap();
    pull(&images, 'a').unwrap();
    assert!(told.lock().unwrap().is_empty());
    pull(&images, 'c').unwrap();
    assert_eq!(*told.lock().unwrap(), [digest('b'), digest('a')]);
    assert_eq!(fake.held(), [digest('c')]);
}

/// Proves a pull of an image already in the store counts as a use and fetches nothing
/// again: A, pulled again after B, is younger than B, so the pull of C evicts B.
#[test]
fn a_pull_of_an_image_held_is_a_use() {
    let fake = Arc::new(Fake::default());
    let (images, told) = images(&fake, 2 * SIZE, &[]);
    pull(&images, 'a').unwrap();
    pull(&images, 'b').unwrap();
    pull(&images, 'a').unwrap();
    pull(&images, 'c').unwrap();
    assert_eq!(*told.lock().unwrap(), [digest('b')]);
    assert_eq!(fake.fetched(), [digest('a'), digest('b'), digest('c')]);
}

/// Proves the images the store held before the first pull are counted, oldest first in the
/// order of their digests: with A and B in the store and room for two, the first pull (of
/// C) evicts A.
#[test]
fn the_images_held_at_the_start_are_counted_and_oldest() {
    let fake = Arc::new(Fake::with_held(&[('b', SIZE), ('a', SIZE)]));
    let (images, told) = images(&fake, 2 * SIZE, &[]);
    pull(&images, 'c').unwrap();
    assert_eq!(*told.lock().unwrap(), [digest('a')]);
    assert_eq!(fake.held(), [digest('b'), digest('c')]);
}

/// Proves a pull of an image the store held already never fails for the budget, as it adds
/// no bytes: alone over the budget it stays and the pull succeeds; beside images that may
/// not go, the store already over and unable to get within it, it succeeds and evicts
/// nothing; and once evicting the one image that may go brings the store within budget, it
/// evicts that. Catches a held image refused because others are leased.
#[test]
fn a_pull_of_an_image_held_already_does_not_fail_for_the_budget() {
    let fake = Arc::new(Fake::with_held(&[('a', 3 * SIZE)]));
    let (images, _) = images(&fake, 2 * SIZE, &[]);
    pull(&images, 'a').unwrap();
    assert_eq!(fake.held(), [digest('a')]);
    assert!(fake.removed().is_empty());
    let fake = Arc::new(Fake::with_held(&[('a', SIZE), ('b', SIZE), ('c', SIZE)]));
    let (images, told) = self::images(&fake, SIZE, &['a']);
    images.lease("js-host-r1", &digest('b'));
    pull(&images, 'c').unwrap();
    assert!(fake.removed().is_empty());
    assert!(told.lock().unwrap().is_empty());
    // Only a is kept now, and the lease moves from b to c: b's eviction brings the store
    // to 20 bytes, still over 10, so again nothing goes; with a let go too, b goes.
    images.lease("js-host-r1", &digest('c'));
    pull(&images, 'c').unwrap();
    assert!(told.lock().unwrap().is_empty());
    let fake = Arc::new(Fake::with_held(&[('b', SIZE), ('c', SIZE)]));
    let (images, told) = self::images(&fake, SIZE, &[]);
    images.lease("js-host-r1", &digest('c'));
    pull(&images, 'c').unwrap();
    assert_eq!(*told.lock().unwrap(), [digest('b')]);
    assert_eq!(fake.held(), [digest('c')]);
}

/// Proves a pull can fail after it evicted: when an image chosen later is refused by the
/// store (a lease of another agent took it meanwhile), the images already evicted stay
/// evicted and told, the pull's own image is removed again, and the error counts the
/// refused image among those that could not go. Catches a half-done eviction that is not
/// reported, and one that leaves the pull's image behind.
#[test]
fn a_pull_that_evicts_and_then_meets_a_refusal_fails() {
    let fake = Arc::new(Fake {
        refused: [digest('a')].into(),
        sizes: [(digest('c'), 2 * SIZE)].into(),
        ..Fake::default()
    });
    let (images, told) = images(&fake, 2 * SIZE, &[]);
    pull(&images, 'b').unwrap();
    pull(&images, 'a').unwrap();
    assert_eq!(
        pull(&images, 'c').unwrap_err(),
        format!(
            "no-capacity: image {} takes 20 bytes, and the image budget of 20 bytes holds 10 \
             bytes of images that a run holds, that are kept or that the store refused to remove",
            digest('c')
        )
    );
    assert_eq!(*told.lock().unwrap(), [digest('b')]);
    assert_eq!(fake.held(), [digest('a')]);
    assert_eq!(fake.removed(), [digest('b'), digest('c')]);
    assert_eq!(images.counted(), SIZE);
}

/// Proves the image of a lease that cannot fit stays in the store: a run's start fetched it
/// under the run's lease, which pins it, so it is not taken from under the run.
#[test]
fn a_leased_image_that_cannot_fit_stays() {
    let fake = Arc::new(Fake {
        sizes: [(digest('a'), 3 * SIZE)].into(),
        ..Fake::default()
    });
    let (images, _) = images(&fake, 2 * SIZE, &[]);
    images.lease("js-host-r1", &digest('a'));
    assert!(pull(&images, 'a').unwrap_err().starts_with("no-capacity: "));
    assert_eq!(fake.held(), [digest('a')]);
    assert_eq!(images.counted(), 3 * SIZE);
}

/// Proves an image the store refuses to remove (a lease of another agent on the store pins
/// it) is passed over and the next oldest evicted instead, and that the image the pull
/// brought stays counted when its own removal is refused. Catches a refusal that fails the
/// pull while another image could go, and a count that forgets an image still there.
#[test]
fn an_image_whose_removal_is_refused_is_passed_over() {
    let fake = Arc::new(Fake {
        refused: [digest('a'), digest('d')].into(),
        ..Fake::default()
    });
    let (images, told) = images(&fake, 2 * SIZE, &[]);
    pull(&images, 'a').unwrap();
    pull(&images, 'b').unwrap();
    pull(&images, 'c').unwrap();
    assert_eq!(*told.lock().unwrap(), [digest('b')]);
    assert_eq!(fake.held(), [digest('a'), digest('c')]);
    // Only A and C may be evicted, A's removal is refused, and so is D's own: D stays.
    images.lease("js-host-r1", &digest('c'));
    assert!(pull(&images, 'd').unwrap_err().starts_with("no-capacity: "));
    assert_eq!(fake.held(), [digest('a'), digest('c'), digest('d')]);
    assert_eq!(images.counted(), 3 * SIZE);
}

/// Proves the failures of the store are the pull's, in words that name the step: a fetch
/// that fails counts nothing, a store that cannot be listed or an image that cannot be
/// measured fails the pull.
#[test]
fn the_failures_of_the_store_fail_the_pull() {
    let fake = Arc::new(Fake {
        unfetchable: [digest('a')].into(),
        ..Fake::default()
    });
    let (images, _) = images(&fake, 2 * SIZE, &[]);
    assert_eq!(
        pull(&images, 'a').unwrap_err(),
        format!("no registry for {}", image('a'))
    );
    assert_eq!(images.counted(), 0);
    let unlisted = Arc::new(Fake {
        unlisted: true,
        ..Fake::default()
    });
    let (images, _) = self::images(&unlisted, 2 * SIZE, &[]);
    assert_eq!(
        pull(&images, 'a').unwrap_err(),
        "the image budget: reading the image store: unlistable"
    );
    let unmeasured = Arc::new(Fake {
        unmeasured: true,
        ..Fake::default()
    });
    let (images, _) = self::images(&unmeasured, 2 * SIZE, &[]);
    assert_eq!(
        pull(&images, 'a').unwrap_err(),
        format!("the image budget: measuring {}: unreadable", image('a'))
    );
}

/// Proves an eviction with nobody to tell is still done, and that without a budget nothing
/// is counted, measured or evicted, and leases are nothing.
#[test]
fn without_a_listener_or_a_budget() {
    let fake = Arc::new(Fake::default());
    let store: Arc<dyn Store> = Arc::clone(&fake) as Arc<dyn Store>;
    let quiet = Images::new(Arc::clone(&store), Some(SIZE), BTreeSet::new());
    pull(&quiet, 'a').unwrap();
    pull(&quiet, 'b').unwrap();
    assert_eq!(fake.removed(), [digest('a')]);
    let open = Images::new(store, None, BTreeSet::new());
    open.on_evict(Box::new(|_| panic!("nothing is evicted")));
    open.lease("js-host-r1", &digest('c'));
    open.release("js-host-r1");
    open.release("js-host-none");
    pull(&open, 'c').unwrap();
    pull(&open, 'd').unwrap();
    assert_eq!(open.counted(), 0);
    assert_eq!(fake.held(), [digest('b'), digest('c'), digest('d')]);
}

/// Proves the prefetcher is told of an eviction through the runtime's pulls: an image the
/// budget evicted is no longer answered `Ready`, and asking for it pulls it again. The pull
/// function is the one the runtime gives its prefetcher; an unpinned reference fails it.
#[test]
fn the_prefetcher_forgets_an_evicted_image() {
    let fake = Arc::new(Fake::default());
    let store: Arc<dyn Store> = Arc::clone(&fake) as Arc<dyn Store>;
    let images = Arc::new(Images::new(store, Some(2 * SIZE), BTreeSet::new()));
    let pulling = Arc::clone(&images);
    let prefetcher = Prefetcher::new(1, move |image| pulling.pull(image)).unwrap();
    let forgetter = prefetcher.forgetter();
    images.on_evict(Box::new(move |digest| forgetter.forget_digest(digest)));
    let (a, b, c) = (
        image('a').reference,
        image('b').reference,
        image('c').reference,
    );
    assert_eq!(prefetcher.wait(&a), Prepared::Ready);
    assert_eq!(prefetcher.wait(&b), Prepared::Ready);
    assert_eq!(prefetcher.wait(&c), Prepared::Ready);
    assert_eq!(fake.held(), [digest('b'), digest('c')]);
    assert!(matches!(prefetcher.prepare(&b, None), Prepared::Ready));
    assert!(matches!(prefetcher.prepare(&a, None), Prepared::Pending(_)));
    assert_eq!(prefetcher.wait(&a), Prepared::Ready);
    assert_eq!(
        fake.fetched().iter().filter(|d| **d == digest('a')).count(),
        2
    );
    assert!(matches!(
        prefetcher.wait("registry.example/a:latest"),
        Prepared::Failed(_)
    ));
}
