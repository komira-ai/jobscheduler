//! Scratch probes for the second review of the tree store.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::common::{Rig, run, sized, wanted};
use js_treestore::{Config, Step};

#[test]
fn q1_a_tree_that_vanished_from_the_disk_does_not_block_every_later_ensure() {
    let rig = Rig::new();
    let store = rig.open(Config {
        budget_bytes: 100,
        ..rig.cfg()
    });
    let (a, b) = (sized("f", 100, 1), sized("f", 100, 2));
    rig.have(&a);
    rig.have(&b);
    store.ensure(&wanted(&a)).unwrap();
    std::fs::remove_dir_all(rig.root().join("trees").join(a.diff_id.hex())).unwrap();
    let outcome = store.ensure(&wanted(&b));
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[test]
fn q2_a_record_that_cannot_be_written_does_not_wedge_the_tree() {
    let rig = Rig::new();
    let a = sized("f", 10, 1);
    rig.have(&a);
    let root = rig.root();
    let once = Arc::new(AtomicBool::new(true));
    let store = rig.open(Config {
        crash: Some(Arc::new(move |step| {
            if step == Step::Renamed && once.swap(false, Ordering::SeqCst) {
                std::fs::remove_dir_all(root.join("tmp")).unwrap();
            }
            false
        })),
        ..rig.cfg()
    });
    assert!(store.ensure(&wanted(&a)).is_err());
    std::fs::create_dir(rig.root().join("tmp")).unwrap();
    let again = store.ensure(&wanted(&a));
    assert!(again.is_ok(), "{again:?}");
}

#[test]
fn q3_a_sealer_panic_during_an_eviction_leaves_the_victim_usable() {
    let rig = Rig::new();
    let store = rig.open(Config {
        budget_bytes: 100,
        ..rig.cfg()
    });
    let (a, b) = (sized("f", 100, 1), sized("f", 100, 2));
    rig.have(&a);
    rig.have(&b);
    store.ensure(&wanted(&a)).unwrap();
    rig.sealer.panic_unseals.store(1, Ordering::SeqCst);
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| store.ensure(&wanted(&b))));
    assert!(outcome.is_err());
    let pinned = store.pin(&a.diff_id, &run("r1"));
    assert!(pinned.is_ok(), "{pinned:?}");
}
