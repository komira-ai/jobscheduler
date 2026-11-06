//! The image budget of a runtime on the stand-in: a pull over the budget evicts the image
//! used least recently from the store itself, and the runtime's prefetcher forgets it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use js_agent::crun::{CrunRuntime, config_dir};
use js_agent::lifecycle::{Prepared, Runtime};

use crate::node::{Keeping, LONG, Node};

/// The reference of the image whose digest is `pair` repeated.
fn reference(pair: &str) -> String {
    format!("registry.example/job@sha256:{}", pair.repeat(32))
}

fn held(node: &Node, pair: &str) -> bool {
    node.scratch
        .join("oci-images")
        .join(pair.repeat(32))
        .is_dir()
}

fn open(node: &Node, budget: u64, source: &Arc<Keeping>) -> CrunRuntime {
    let settings = js_agent::crun::CrunSettings {
        image_budget: Some(budget),
        ..node.settings("/root")
    };
    CrunRuntime::open_with(settings, Arc::clone(source) as _, Node::hooks(LONG, LONG))
        .expect("opens")
}

/// Puts the image whose digest is `pair` repeated in the store, as the node's own is.
fn store(node: &Node, pair: &str) {
    let dir = node.scratch.join("oci-images").join(pair.repeat(32));
    std::fs::create_dir_all(dir.join("rootfs")).expect("an image");
}

/// Proves the budget works on the runtime's own store and prefetcher: with room for two of
/// the three images the store holds when the runtime opens, all three are counted, the
/// first pull evicts the oldest (the directory goes), a pull of the evicted image fetches
/// it again and evicts the then-oldest, and the prefetcher, which had that one `Ready`,
/// pulls it again when asked. A run's start on a held image fits. Catches an eviction that
/// is only counted (the store keeps the files), and a prefetcher that still answers `Ready`
/// for an image that is gone.
#[test]
fn a_pull_over_the_budget_evicts_from_the_store_and_the_prefetcher_forgets() {
    let node = Node::new("budget-evicts");
    let source = Arc::new(Keeping {
        configs: config_dir(&node.scratch),
        fetched: AtomicUsize::new(0),
    });
    let (one, two) = (reference("11"), reference("22"));
    store(&node, "11");
    store(&node, "22");
    // Without a budget to speak of, the size of the three images (made alike).
    let measuring = open(&node, u64::MAX, &source);
    assert_eq!(measuring.wait_prepared(&node.image()), Prepared::Ready);
    let each = measuring.image_bytes() / 3;
    drop(measuring);
    assert!(each > 0);
    let mut runtime = open(&node, 2 * each, &source);
    // Opened on three images (11, 22, the node's ab), oldest in digest order: the first
    // pull, of 22, evicts 11.
    assert_eq!(runtime.wait_prepared(&two), Prepared::Ready);
    assert!(!held(&node, "11") && held(&node, "22") && held(&node, "ab"));
    assert_eq!(runtime.image_bytes(), 2 * each);
    assert_eq!(runtime.wait_prepared(&node.image()), Prepared::Ready);
    // 11 again: fetched, and 22 (used before ab) goes.
    assert_eq!(runtime.wait_prepared(&one), Prepared::Ready);
    assert_eq!(source.fetched.load(Ordering::SeqCst), 1);
    assert!(held(&node, "11") && !held(&node, "22") && held(&node, "ab"));
    assert!(matches!(runtime.prepare(&two, None), Prepared::Pending(_)));
    assert_eq!(runtime.wait_prepared(&two), Prepared::Ready);
    assert_eq!(source.fetched.load(Ordering::SeqCst), 2);
    // A run's start on the node's image, which is held: it fits, and its end lets it go.
    let id = runtime
        .create_and_start(&node.spec("r1-exit"))
        .expect("started");
    runtime.wait_ended(&id);
    runtime.remove(&id).expect("removed");
    assert!(held(&node, "ab"));
}

/// Proves a run's lease holds its image against eviction in the runtime itself: while a
/// container is made on the node's image (staged, not removed), pulls over the budget evict
/// the other images and never that one; once the run is removed its image may go, as the
/// one used least recently. Catches a start that does not take its lease, and a removal
/// that does not let it go.
#[test]
fn the_image_of_a_run_is_not_evicted_until_the_run_is_removed() {
    let node = Node::new("budget-lease");
    let source = Arc::new(Keeping {
        configs: config_dir(&node.scratch),
        fetched: AtomicUsize::new(0),
    });
    // The sizes of the node's image and of one the stand-in's source fetches.
    let measuring = open(&node, u64::MAX, &source);
    assert_eq!(measuring.wait_prepared(&node.image()), Prepared::Ready);
    let base = measuring.image_bytes();
    assert_eq!(measuring.wait_prepared(&reference("99")), Prepared::Ready);
    let fetched = measuring.image_bytes() - base;
    drop(measuring);
    std::fs::remove_dir_all(node.scratch.join("oci-images").join("99".repeat(32)))
        .expect("the measured image goes");
    assert!(base > 0 && fetched > 0, "{base} {fetched}");
    // Room for the node's image and one fetched image.
    let mut runtime = open(&node, base + fetched, &source);
    let id = runtime.stage(&node.spec("r1-exit")).expect("staged");
    assert_eq!(runtime.wait_prepared(&reference("22")), Prepared::Ready);
    assert!(held(&node, "22") && held(&node, "ab"));
    // 33: 22 goes, and ab stays, though it was used before 22.
    assert_eq!(runtime.wait_prepared(&reference("33")), Prepared::Ready);
    assert!(!held(&node, "22") && held(&node, "33") && held(&node, "ab"));
    assert_eq!(runtime.wait_prepared(&reference("44")), Prepared::Ready);
    assert!(!held(&node, "33") && held(&node, "44") && held(&node, "ab"));
    runtime.remove(&id).expect("removed");
    // The removal is ab's last use, after 44's pull: 55 evicts 44, and then 66 evicts ab.
    assert_eq!(runtime.wait_prepared(&reference("55")), Prepared::Ready);
    assert!(!held(&node, "44") && held(&node, "ab"));
    assert_eq!(runtime.wait_prepared(&reference("66")), Prepared::Ready);
    assert!(!held(&node, "ab") && held(&node, "55") && held(&node, "66"));
}
