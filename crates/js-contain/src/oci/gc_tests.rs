//! Unit tests of the image store collection ([`super::gc`]): what no `crun` is needed
//! for (`/usr/bin/true` stands in where a helper only has to exit 0). The files of the
//! stored trees are the test user's own, so the no-recursion removal takes them.
//! No test here waits for a time: where an unpack must be in flight, a source holds it
//! until the test lets it go.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use super::gc::GcReport;
use super::image::{FetchError, ImageSource, Images, PinnedImage, ROOTFS};
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

/// A source whose fetch says it began and then waits to be let go: an unpack held in
/// flight with no clock involved.
#[derive(Debug)]
struct Gate {
    began: Mutex<Option<oneshot::Sender<()>>>,
    go: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait::async_trait]
impl ImageSource for Gate {
    async fn fetch(&self, _: &PinnedImage, _: &Path) -> Result<Vec<String>, FetchError> {
        let began = self.began.lock().expect("lock").take();
        if let Some(began) = began {
            let _ = began.send(());
        }
        let go = self.go.lock().expect("lock").take();
        if let Some(go) = go {
            let _ = go.await;
        }
        Ok(Vec::new())
    }
}

/// A started backend over a scratch directory.
struct Fixture {
    oci: Arc<Oci>,
    scratch: PathBuf,
    images: PathBuf,
    // Last: removed after everything that runs in it.
    dir: crate::test_dir::TestDir,
}

fn fixture_with(name: &str, source: Arc<dyn ImageSource>) -> Fixture {
    let dir = scratch(name);
    let scratch = dir.join("scratch");
    let oci = Oci::on(&config(&dir, true), source, &scratch, "/actions", "x86_64").expect("starts");
    let images = scratch.join("oci-images");
    std::fs::create_dir_all(&images).expect("images");
    Fixture {
        oci,
        scratch,
        images,
        dir,
    }
}

fn fixture(name: &str) -> Fixture {
    fixture_with(name, Arc::new(Counting::default()))
}

/// An unpacked image `<images>/<hex of c>/rootfs/usr/bin/tool` of `size` bytes.
fn image(images: &Path, c: char, size: usize) {
    let bin = images.join(hexn(c)).join(ROOTFS).join("usr/bin");
    std::fs::create_dir_all(&bin).expect("image");
    std::fs::write(bin.join("tool"), vec![b'x'; size]).expect("tool");
}

/// The disk of the entries of the image [`image`] made, counted by the standard library
/// (512-byte blocks of each: the digest directory, the rootfs, `usr`, `bin`, the file).
fn image_disk(images: &Path, c: char) -> u64 {
    let top = images.join(hexn(c));
    [
        top.clone(),
        top.join(ROOTFS),
        top.join("rootfs/usr"),
        top.join("rootfs/usr/bin"),
        top.join("rootfs/usr/bin/tool"),
    ]
    .iter()
    .map(|p| std::fs::symlink_metadata(p).expect("stat").blocks() * 512)
    .sum()
}

