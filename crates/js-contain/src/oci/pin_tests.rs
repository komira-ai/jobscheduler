//! Unit tests of the pins of the image store ([`super::pin`]): two backends started on one
//! scratch directory, as two agents share it, each with its own collection. Where one
//! backend's step must fall between two steps of the other, the other's hook runs it there:
//! the order is fixed by the code, never by a wait.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::image::{FetchError, ImageSource, PinnedImage, ROOTFS};
use super::pin::PINS_DIR;
use super::tests::{config, scratch};
use super::*;

fn hexn(c: char) -> String {
    c.to_string().repeat(64)
}

fn digest(c: char) -> String {
    format!("sha256:{}", hexn(c))
}

fn pinned(c: char) -> PinnedImage {
    PinnedImage {
        reference: format!("registry.example/library/tool@{}", digest(c)),
        digest: digest(c),
    }
}

/// A source that counts its calls and fetches no layers.
#[derive(Debug, Default)]
struct Counting {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ImageSource for Counting {
    async fn fetch(&self, _: &PinnedImage, _: &Path) -> Result<Vec<String>, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

/// Two backends on one scratch directory: `a` started first (alone, so it swept), `b`
/// beside it.
struct Pair {
    a: Arc<Oci>,
    b: Arc<Oci>,
    scratch: PathBuf,
    images: PathBuf,
    // Last: removed after everything that runs in it.
    _dir: crate::test_dir::TestDir,
}

fn pair_with(name: &str, source: &Arc<Counting>) -> Pair {
    let dir = scratch(name);
    let scratch = dir.join("scratch");
    let start = || {
        let source: Arc<dyn ImageSource> = source.clone();
        Oci::on(&config(&dir, true), source, &scratch, "/actions", "x86_64").expect("starts")
    };
    let (a, b) = (start(), start());
    let images = scratch.join(image::IMAGES_DIR);
    std::fs::create_dir_all(&images).expect("images");
    Pair {
        a,
        b,
        scratch,
        images,
        _dir: dir,
    }
}

fn pair(name: &str) -> Pair {
    pair_with(name, &Arc::new(Counting::default()))
}

/// An unpacked image `<images>/<hex of c>/rootfs/usr/bin/tool`.
fn image(images: &Path, c: char) {
    let bin = images.join(hexn(c)).join(ROOTFS).join("usr/bin");
    std::fs::create_dir_all(&bin).expect("image");
    std::fs::write(bin.join("tool"), b"tool").expect("tool");
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The collection of `oci` with nothing named, as a backend that knows no lease does.
fn collect(oci: &Oci) -> GcReport {
    oci.collect_images(&[], &[]).expect("collected")
}

/// Catches the collection of one agent removing the image of another agent's lease on a
/// shared store: `b` pins `a`'s unlisted image X, `a`'s collection keeps X (and removes Y,
/// which nothing pins), and once the pin goes, the next collection removes X and its pin
/// file.
#[test]
fn a_lease_of_one_backend_keeps_its_image_from_the_collection_of_another() {
    let p = pair("pin-two-backends");
    image(&p.images, 'c');
    image(&p.images, 'd');
    let lease = p.b.pin_image(&pinned('c')).expect("pinned");
    let report = collect(&p.a);
    assert_eq!(report.removed, [digest('d')]);
    assert_eq!((report.kept, report.failed.len()), (1, 0), "{report:?}");
    assert!(report.kept_bytes > 0, "{report:?}");
    assert_eq!(names(&p.images), [hexn('c')]);
    assert!(
        p.images
            .join(hexn('c'))
            .join("rootfs/usr/bin/tool")
            .is_file()
    );
    drop(lease);
    let report = collect(&p.a);
    assert_eq!(report.removed, [digest('c')]);
    assert!(names(&p.images).is_empty());
    let pins = p.scratch.join(PINS_DIR);
    assert!(names(&pins).is_empty(), "{:?}", names(&pins));
}

/// Catches a pin that only the backend holding it honours, and a pin that is exclusive: two
/// backends pin X at once; the collection of the first keeps it while either pin is held
/// (its own included, with X on no list), and removes it once both have gone.
#[test]
fn an_image_pinned_by_two_backends_stays_until_both_pins_go() {
    let p = pair("pin-shared");
    image(&p.images, 'c');
    let (own, other) = (
        p.a.pin_image(&pinned('c')).expect("a pins"),
        p.b.pin_image(&pinned('c')).expect("b pins beside a"),
    );
    assert!(collect(&p.a).removed.is_empty());
    drop(own);
    let report = collect(&p.a);
    assert_eq!((report.removed.len(), report.kept), (0, 1), "{report:?}");
    drop(other);
    assert_eq!(collect(&p.a).removed, [digest('c')]);
}

/// Catches a collection that decides before it holds the lock: `b` pins X after `a`'s
/// collection opened X's pin file and before it locked it; X is kept, and `b`'s pin is live
/// (the next collection keeps X too) until it goes.
#[test]
fn a_pin_taken_between_the_collections_open_and_lock_keeps_the_image() {
    let p = pair("pin-before-claim");
    image(&p.images, 'c');
    let mut lease = None;
    let mut pin_now = || lease = Some(p.b.pin_image(&pinned('c')).expect("pinned"));
    let report =
        p.a.images
            .collect_with(&Default::default(), &mut |_| Ok(()), &mut pin_now)
            .expect("collected");
    assert_eq!((report.removed.len(), report.kept), (0, 1), "{report:?}");
    assert!(lease.is_some(), "the hook did not run");
    assert!(collect(&p.a).removed.is_empty());
    drop(lease);
    assert_eq!(collect(&p.a).removed, [digest('c')]);
}

/// Catches a pin that locks a pin file a collection has removed (a lock nobody else sees):
/// `a`'s whole collection of X runs after `b` opened X's pin file and before `b` locked it.
/// `b`'s pin is then on a new file: X, fetched again, is kept by the next collection, and
/// goes once the pin does.
#[test]
fn a_pin_whose_file_was_removed_before_its_lock_pins_the_file_that_replaced_it() {
    let p = pair("pin-after-claim");
    image(&p.images, 'c');
    let mut collected = None;
    let mut collect_now = || {
        if collected.is_none() {
            collected = Some(collect(&p.a));
        }
    };
    let lease =
        p.b.images
            .pin_with(&hexn('c'), &mut collect_now)
            .expect("pinned");
    let first = collected.expect("the hook ran");
    assert_eq!(first.removed, [digest('c')]);
    assert!(names(&p.images).is_empty(), "the image went");
    // The lease's check fetches X again (here: made in place).
    image(&p.images, 'c');
    let report = collect(&p.a);
    assert_eq!((report.removed.len(), report.kept), (0, 1), "{report:?}");
    drop(lease);
    assert_eq!(collect(&p.a).removed, [digest('c')]);
}

/// The same with the pin's lock taken while the claim may still be held: `a` holds the
/// claim of X; `b`, on a thread of its own, opens X's pin file and then locks it; `a` sets X
/// aside and retires the claim. The thread's signal proves only the open: `b`'s lock comes
/// either while the claim is held (and waits for it) or after it is retired, and both
/// orders must end the same way, with `b`'s pin on the file that is there now, which the
/// next collection cannot claim.
#[test]
fn a_pin_that_waited_for_a_removal_pins_the_file_that_replaced_it() {
    let p = pair("pin-waits");
    image(&p.images, 'c');
    let claim =
        p.a.images
            .claim(&hexn('c'), &mut || {})
            .expect("claimed")
            .expect("nothing pins it");
    let (opened_tx, opened) = std::sync::mpsc::channel();
    let b = Arc::clone(&p.b);
    let lease = std::thread::spawn(move || {
        let mut said = Some(opened_tx);
        let mut opened = || {
            if let Some(tx) = said.take() {
                tx.send(()).expect("the test waits");
            }
        };
        b.images.pin_with(&hexn('c'), &mut opened)
    });
    opened.recv().expect("b opened the pin file");
    p.a.images.set_aside(&hexn('c')).expect("set aside");
    claim.retire();
    let lease = lease.join().expect("joined").expect("pinned");
    image(&p.images, 'c');
    let report = collect(&p.a);
    assert_eq!((report.removed.len(), report.kept), (0, 1), "{report:?}");
    drop(lease);
    assert_eq!(collect(&p.a).removed, [digest('c')]);
}

/// Catches two collections on one store both removing X, or the second one reporting a
/// digest that is gone as one it could not remove: `b`'s collection lists X, then `a`'s
/// collection removes X between `b`'s open of X's pin file and its lock. `b` reports
/// nothing about X: not removed, not kept, not failed.
#[test]
fn a_digest_another_collection_removed_meanwhile_is_skipped() {
    let p = pair("pin-two-collections");
    image(&p.images, 'c');
    let mut collected = None;
    let mut collect_now = || {
        if collected.is_none() {
            collected = Some(collect(&p.a));
        }
    };
    let report =
        p.b.images
            .collect_with(&Default::default(), &mut |_| Ok(()), &mut collect_now)
            .expect("collected");
    assert_eq!(collected.expect("the hook ran").removed, [digest('c')]);
    assert!(
        report.removed.is_empty() && report.failed.is_empty() && report.kept == 0,
        "{report:?}"
    );
    assert_eq!((report.removed_bytes, report.kept_bytes), (0, 0));
    assert!(names(&p.images).is_empty());
    let pins = p.scratch.join(PINS_DIR);
    assert!(names(&pins).is_empty(), "{:?}", names(&pins));
}

/// Catches a removal of one image (`Oci::remove_image`, which a lease asks for when the
/// image has no kept config) taking the image of another backend's lease: while `b` pins X,
/// `a`'s removal is refused and says why, and X stays whole; once the pin goes, it removes X
/// and its pin file.
#[tokio::test]
async fn remove_image_refuses_an_image_another_backend_pins() {
    let p = pair("pin-remove-image");
    image(&p.images, 'c');
    let lease = p.b.pin_image(&pinned('c')).expect("pinned");
    let refused = p.a.remove_image(&pinned('c')).await.expect_err("pinned");
    assert_eq!(refused.kind(), std::io::ErrorKind::ResourceBusy);
    assert_eq!(
        refused.to_string(),
        format!("image {} is pinned by a lease on this store", pinned('c'))
    );
    assert!(
        p.images
            .join(hexn('c'))
            .join("rootfs/usr/bin/tool")
            .is_file()
    );
    drop(lease);
    p.a.remove_image(&pinned('c')).await.expect("removed");
    assert!(names(&p.images).is_empty());
    let pins = p.scratch.join(PINS_DIR);
    assert!(names(&pins).is_empty(), "{:?}", names(&pins));
}

/// Catches a pin file that is not one being followed or trusted: a link in its place makes
/// the pin fail (naming the path) and the collection keep the image and report it; a pin
/// directory that is a file fails the pin too.
#[test]
fn a_pin_file_that_cannot_be_used_fails_the_pin_and_keeps_the_image() {
    let p = pair("pin-refused");
    image(&p.images, 'c');
    let pins = p.scratch.join(PINS_DIR);
    std::fs::create_dir_all(&pins).expect("pins");
    let target = p.scratch.join("target");
    std::fs::write(&target, b"").expect("target");
    std::os::unix::fs::symlink(&target, pins.join(hexn('c'))).expect("link");
    let why =
        p.b.pin_image(&pinned('c'))
            .err()
            .expect("a link")
            .to_string();
    assert!(
        why.starts_with(&pins.join(hexn('c')).display().to_string()),
        "{why}"
    );
    let report = collect(&p.a);
    assert!(report.removed.is_empty(), "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(
        report.failed[0].starts_with(&format!("{}: pinning it: ", digest('c'))),
        "{report:?}"
    );
    assert!(report.kept_bytes > 0, "{report:?}");
    assert!(p.images.join(hexn('c')).join(ROOTFS).is_dir());
    std::fs::remove_file(pins.join(hexn('c'))).expect("unlink");
    std::fs::remove_dir(&pins).expect("rmdir");
    std::fs::write(&pins, b"").expect("a file in its place");
    assert!(
        p.b.pin_image(&pinned('c')).is_err(),
        "a pin directory that is a file"
    );
}

/// Catches a backend that takes an image for present because it fetched it once, after
/// another backend's collection removed it: the next check fetches it again.
#[tokio::test]
async fn an_image_another_backend_removed_is_fetched_again() {
    let source = Arc::new(Counting::default());
    let p = pair_with("pin-refetch", &source);
    p.a.check_pinned(&pinned('c')).await.expect("first use");
    // The stand-in unpack made no root filesystem; make it complete.
    std::fs::create_dir_all(p.images.join(hexn('c')).join(ROOTFS)).expect("rootfs");
    assert_eq!(collect(&p.b).removed, [digest('c')]);
    p.a.check_pinned(&pinned('c')).await.expect("second use");
    assert_eq!(
        source.calls.load(Ordering::SeqCst),
        2,
        "served from a stale flight"
    );
    assert!(p.images.join(hexn('c')).is_dir());
}
