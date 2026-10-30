//! Scratch probes for the review of the tree store. Each asserts what a correct store does.

use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use crate::common::{FakeSealer, Rig, sized, wanted};
use js_treestore::{
    BlobRef, BlobSink, Config, FetchError, LayerSource, Step, TreeStore,
};

struct Boom;

impl LayerSource for Boom {
    fn fetch(&self, _blob: &BlobRef, _sink: &mut dyn BlobSink) -> Result<(), FetchError> {
        panic!("source panics")
    }
}

#[test]
fn p1_a_panicking_leader_leaves_no_flight_behind() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    let store = TreeStore::open(
        &rig.root(),
        Boom,
        FakeSealer::new(),
        rig.cfg(),
        &BTreeSet::new(),
    )
    .unwrap();
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| store.ensure(&wanted(&l))));
    assert!(outcome.is_err());
    assert_eq!(
        store.stats().in_flight,
        0,
        "a later caller for this blob would wait forever"
    );
}

#[test]
fn p2_concurrent_ensures_of_two_trees_hold_the_budget() {
    let rig = Rig::new();
    let (a, b) = (sized("f", 100, 1), sized("f", 100, 2));
    rig.have(&a);
    rig.have(&b);
    let barrier = Arc::new(Barrier::new(2));
    let at = barrier.clone();
    let cfg = Config {
        budget_bytes: 150,
        crash: Some(Arc::new(move |step| {
            if step == Step::Renamed {
                at.wait();
            }
            false
        })),
        ..rig.cfg()
    };
    let store = rig.open(cfg);
    let (wa, wb) = (wanted(&a), wanted(&b));
    thread::scope(|scope| {
        let x = scope.spawn(|| store.ensure(&wa));
        let y = scope.spawn(|| store.ensure(&wb));
        x.join().unwrap().unwrap();
        y.join().unwrap().unwrap();
    });
    assert!(store.stats().used <= 150, "used {}", store.stats().used);
}

#[test]
fn p3_an_eviction_that_cannot_move_the_tree_does_not_count_as_room() {
    let rig = Rig::new();
    let (a, b) = (sized("f", 100, 1), sized("f", 100, 2));
    rig.have(&a);
    rig.have(&b);
    let store = rig.open(Config {
        budget_bytes: 100,
        ..rig.cfg()
    });
    store.ensure(&wanted(&a)).unwrap();
    std::fs::remove_dir(rig.root().join("trash")).unwrap();
    let _ = store.ensure(&wanted(&b));
    let still = rig.root().join("trees").join(a.diff_id.hex()).exists();
    assert!(
        !still || store.stats().trees == 2,
        "the victim is on disk but not counted: budget 100, disk holds both"
    );
}

#[test]
fn p4_a_stale_tree_whose_trash_failed_is_not_relabelled_current() {
    let rig = Rig::new();
    let a = sized("f", 10, 1);
    rig.have(&a);
    let first = rig.open(rig.cfg());
    first.ensure(&wanted(&a)).unwrap();
    drop(first);
    let mark = rig.root().join("trees").join(a.diff_id.hex()).join("OLD");
    std::fs::write(&mark, b"made by the old extractor").unwrap();
    let store = rig.open(Config {
        extractor_version: 2,
        ..rig.cfg()
    });
    let trash = rig.root().join("trash");
    std::fs::remove_dir(&trash).unwrap();
    assert!(store.ensure(&wanted(&a)).is_err());
    std::fs::create_dir(&trash).unwrap();
    store.ensure(&wanted(&a)).unwrap();
    assert!(!mark.exists(), "the old tree now carries the new version");
}

#[test]
fn p5_a_second_open_of_the_directory_does_not_take_a_tree_from_the_first() {
    let rig = Rig::new();
    let a = sized("f", 10, 1);
    rig.have(&a);
    let done = Arc::new(AtomicBool::new(false));
    let (source, sealer, root) = (rig.source.clone(), rig.sealer.clone(), rig.root());
    let plain = rig.cfg();
    let flag = done.clone();
    let cfg = Config {
        crash: Some(Arc::new(move |step| {
            if step == Step::Renamed && !flag.swap(true, Ordering::SeqCst) {
                TreeStore::open(
                    &root,
                    source.clone(),
                    sealer.clone(),
                    plain.clone(),
                    &BTreeSet::new(),
                )
                .unwrap();
            }
            false
        })),
        ..rig.cfg()
    };
    let store = rig.open(cfg);
    let ready = store.ensure(&wanted(&a)).unwrap();
    assert!(ready.path.is_dir(), "ensure returned a path that is not there");
}