/// A report of a collection that removed nothing and failed at nothing, with `kept`
/// digests of `kept_bytes`.
fn settled(report: &GcReport, kept: usize, kept_bytes: u64) {
    assert!(
        report.removed.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    assert_eq!(
        (report.removed_bytes, report.kept, report.kept_bytes),
        (0, kept, kept_bytes)
    );
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

/// Catches an image nothing names being kept, a removal that leaves part of the tree,
/// and a sweep that takes what is not a digest directory (a stray file, a short name,
/// capital hex).
#[test]
fn an_unreferenced_digest_is_removed_and_only_digests() {
    let fx = fixture("gc-unreferenced");
    image(&fx.images, 'a', 100);
    image(&fx.images, 'b', 100);
    std::fs::write(fx.images.join("note"), b"x").expect("note");
    std::fs::create_dir_all(fx.images.join("abc/rootfs")).expect("short");
    let capital = "A".repeat(64);
    std::fs::create_dir_all(fx.images.join(&capital)).expect("capital");
    let report = fx.oci.collect_images(&[], &[]).expect("collected");
    assert_eq!(report.removed, [digest('a'), digest('b')]);
    assert_eq!((report.kept, report.failed.len()), (0, 0), "{report:?}");
    assert_eq!(
        names(&fx.images),
        ["A".repeat(64), "abc".to_owned(), "note".to_owned()]
    );
}

/// Catches a named digest being removed, the `sha256:` form and the bare hex taking
/// different paths, and a kept image's files being touched.
#[test]
fn a_configured_digest_is_kept_with_its_files() {
    let fx = fixture("gc-configured");
    for c in ['a', 'b', 'c'] {
        image(&fx.images, c, 100);
    }
    let report = fx
        .oci
        .collect_images(&[digest('a'), hexn('b')], &[])
        .expect("collected");
    assert_eq!(report.removed, [digest('c')]);
    assert_eq!(report.kept, 2);
    for c in ['a', 'b'] {
        let tool = fx.images.join(hexn(c)).join("rootfs/usr/bin/tool");
        assert_eq!(std::fs::read(tool).expect("kept").len(), 100);
    }
}

/// Catches the digests of live leases being ignored: only the configured digest and the
/// leased one stay.
#[test]
fn the_digest_of_a_live_lease_is_kept() {
    let fx = fixture("gc-leased");
    for c in ['a', 'b', 'c'] {
        image(&fx.images, c, 100);
    }
    let report = fx
        .oci
        .collect_images(&[digest('a')], &[digest('b')])
        .expect("collected");
    assert_eq!(report.removed, [digest('c')]);
    assert_eq!(names(&fx.images), [hexn('a'), hexn('b')]);
}

/// Catches an unpack that is under way being collected from under it: a digest with
/// an unpack in flight keeps its stage and blobs, the unpack finishes, and an image
/// beside it that nothing names still goes.
#[tokio::test]
async fn an_unpack_in_flight_is_not_disturbed() {
    let (began_tx, began) = oneshot::channel();
    let (go_tx, go) = oneshot::channel();
    let gate = Gate {
        began: Mutex::new(Some(began_tx)),
        go: Mutex::new(Some(go)),
    };
    let fx = fixture_with("gc-in-flight", Arc::new(gate));
    let oci = Arc::clone(&fx.oci);
    let flight = tokio::spawn(async move { oci.check_pinned(&pinned('a')).await });
    began.await.expect("the fetch began");
    image(&fx.images, 'b', 100);
    let before = names(&fx.images);
    assert!(
        before
            .iter()
            .any(|n| n.starts_with(".stage-") && n.ends_with("-blobs")),
        "{before:?}"
    );
    let report = fx.oci.collect_images(&[], &[]).expect("collected");
    assert_eq!(report.removed, [digest('b')]);
    let after = names(&fx.images);
    assert_eq!(
        after,
        before
            .iter()
            .filter(|n| **n != hexn('b'))
            .cloned()
            .collect::<Vec<_>>()
    );
    go_tx.send(()).expect("the fetch waits");
    flight.await.expect("joined").expect("unpacked");
    assert!(fx.images.join(hexn('a')).is_dir());
}

/// Catches a digest whose gate is held (a fetch or unpack under way) being collected, and
/// a gate that stays held after: the digest is kept while another holds its gate and
/// removed once the gate is free.
#[test]
fn a_digest_whose_gate_is_held_is_kept_until_it_is_let_go() {
    let dir = scratch("gc-gate");
    let store = Images::new(dir.join("oci-images"));
    image(&dir.join("oci-images"), 'a', 100);
    let gate = store.gate_of(&hexn('a'));
    let mut remove = |p: &Path| crate::remove::remove_tree(p).map_err(|e| e.to_string());
    let keep = std::collections::HashSet::new();
    let held = gate.try_lock().expect("free");
    let kept = store.collect(&keep, &mut remove).expect("collected");
    assert_eq!((kept.removed.len(), kept.kept), (0, 1), "{kept:?}");
    drop(held);
    let freed = store.collect(&keep, &mut remove).expect("collected");
    assert_eq!(freed.removed, [digest('a')]);
}

/// Catches a removal that runs without the digest's gate, so that a fetch could start
/// in the directory being removed: the gate is held while `remove` runs, and free after.
#[test]
fn the_gate_is_held_while_a_digest_is_removed() {
    let dir = scratch("gc-removing");
    let store = Images::new(dir.join("oci-images"));
    image(&dir.join("oci-images"), 'a', 100);
    let gate = store.gate_of(&hexn('a'));
    let mut during = None;
    let mut remove = |p: &Path| {
        during = Some(gate.try_lock().is_err());
        crate::remove::remove_tree(p).map_err(|e| e.to_string())
    };
    store
        .collect(&std::collections::HashSet::new(), &mut remove)
        .expect("collected");
    assert_eq!(during, Some(true), "the removal did not hold the gate");
    assert!(gate.try_lock().is_ok());
}

/// Catches a removed digest that a later `ensure` takes for present because its single
/// flight still says done: the image is fetched again.
#[tokio::test]
async fn a_removed_digest_is_fetched_again_by_the_next_use() {
    let source = Arc::new(Counting::default());
    let fx = fixture_with("gc-refetch", source.clone());
    let image_a = pinned('a');
    fx.oci.check_pinned(&image_a).await.expect("first use");
    // The stand-in unpack made no root filesystem; make it complete.
    std::fs::create_dir_all(fx.images.join(hexn('a')).join(ROOTFS)).expect("rootfs");
    let report = fx.oci.collect_images(&[], &[]).expect("collected");
    assert_eq!(report.removed, [digest('a')]);
    fx.oci.check_pinned(&image_a).await.expect("second use");
    assert_eq!(
        source.calls.load(Ordering::SeqCst),
        2,
        "served from a stale flight"
    );
    assert!(fx.images.join(hexn('a')).is_dir());
}

/// Catches a collection that follows a symlink out of the store: a link to a directory
/// and to a file inside an image, a digest name that is itself a link, and a `rootfs`
/// that is a link. What they point at stays; the links go.
#[test]
fn a_symlink_in_a_stored_tree_cannot_reach_outside_the_store() {
    let fx = fixture("gc-symlinks");
    let outside = fx.scratch.join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("precious"), b"keep").expect("precious");
    image(&fx.images, 'a', 100);
    let rootfs = fx.images.join(hexn('a')).join(ROOTFS);
    std::os::unix::fs::symlink(&outside, rootfs.join("dir-link")).expect("link");
    std::os::unix::fs::symlink(outside.join("precious"), rootfs.join("usr/file-link"))
        .expect("link");
    std::os::unix::fs::symlink(&outside, fx.images.join(hexn('b'))).expect("name link");
    std::fs::create_dir_all(fx.images.join(hexn('c'))).expect("c");
    std::os::unix::fs::symlink(&outside, fx.images.join(hexn('c')).join(ROOTFS)).expect("rootfs");
    let report = fx
        .oci
        .collect_images(&[digest('b'), digest('c')], &[])
        .expect("collected");
    assert_eq!(report.removed, [digest('a'), digest('b'), digest('c')]);
    assert_eq!(
        std::fs::read(outside.join("precious")).expect("intact"),
        b"keep"
    );
    assert_eq!(names(&outside), ["precious"]);
    assert!(names(&fx.images).is_empty(), "{:?}", names(&fx.images));
}

/// Catches what a crash leaves staying: the stage and blobs of a dead unpack go at the
/// next start, and a digest directory with no root filesystem goes even when named
/// (it would block the rename of the next unpack), after which the image unpacks.
#[tokio::test]
async fn what_a_crash_left_is_removed_and_the_image_can_unpack_again() {
    let fx = fixture("gc-crash");
    let stage = format!(".stage-{}-77-0", hexn('a'));
    std::fs::create_dir_all(fx.images.join(&stage)).expect("stage");
    std::fs::create_dir_all(fx.images.join(format!("{stage}-blobs"))).expect("blobs");
    std::fs::create_dir_all(fx.images.join(hexn('a'))).expect("partial digest");
    std::fs::write(fx.images.join(hexn('a')).join("partial"), b"x").expect("partial");
    let source = Arc::new(Counting::default());
    let restarted = Oci::on(
        &config(&fx.dir, true),
        source,
        &fx.scratch,
        "/actions",
        "x86_64",
    )
    .expect("restarts");
    assert_eq!(
        names(&fx.images),
        [hexn('a')],
        "the start leaves the digest dir"
    );
    let report = restarted
        .collect_images(&[digest('a')], &[])
        .expect("collected");
    assert_eq!(report.removed, [digest('a')]);
    assert!(names(&fx.images).is_empty());
    restarted.check_pinned(&pinned('a')).await.expect("unpacks");
    assert!(fx.images.join(hexn('a')).is_dir());
    assert!(!fx.images.join(hexn('a')).join("partial").exists());
}

/// Catches a second collection that removes or reports something, a store that changes
/// across a restart, and a first collection that did not settle: the second, and one
/// after a start, are no-ops with the same sizes.
#[test]
fn a_second_collection_is_a_no_op() {
    let fx = fixture("gc-twice");
    image(&fx.images, 'a', 100);
    image(&fx.images, 'b', 100);
    let first = fx.oci.collect_images(&[digest('a')], &[]).expect("first");
    assert_eq!(first.removed, [digest('b')]);
    let again = fx.oci.collect_images(&[digest('a')], &[]).expect("again");
    settled(&again, 1, first.kept_bytes);
    let restarted = Oci::on(
        &config(&fx.dir, true),
        Arc::new(Counting::default()),
        &fx.scratch,
        "/actions",
        "x86_64",
    )
    .expect("restarts");
    let after = restarted
        .collect_images(&[digest('a')], &[])
        .expect("again");
    settled(&after, 1, first.kept_bytes);
    assert_eq!(names(&fx.images), [hexn('a')]);
}

/// Catches the accounting being wrong: the report's bytes are the disk the removed and
/// the kept digests hold (512-byte blocks of every entry, the digest directory
/// included), not file sizes, not a count of one side only; what the store holds after
/// is what is kept, so that free space read after the collection already excludes it and
/// the offered disk is not reduced twice. The report's words (`Display`) carry both.
#[test]
fn the_report_counts_the_disk_of_what_went_and_what_stays() {
    let fx = fixture("gc-accounting");
    image(&fx.images, 'a', 10_000);
    image(&fx.images, 'b', 300_000);
    let (kept, gone) = (image_disk(&fx.images, 'a'), image_disk(&fx.images, 'b'));
    let report = fx
        .oci
        .collect_images(&[digest('a')], &[])
        .expect("collected");
    assert_eq!((report.removed_bytes, report.kept_bytes), (gone, kept));
    assert!(gone > kept, "the larger image went");
    let shown = report.to_string();
    // The derived `Debug` is what a failed assertion prints; run it once here.
    assert!(format!("{report:?}").contains("removed_bytes"));
    assert!(
        shown.contains(&format!("removed 1 digest(s) ({gone} bytes)")),
        "{shown}"
    );
    assert!(
        shown.contains(&format!("({kept} bytes in the store)")),
        "{shown}"
    );
}

/// Catches a removal that cannot finish being reported as done, or hidden: a directory
/// the daemon user may not write is left, named in the report, counted as still held,
/// and goes at the next collection once it can.
#[test]
fn a_digest_that_cannot_be_removed_is_reported_and_stays_counted() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = fixture("gc-stuck");
    image(&fx.images, 'a', 100);
    let locked = fx.images.join(hexn('a')).join("rootfs/usr/bin");
    let mode = |m| std::fs::Permissions::from_mode(m);
    std::fs::set_permissions(&locked, mode(0o500)).expect("chmod");
    let report = fx.oci.collect_images(&[], &[]).expect("collected");
    std::fs::set_permissions(&locked, mode(0o755)).expect("chmod back");
    assert!(
        report.removed.is_empty() && report.removed_bytes == 0,
        "{report:?}"
    );
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(report.failed[0].starts_with(&digest('a')), "{report:?}");
    assert!(report.kept_bytes > 0, "{report:?}");
    assert!(report.to_string().contains("could not remove"), "{report}");
    let again = fx.oci.collect_images(&[], &[]).expect("collected");
    assert_eq!(again.removed, [digest('a')]);
}

