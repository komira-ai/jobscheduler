//! Unit tests of the pins of the image store ([`super::pin`]): two backends started on one
//! scratch directory, as two agents share it, each with its own collection. Where one
//! backend's step must fall between two steps of the other, the other's hook runs it there:
//! the order is fixed by the code, never by a wait. A fetch that must be in flight is held
//! by its source until the test lets it go.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use super::image::{FetchError, ImageSource, PinnedImage, ROOTFS};
use super::pin::{LEASES_DIR, PINS_DIR};
use super::tests::{config, scratch};
use super::*;

pub(super) fn hexn(c: char) -> String {
    c.to_string().repeat(64)
}

pub(super) fn digest(c: char) -> String {
    format!("sha256:{}", hexn(c))
}

pub(super) fn pinned(c: char) -> PinnedImage {
    PinnedImage {
        reference: format!("registry.example/library/tool@{}", digest(c)),
        digest: digest(c),
    }
}

/// What one fetch of a [`Steps`] source does.
#[derive(Debug)]
pub(super) enum Step {
    /// Fails.
    Fail,
    /// Says it began, then waits to be let go, then fetches.
    Hold {
        began: oneshot::Sender<()>,
        go: oneshot::Receiver<()>,
    },
}

/// A source that counts its fetches and fetches no layers, each fetch doing the next of its
/// steps (and succeeding at once when there are none left).
#[derive(Debug, Default)]
pub(super) struct Steps {
    pub(super) calls: AtomicUsize,
    next: Mutex<VecDeque<Step>>,
}

impl Steps {
    pub(super) fn then(&self, step: Step) {
        self.next.lock().expect("lock").push_back(step);
    }
}

#[async_trait::async_trait]
impl ImageSource for Steps {
    async fn fetch(&self, _: &PinnedImage, _: &Path) -> Result<Vec<String>, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let step = self.next.lock().expect("lock").pop_front();
        match step {
            None => Ok(Vec::new()),
            Some(Step::Fail) => Err(FetchError::Failed("refused".to_owned())),
            Some(Step::Hold { began, go }) => {
                let _ = began.send(());
                let _ = go.await;
                Ok(Vec::new())
            }
        }
    }
}

/// Two backends on one scratch directory: `a` started first (alone, so it swept), `b`
/// beside it.
pub(super) struct Pair {
    pub(super) a: Arc<Oci>,
    pub(super) b: Arc<Oci>,
    pub(super) scratch: PathBuf,
    pub(super) images: PathBuf,
    pub(super) pins: PathBuf,
    // Last: removed after everything that runs in it.
    pub(super) dir: crate::test_dir::TestDir,
}

pub(super) fn pair_with(name: &str, source: &Arc<Steps>) -> Pair {
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
        pins: scratch.join(PINS_DIR),
        scratch,
        images,
        dir,
    }
}

pub(super) fn pair(name: &str) -> Pair {
    pair_with(name, &Arc::new(Steps::default()))
}

/// An unpacked image `<images>/<hex of c>/rootfs/usr/bin/tool`.
pub(super) fn image(images: &Path, c: char) {
    let bin = images.join(hexn(c)).join(ROOTFS).join("usr/bin");
    std::fs::create_dir_all(&bin).expect("image");
    std::fs::write(bin.join("tool"), b"tool").expect("tool");
}

/// Whether the image `c` is there whole.
pub(super) fn whole(images: &Path, c: char) -> bool {
    images.join(hexn(c)).join("rootfs/usr/bin/tool").is_file()
}

pub(super) fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The collection of `oci` with nothing named, as a backend that knows no lease does.
pub(super) fn collect(oci: &Oci) -> GcReport {
    oci.collect_images(&[], &[]).expect("collected")
}

/// A report that removed nothing, failed at nothing and kept `kept` digests.
pub(super) fn kept(report: &GcReport, kept: usize) {
    assert!(
        report.removed.is_empty() && report.failed.is_empty() && report.kept == kept,
        "{report:?}"
    );
}

/// The pins' directory holds no lock file and no lease's pin.
pub(super) fn no_pins(p: &Pair) {
    assert_eq!(names(&p.pins), [LEASES_DIR], "{:?}", names(&p.pins));
    assert!(names(&p.pins.join(LEASES_DIR)).is_empty());
}

