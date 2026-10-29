//! Release in the blob store's order, and what the tree store will not release.

mod common;

use common::{Rig, Store, sized, wanted};
use js_bundle::DiffId;
use js_treestore::Config;
use js_treestore::blobstore::Busy;

/// Three trees of 100 bytes each, made in order, then the first used again: the order of
/// recency is second, third, first.
fn three(rig: &Rig, cfg: Config) -> (Store, Vec<DiffId>) {
    let store = rig.open(cfg);
    let mut ids = Vec::new();
    for fill in 1..=3u8 {
        let l = sized("f", 100, fill);
        rig.have(&l);
        store.ensure(&wanted(&l)).unwrap();
        ids.push(l.diff_id);
    }
    let first = sized("f", 100, 1);
    store.ensure(&wanted(&first)).unwrap();
    (store, ids)
}

fn present(rig: &Rig, id: &DiffId) -> bool {
    rig.root().join("trees").join(id.hex()).is_dir()
        && rig
            .root()
            .join("trees")
            .join(format!("{}.sealed", id.hex()))
            .is_file()
}

#[test]
fn the_least_recently_used_tree_goes_first() {
    let rig = Rig::new();
    let (_store, ids) = three(&rig, Config::default());
    assert_eq!(rig.fake.pressure(200), 0);
    assert!(present(&rig, &ids[0]));
    assert!(!present(&rig, &ids[1]));
    assert!(present(&rig, &ids[2]));
    assert_eq!(rig.fake.pressure(100), 0);
    assert!(present(&rig, &ids[0]));
    assert!(!present(&rig, &ids[2]));
    assert_eq!(rig.fake.accounted(), 100);
    assert!(rig.names("trash").is_empty());
}

#[test]
fn a_pinned_tree_answers_busy_and_the_next_goes() {
    let rig = Rig::new();
    let (store, ids) = three(&rig, Config::default());
    let handle = store.pin(&ids[1], &common::run("r1")).unwrap();
    let over = rig.fake.pressure(100);
    assert_eq!(over, 0);
    assert!(present(&rig, &ids[1]));
    assert!(!present(&rig, &ids[0]));
    assert!(!present(&rig, &ids[2]));
    // Nothing else can go: the pinned tree stays and the shortfall is reported.
    assert_eq!(rig.fake.pressure(0), 100);
    assert!(present(&rig, &ids[1]));
    store.unpin(handle);
    assert_eq!(rig.fake.pressure(0), 0);
    assert!(!present(&rig, &ids[1]));
}

#[test]
fn a_kept_tree_is_never_released() {
    let rig = Rig::new();
    let probe = sized("f", 100, 2);
    let cfg = Config {
        keep: [probe.diff_id.clone()].into(),
        ..Config::default()
    };
    let (_store, ids) = three(&rig, cfg);
    assert_eq!(rig.fake.pressure(0), 100);
    assert!(present(&rig, &ids[1]));
    assert!(!present(&rig, &ids[0]));
    assert!(!present(&rig, &ids[2]));
}

#[test]
fn the_budget_is_honoured_after_a_run_of_ensures() {
    let rig = Rig::new();
    let store = rig.open(Config::default());
    let mut ids = Vec::new();
    for fill in 1..=5u8 {
        let l = sized("f", 100, fill);
        rig.have(&l);
        store.ensure(&wanted(&l)).unwrap();
        assert_eq!(rig.fake.pressure(250), 0);
        assert!(rig.fake.accounted() <= 250);
        ids.push(l.diff_id);
    }
    assert!(!present(&rig, &ids[2]));
    assert!(present(&rig, &ids[3]));
    assert!(present(&rig, &ids[4]));
    assert_eq!(store.stats().trees, 2);
}

#[test]
fn a_tree_that_is_gone_or_cannot_move_answers_busy() {
    let rig = Rig::new();
    let (store, ids) = three(&rig, Config::default());
    // Its directory vanished behind the store's back: the move fails and the entry stays.
    std::fs::remove_dir_all(rig.root().join("trees").join(ids[1].hex())).unwrap();
    assert_eq!(rig.fake.pressure(200), 0);
    assert!(present(&rig, &ids[0]));
    assert!(!present(&rig, &ids[2]));
    assert_eq!(store.stats().trees, 2);
    assert_eq!(rig.fake.accounted(), 200);
}

#[test]
fn a_callback_for_a_tree_already_released_answers_busy() {
    let rig = Rig::new();
    let (_store, _ids) = three(&rig, Config::default());
    let callback = rig.fake.first_release();
    assert_eq!(rig.fake.pressure(0), 0);
    assert_eq!(callback.release(), Err(Busy));
}

#[test]
fn a_callback_after_the_tree_store_is_gone_frees_nothing() {
    let rig = Rig::new();
    let (store, _ids) = three(&rig, Config::default());
    let callback = rig.fake.first_release();
    drop(store);
    assert_eq!(callback.release(), Ok(0));
}

#[test]
fn a_hit_is_a_use_for_the_recency_order() {
    let rig = Rig::new();
    let (store, ids) = three(&rig, Config::default());
    store.ensure(&wanted(&sized("f", 100, 2))).unwrap();
    // 2 is now the most recent, so 3 is the oldest.
    assert_eq!(rig.fake.pressure(200), 0);
    assert!(!present(&rig, &ids[2]));
}