/// Catches a collection that fails on a node with no store yet (the first start).
#[test]
fn a_node_with_no_store_collects_nothing() {
    let dir = scratch("gc-no-store");
    let store = Images::new(dir.join("never-made"));
    let mut remove = |_: &Path| Ok(());
    let report = store
        .collect(&std::collections::HashSet::new(), &mut remove)
        .expect("nothing to read is not an error");
    settled(&report, 0, 0);
    // A store that cannot be read is an error, not an empty store.
    std::fs::write(dir.join("a-file"), b"x").expect("file");
    let broken = Images::new(dir.join("a-file"));
    let why = broken
        .collect(&std::collections::HashSet::new(), &mut remove)
        .expect_err("not a directory");
    assert!(why.contains("reading the image store"), "{why}");
}

/// Catches a directory the daemon user may not open being fatal to the measure or
/// counted for nothing: it counts for its own entry, and the rest of the tree is still
/// counted.
#[test]
fn a_directory_that_cannot_be_opened_counts_for_its_own_entry() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = fixture("gc-unreadable");
    image(&fx.images, 'a', 10_000);
    let private = fx.images.join(hexn('a')).join("rootfs/private");
    std::fs::create_dir(&private).expect("private");
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let own = std::fs::symlink_metadata(&private).expect("stat").blocks() * 512;
    let want = image_disk(&fx.images, 'a') + own;
    let report = fx
        .oci
        .collect_images(&[digest('a')], &[])
        .expect("collected");
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_eq!((report.kept, report.kept_bytes), (1, want), "{report:?}");
}

/// Catches a digest that vanishes under a removal being an error, and a failure to look
/// at a path being taken for a removal.
#[test]
fn removing_an_entry_that_is_gone_is_done_and_one_that_cannot_be_seen_fails() {
    let fx = fixture("gc-entry");
    assert_eq!(fx.oci.remove_entry(&fx.images.join(hexn('a'))), Ok(()));
    std::fs::write(fx.images.join("file"), b"x").expect("file");
    let why = fx.oci.remove_entry(&fx.images.join("file/below"));
    assert!(why.is_err(), "{why:?}");
}