/// The issue's case. Catches the collection of one agent removing the image of another
/// agent's lease on a shared store: `b`'s lease pins X, which `a` knows nothing of; `a`'s
/// collection keeps X whole and removes Y, which nothing pins; once `b` unpins, the next
/// collection removes X and its lock file.
#[test]
fn a_lease_of_one_backend_keeps_its_image_from_the_collection_of_another() {
    let p = pair("pin-two-backends");
    image(&p.images, 'c');
    image(&p.images, 'd');
    p.b.pin_image(&pinned('c'), "lease-b").expect("pinned");
    let report = collect(&p.a);
    assert_eq!(report.removed, [digest('d')]);
    assert_eq!((report.kept, report.failed.len()), (1, 0), "{report:?}");
    assert!(report.kept_bytes > 0, "{report:?}");
    assert_eq!(names(&p.images), [hexn('c')]);
    assert!(whole(&p.images, 'c'));
    p.b.unpin_image("lease-b").expect("unpinned");
    assert_eq!(collect(&p.a).removed, [digest('c')]);
    assert!(names(&p.images).is_empty());
    no_pins(&p);
}

/// Catches a pin that only the backend holding it honours, one lease's unpin taking another
/// lease's pin, and a second unpin failing: X, pinned by a lease of each backend, stays
/// through either unpin and goes after both.
#[test]
fn an_image_pinned_by_two_leases_stays_until_both_are_unpinned() {
    let p = pair("pin-shared");
    image(&p.images, 'c');
    p.a.pin_image(&pinned('c'), "lease-a").expect("a pins");
    p.b.pin_image(&pinned('c'), "lease-b")
        .expect("b pins beside a");
    kept(&collect(&p.a), 1);
    p.a.unpin_image("lease-a").expect("unpinned");
    p.a.unpin_image("lease-a").expect("nothing left to unpin");
    kept(&collect(&p.a), 1);
    p.b.unpin_image("lease-b").expect("unpinned");
    assert_eq!(collect(&p.a).removed, [digest('c')]);
}

/// The environment variable naming the directory [`pin_child`] works in.
const CHILD_DIR: &str = "JS_CONTAIN_PIN_CHILD_DIR";

/// Run as a process of its own by
/// [`a_pin_outlives_the_process_that_wrote_it`]: starts a backend on the parent's scratch
/// directory, pins C for the lease `child-lease`, and exits without unpinning and without
/// running a destructor, as an agent killed with its container still running does.
#[test]
#[ignore = "a process of its own of a_pin_outlives_the_process_that_wrote_it"]
fn pin_child() {
    let dir = PathBuf::from(std::env::var_os(CHILD_DIR).expect("run by its parent test"));
    let source: Arc<dyn ImageSource> = Arc::new(Steps::default());
    let scratch = dir.join("scratch");
    let oci = Oci::on(&config(&dir, true), source, &scratch, "/actions", "x86_64");
    let pinned = oci.expect("starts").pin_image(&pinned('c'), "child-lease");
    pinned.expect("pinned");
    std::process::exit(0);
}

/// Catches a pin that ends with its process: the lease's container may still run after its
/// agent died, until the next start of that agent sweeps the lease. A process of its own
/// pins C and exits; the collection of another backend keeps C, and removes it once the
/// lease is unpinned (as that sweep does).
#[test]
fn a_pin_outlives_the_process_that_wrote_it() {
    let p = pair("pin-child");
    image(&p.images, 'c');
    let exe = std::env::current_exe().expect("the test binary");
    let ran = std::process::Command::new(exe)
        .args(["--exact", "oci::pin_tests::pin_child", "--ignored"])
        .env(CHILD_DIR, p.dir.as_os_str())
        .output()
        .expect("the child ran");
    assert!(ran.status.success(), "{ran:?}");
    assert_eq!(names(&p.pins.join(LEASES_DIR)), ["child-lease"]);
    kept(&collect(&p.a), 1);
    assert!(whole(&p.images, 'c'));
    p.a.unpin_image("child-lease").expect("swept");
    assert_eq!(collect(&p.a).removed, [digest('c')]);
}

/// Catches a collection that decides before it holds the lock: `b` pins X after `a`'s
/// collection opened X's lock file and before it locked it; X is kept, and stays kept until
/// `b` unpins.
#[test]
fn a_pin_written_between_the_collections_open_and_lock_keeps_the_image() {
    let p = pair("pin-before-claim");
    image(&p.images, 'c');
    let mut pinned_now = false;
    let mut pin_now = || {
        p.b.pin_image(&pinned('c'), "lease-b").expect("pinned");
        pinned_now = true;
    };
    let report =
        p.a.images
            .collect_with(&Default::default(), &mut |_| Ok(()), &mut pin_now)
            .expect("collected");
    kept(&report, 1);
    assert!(pinned_now, "the hook did not run");
    kept(&collect(&p.a), 1);
    p.b.unpin_image("lease-b").expect("unpinned");
    assert_eq!(collect(&p.a).removed, [digest('c')]);
}

