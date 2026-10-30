//! Start: sweep what a crash left, drop stale pins, keep live ones.

mod common;

use std::collections::BTreeSet;

use common::{Rig, run, sized, wanted};
use js_treestore::{Config, Error};

#[test]
fn stale_pins_are_dropped_and_live_ones_kept() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let first = rig.open(rig.cfg());
    first.ensure(&wanted(&l)).unwrap();
    first.pin(&l.diff_id, &run("live")).unwrap();
    first.pin(&l.diff_id, &run("stale")).unwrap();
    let dir = rig.root().join("pins").join(l.diff_id.hex());
    std::fs::write(dir.join(".not-a-run"), b"").unwrap();
    drop(first);

    let store = rig.open_live(rig.cfg(), &BTreeSet::from([run("live")]));
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["live"]);
    assert_eq!(store.stats().pinned, 1);
    assert!(matches!(
        store.pin(&l.diff_id, &run("live")),
        Err(Error::AlreadyPinned)
    ));
    store.pin(&l.diff_id, &run("stale")).unwrap();
}

#[test]
fn a_pinned_tree_survives_a_restart_and_is_not_evicted() {
    let rig = Rig::new();
    let l = sized("f", 100, 1);
    rig.have(&l);
    let first = rig.open(rig.cfg());
    first.ensure(&wanted(&l)).unwrap();
    first.pin(&l.diff_id, &run("live")).unwrap();
    drop(first);
    let cfg = Config {
        budget_bytes: 100,
        ..rig.cfg()
    };
    let store = rig.open_live(cfg, &BTreeSet::from([run("live")]));
    let other = sized("f", 100, 2);
    rig.have(&other);
    assert!(matches!(
        store.ensure(&wanted(&other)),
        Err(Error::NoSpace { .. })
    ));
    assert!(rig.present(&l.diff_id));
}

#[test]
fn what_a_crash_left_is_swept() {
    let rig = Rig::new();
    let good = sized("f", 10, 1);
    rig.have(&good);
    let first = rig.open(rig.cfg());
    first.ensure(&wanted(&good)).unwrap();
    drop(first);

    let root = rig.root();
    let hex = |n: char| n.to_string().repeat(64);
    // A fetched blob, a stage and a trash entry.
    std::fs::write(root.join("tmp").join("part-1"), b"x").unwrap();
    std::fs::create_dir_all(root.join("stage").join("x-1")).unwrap();
    std::fs::create_dir_all(root.join("trash").join("y-1").join("deep")).unwrap();
    // A tree that was renamed and never recorded.
    std::fs::create_dir_all(root.join("trees").join(hex('a'))).unwrap();
    // A record with no tree, and a half-written record.
    std::fs::write(
        root.join("trees").join(format!("{}.sealed", hex('b'))),
        b"{}",
    )
    .unwrap();
    std::fs::write(root.join("trees").join(format!("{}.tmp", hex('c'))), b"{").unwrap();
    // A tree whose record does not parse, and one whose record names another tree.
    std::fs::create_dir_all(root.join("trees").join(hex('d'))).unwrap();
    std::fs::write(
        root.join("trees").join(format!("{}.sealed", hex('d'))),
        b"not json",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("trees").join(hex('e'))).unwrap();
    let other = std::fs::read(
        root.join("trees")
            .join(format!("{}.sealed", good.diff_id.hex())),
    )
    .unwrap();
    std::fs::write(
        root.join("trees").join(format!("{}.sealed", hex('e'))),
        other,
    )
    .unwrap();
    // A name that is no tree.
    std::fs::create_dir_all(root.join("trees").join("junk")).unwrap();

    let store = rig.open(rig.cfg());
    for sub in ["tmp", "stage", "trash"] {
        assert!(rig.names(sub).is_empty(), "{sub}");
    }
    assert_eq!(
        rig.names("trees"),
        [
            good.diff_id.hex().to_owned(),
            format!("{}.sealed", good.diff_id.hex())
        ]
    );
    assert_eq!(store.stats().trees, 1);
    assert_eq!(store.stats().used, 10);
}

#[test]
fn a_store_directory_that_cannot_be_made_is_an_error() {
    let rig = Rig::new();
    std::fs::create_dir_all(rig.root()).unwrap();
    std::fs::write(rig.root().join("trees"), b"").unwrap();
    assert!(matches!(
        rig.try_open(rig.cfg(), &BTreeSet::new()),
        Err(Error::Io { .. })
    ));
}
