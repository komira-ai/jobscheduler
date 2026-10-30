//! Eviction: the budget, the free-space low-water mark, recency, and what is never evicted.

use crate::common;

use std::sync::Arc;

use crate::common::{Rig, size_of, sized, wanted};
use js_treestore::{Config, Error};

fn budget(rig: &Rig, bytes: u64) -> Config {
    Config {
        budget_bytes: bytes,
        ..rig.cfg()
    }
}

#[test]
fn the_least_recently_used_tree_goes_first_and_a_hit_is_a_use() {
    let rig = Rig::new();
    let store = rig.open(budget(&rig, 300));
    let mut ids = Vec::new();
    for fill in 1..=3u8 {
        let l = sized("f", 100, fill);
        rig.have(&l);
        store.ensure(&wanted(&l)).unwrap();
        ids.push(l.diff_id);
    }
    // The first is used again, so the second is the oldest.
    store.ensure(&wanted(&sized("f", 100, 1))).unwrap();
    let l4 = sized("f", 100, 4);
    rig.have(&l4);
    store.ensure(&wanted(&l4)).unwrap();
    assert!(rig.present(&ids[0]));
    assert!(!rig.present(&ids[1]));
    assert!(rig.present(&ids[2]));
    assert!(rig.present(&l4.diff_id));
    assert!(rig.names("trash").is_empty());
}

#[test]
fn a_pinned_tree_is_skipped_and_the_next_goes() {
    let rig = Rig::new();
    let store = rig.open(budget(&rig, 200));
    let mut ids = Vec::new();
    for fill in 1..=2u8 {
        let l = sized("f", 100, fill);
        rig.have(&l);
        store.ensure(&wanted(&l)).unwrap();
        ids.push(l.diff_id);
    }
    let handle = store.pin(&ids[0], &common::run("r1")).unwrap();
    // The oldest is pinned (and, being pinned, now the most recent too): the other goes.
    let l3 = sized("f", 100, 3);
    rig.have(&l3);
    store.ensure(&wanted(&l3)).unwrap();
    assert!(rig.present(&ids[0]));
    assert!(!rig.present(&ids[1]));
    assert!(rig.present(&l3.diff_id));
    store.unpin(handle);
}

#[test]
fn nothing_is_evicted_when_every_tree_is_pinned_kept_or_too_recent() {
    let rig = Rig::new();
    let kept = sized("f", 100, 1);
    let cfg = Config {
        keep: [kept.diff_id.clone()].into(),
        min_age_ms: 1000,
        ..budget(&rig, 200)
    };
    let store = rig.open(cfg);
    rig.have(&kept);
    store.ensure(&wanted(&kept)).unwrap();
    let pinned = sized("f", 100, 2);
    rig.have(&pinned);
    store.ensure(&wanted(&pinned)).unwrap();
    let handle = store.pin(&pinned.diff_id, &common::run("r1")).unwrap();
    let third = sized("f", 100, 3);
    rig.have(&third);
    rig.advance(5000);
    // Kept and pinned: no room, and the new tree is not placed.
    let error = store.ensure(&wanted(&third)).unwrap_err();
    assert!(matches!(
        error,
        Error::NoSpace {
            needed: 100,
            used: 200,
            budget: 200
        }
    ));
    assert!(!rig.present(&third.diff_id));
    assert!(rig.names("stage").is_empty());
    assert!(rig.present(&kept.diff_id));
    assert!(rig.present(&pinned.diff_id));
    store.unpin(handle);
    // Unpinned but just used again: too recent.
    store.ensure(&wanted(&pinned)).unwrap();
    assert!(matches!(
        store.ensure(&wanted(&third)),
        Err(Error::NoSpace { .. })
    ));
    // Old enough now.
    rig.advance(1000);
    store.ensure(&wanted(&third)).unwrap();
    assert!(!rig.present(&pinned.diff_id));
    assert!(rig.present(&kept.diff_id));
    assert!(rig.present(&third.diff_id));
}

#[test]
fn the_budget_holds_across_a_run_of_ensures() {
    let rig = Rig::new();
    let store = rig.open(budget(&rig, 250));
    let mut ids = Vec::new();
    for fill in 1..=5u8 {
        let l = sized("f", 100, fill);
        rig.have(&l);
        store.ensure(&wanted(&l)).unwrap();
        assert!(store.stats().used <= 250);
        ids.push(l.diff_id);
    }
    assert!(!rig.present(&ids[2]));
    assert!(rig.present(&ids[3]));
    assert!(rig.present(&ids[4]));
    assert_eq!(store.stats().trees, 2);
}

/// The bytes of the tree directories (not the records).
fn trees_size(root: &std::path::Path) -> u64 {
    std::fs::read_dir(root.join("trees"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .map(|p| size_of(&p))
        .sum()
}

#[test]
fn the_free_space_low_water_mark_evicts_too() {
    let rig = Rig::new();
    let root = rig.root();
    // A volume of 350 bytes: free is what the trees leave.
    let cfg = Config {
        low_water_free_bytes: 100,
        free_space: Arc::new(move || 350 - trees_size(&root)),
        ..rig.cfg()
    };
    let store = rig.open(cfg);
    let mut ids = Vec::new();
    for fill in 1..=3u8 {
        let l = sized("f", 100, fill);
        rig.have(&l);
        store.ensure(&wanted(&l)).unwrap();
        ids.push(l.diff_id);
    }
    // 300 bytes in, 50 free (the records add a few bytes): under the mark. Trim evicts.
    store.trim().unwrap();
    assert!(!rig.present(&ids[0]));
    assert!(rig.present(&ids[2]));
    // A volume that stays full leaves nothing to evict.
    drop(store);
    let full = rig.open(Config {
        low_water_free_bytes: 1 << 40,
        free_space: Arc::new(|| 0),
        ..rig.cfg()
    });
    assert!(matches!(full.trim(), Err(Error::NoSpace { .. })));
    assert_eq!(full.stats().trees, 0);
}

#[test]
fn a_tree_whose_directory_vanished_still_leaves_the_table() {
    let rig = Rig::new();
    let store = rig.open(budget(&rig, 100));
    let a = sized("f", 100, 1);
    rig.have(&a);
    store.ensure(&wanted(&a)).unwrap();
    std::fs::remove_dir_all(rig.root().join("trees").join(a.diff_id.hex())).unwrap();
    let b = sized("f", 100, 2);
    rig.have(&b);
    store.ensure(&wanted(&b)).unwrap();
    assert_eq!(store.stats().trees, 1);
    assert!(rig.present(&b.diff_id));
}