/// Catches a pin written under a lock file a collection has removed (a lock nobody else
/// sees), and the lease's check not fetching again, or a fetch that the pin does not protect:
/// `a`'s whole collection of X runs after `b` opened X's lock file and before `b` locked it.
/// `b`'s check then fetches X again; `a`'s collection while that fetch is held keeps
/// everything of it, and the one after keeps X; X goes once `b` unpins.
#[tokio::test]
async fn a_pin_after_a_removal_protects_the_image_its_check_fetches_again() {
    let source = Arc::new(Steps::default());
    let p = pair_with("pin-after-claim", &source);
    image(&p.images, 'c');
    let mut collected = None;
    let mut collect_now = || {
        if collected.is_none() {
            collected = Some(collect(&p.a));
        }
    };
    p.b.images
        .pin_with(&hexn('c'), "lease-b", &mut collect_now)
        .expect("pinned");
    assert_eq!(collected.expect("the hook ran").removed, [digest('c')]);
    assert!(names(&p.images).is_empty(), "the image went");
    let (began_tx, began) = oneshot::channel();
    let (go_tx, go) = oneshot::channel();
    source.then(Step::Hold {
        began: began_tx,
        go,
    });
    let b = Arc::clone(&p.b);
    let fetch = tokio::spawn(async move { b.check_pinned(&pinned('c')).await });
    began.await.expect("the fetch began");
    let during = names(&p.images);
    kept(&collect(&p.a), 0);
    assert_eq!(names(&p.images), during, "the fetch in flight was touched");
    go_tx.send(()).expect("the fetch waits");
    fetch.await.expect("joined").expect("fetched again");
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    kept(&collect(&p.a), 1);
    assert_eq!(names(&p.images), [hexn('c')]);
    p.b.unpin_image("lease-b").expect("unpinned");
    assert_eq!(collect(&p.a).removed, [digest('c')]);
}

/// Catches a fetch that fails under a pin losing the pin, or a retry that is not protected:
/// `b` pins X, which is not stored; its first fetch fails, a collection meanwhile changes
/// nothing, the retry fetches X, and the next collection keeps it.
#[tokio::test]
async fn a_failed_fetch_under_a_pin_is_retried_and_its_image_kept() {
    let source = Arc::new(Steps::default());
    let p = pair_with("pin-refetch-fails", &source);
    p.b.pin_image(&pinned('c'), "lease-b").expect("pinned");
    source.then(Step::Fail);
    let failed = p.b.check_pinned(&pinned('c')).await.expect_err("refused");
    assert_eq!(failed.to_string(), "refused");
    kept(&collect(&p.a), 0);
    p.b.check_pinned(&pinned('c')).await.expect("fetched");
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    kept(&collect(&p.a), 1);
    assert_eq!(names(&p.images), [hexn('c')]);
}

/// The lock-file race with the pin's lock taken on a thread while the claim may still be
/// held: `a` holds the claim of X; `b`, on a thread, opens X's lock file and then locks it;
/// `a` sets X aside and retires the claim. The thread's signal proves only the open: `b`'s
/// lock comes either while the claim is held (and waits for it) or after it is retired, and
/// both orders must end the same way: `b`'s pin protects X once it is there again.
#[test]
fn a_pin_that_may_wait_for_a_removal_protects_the_image_after_it() {
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
        b.images.pin_with(&hexn('c'), "lease-b", &mut opened)
    });
    opened.recv().expect("b opened the lock file");
    p.a.images.set_aside(&hexn('c')).expect("set aside");
    claim.retire();
    lease.join().expect("joined").expect("pinned");
    image(&p.images, 'c');
    kept(&collect(&p.a), 1);
    p.b.unpin_image("lease-b").expect("unpinned");
    assert_eq!(collect(&p.a).removed, [digest('c')]);
}

/// Catches two collections on one store both removing X, the second reporting a digest that
/// is gone as one it could not remove, or leaving a lock file behind: `b`'s collection lists
/// X, then `a`'s collection removes X between `b`'s open of X's lock file and its lock. `b`
/// reports nothing about X.
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
    kept(&report, 0);
    assert_eq!((report.removed_bytes, report.kept_bytes), (0, 0));
    assert!(names(&p.images).is_empty());
    no_pins(&p);
}

/// Catches a removal that takes a digest another removal holds: while `a` holds X's claim,
/// `b`'s collection keeps X and `b`'s removal of X is refused as busy; once the claim is let
/// go (X not set aside), `b`'s collection removes X.
#[tokio::test]
async fn a_claim_held_by_one_removal_keeps_the_digest_from_another() {
    let p = pair("pin-claimed");
    image(&p.images, 'c');
    let claim = p.a.images.claim(&hexn('c'), &mut || {});
    let claim = claim.expect("claimed").expect("nothing pins it");
    kept(&collect(&p.b), 1);
    let busy = p.b.remove_image(&pinned('c')).await.expect_err("claimed");
    assert_eq!(busy.kind(), std::io::ErrorKind::ResourceBusy);
    assert!(whole(&p.images, 'c'));
    drop(claim);
    assert_eq!(collect(&p.b).removed, [digest('c')]);
}

/// Catches the slow removal of a tree set aside reaching the image fetched again in its
/// place, or holding the lock while it runs: while `a`'s collection removes the old tree of
/// X, `b` pins X (not waiting: the claim is let go) and X is unpacked again; the new X stays
/// whole and the next collection keeps it.
#[test]
fn a_pin_and_fetch_during_the_removal_of_the_old_tree_keep_the_new_image() {
    let p = pair("pin-slow-removal");
    image(&p.images, 'c');
    let mut during = |aside: &Path| {
        p.b.pin_image(&pinned('c'), "lease-b").expect("not waiting");
        image(&p.images, 'c');
        crate::remove::remove_tree(aside).map_err(|e| e.to_string())
    };
    let report =
        p.a.images
            .collect_with(&Default::default(), &mut during, &mut || {})
            .expect("collected");
    assert_eq!(report.removed, [digest('c')]);
    assert!(whole(&p.images, 'c'));
    kept(&collect(&p.a), 1);
    p.b.unpin_image("lease-b").expect("unpinned");
    assert_eq!(collect(&p.a).removed, [digest('c')]);
}

/// Catches a lock file that could not be removed after a removal making later pins or
/// removals unsafe: the pins' directory is read-only while `a` removes X, so X's lock file
/// stays; `b` then pins X through that same file, X is fetched again and kept, and goes
/// once unpinned. Assumes a non-root user (root ignores the mode).
#[test]
fn a_lock_file_that_could_not_be_removed_still_serves_pins_and_removals() {
    use std::os::unix::fs::PermissionsExt as _;
    let p = pair("pin-unlink-fails");
    image(&p.images, 'c');
    // X's lock file, made by an earlier lease of X (a read-only directory makes none).
    p.b.pin_image(&pinned('c'), "earlier").expect("pinned");
    p.b.unpin_image("earlier").expect("unpinned");
    let mode = |m| std::fs::Permissions::from_mode(m);
    std::fs::set_permissions(&p.pins, mode(0o500)).expect("read-only");
    assert_eq!(collect(&p.a).removed, [digest('c')]);
    assert_eq!(names(&p.pins), [hexn('c'), LEASES_DIR.to_owned()]);
    p.b.pin_image(&pinned('c'), "lease-b").expect("pinned");
    image(&p.images, 'c');
    kept(&collect(&p.a), 1);
    p.b.unpin_image("lease-b").expect("unpinned");
    assert_eq!(collect(&p.a).removed, [digest('c')]);
    std::fs::set_permissions(&p.pins, mode(0o700)).expect("back");
}

/// Catches the issue's other path: a removal of one image (`Oci::remove_image`, which a
/// lease asks for when the image has no kept config) taking the image of another backend's
/// lease. While `b` pins X, `a`'s removal is refused and says why, and X stays whole; once
/// `b` unpins, it removes X and its lock file.
#[tokio::test]
async fn remove_image_refuses_an_image_another_backend_pins() {
    let p = pair("pin-remove-image");
    image(&p.images, 'c');
    p.b.pin_image(&pinned('c'), "lease-b").expect("pinned");
    let refused = p.a.remove_image(&pinned('c')).await.expect_err("pinned");
    assert_eq!(refused.kind(), std::io::ErrorKind::ResourceBusy);
    assert_eq!(
        refused.to_string(),
        format!(
            "image {} is pinned by a lease, or being removed, on this store",
            pinned('c')
        )
    );
    assert!(whole(&p.images, 'c'));
    p.b.unpin_image("lease-b").expect("unpinned");
    p.a.remove_image(&pinned('c')).await.expect("removed");
    assert!(names(&p.images).is_empty());
    no_pins(&p);
}

/// Catches a backend that takes an image for present because it fetched it once, after
/// another backend's collection removed it: the next check fetches it again.
#[tokio::test]
async fn an_image_another_backend_removed_is_fetched_again() {
    let source = Arc::new(Steps::default());
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
