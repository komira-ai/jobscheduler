//! Start: sweep what a crash left, drop stale pins, keep live ones, register before GC.

mod common;

use std::collections::BTreeSet;

use common::{Rig, run, sized, wanted};
use js_treestore::blobstore::Holder;
use js_treestore::{Config, Error};

#[test]
fn stale_pins_are_dropped_and_live_ones_kept() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let first = rig.open(Config::default());
    first.ensure(&wanted(&l)).unwrap();
    first.pin(&l.diff_id, &run("live")).unwrap();
    first.pin(&l.diff_id, &run("stale")).unwrap();
    let dir = rig.root().join("pins").join(l.diff_id.hex());
    std::fs::write(dir.join(".not-a-run"), b"").unwrap();
    drop(first);
    rig.fake.restart(&BTreeSet::new());

    let store = rig.open_live(Config::default(), &BTreeSet::from([run("live")]));
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
    // The pinned tree is not released.
    assert_eq!(rig.fake.pressure(0), 10);
}

#[test]
fn bytes_are_registered_before_gc_starts() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let first = rig.open(Config::default());
    first.ensure(&wanted(&l)).unwrap();
    drop(first);
    rig.fake.restart(&BTreeSet::new());
    assert_eq!(rig.fake.accounted(), 0);
    let _store = rig.open(Config::default());
    assert_eq!(rig.fake.accounted(), 10);
    assert!(!rig.fake.gc_started());
    // The caller's next calls, in the store's order.
    rig.fake.reap_pins(&BTreeSet::<Holder>::new());
    rig.fake.start_gc_now();
    assert!(rig.fake.gc_started());
}

#[test]
fn what_a_crash_left_is_swept() {
    let rig = Rig::new();
    let good = sized("f", 10, 1);
    rig.have(&good);
    let first = rig.open(Config::default());
    first.ensure(&wanted(&good)).unwrap();
    drop(first);
    rig.fake.restart(&BTreeSet::new());

    let root = rig.root();
    let hex = |n: char| n.to_string().repeat(64);
    // A stage and a trash entry.
    std::fs::create_dir_all(root.join("stage").join("x-1")).unwrap();
    std::fs::create_dir_all(root.join("trash").join("y-1").join("deep")).unwrap();
    std::fs::write(root.join("trash").join("loose-file"), b"").unwrap();
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

    let store = rig.open(Config::default());
    assert!(rig.names("stage").is_empty());
    assert!(rig.names("trash").is_empty());
    let mut left = rig.names("trees");
    left.sort();
    assert_eq!(
        left,
        [
            good.diff_id.hex().to_owned(),
            format!("{}.sealed", good.diff_id.hex())
        ]
    );
    assert_eq!(store.stats().trees, 1);
    assert_eq!(rig.fake.accounted(), 10);
    assert!(rig.sealer.unsealed.lock().unwrap().len() >= 6);
}

#[test]
fn a_store_directory_that_cannot_be_made_or_read_is_an_error() {
    let rig = Rig::new();
    std::fs::create_dir_all(rig.root()).unwrap();
    std::fs::write(rig.root().join("trees"), b"").unwrap();
    assert!(matches!(
        rig.try_open(Config::default(), &BTreeSet::new()),
        Err(Error::Io { .. })
    ));
}

#[test]
fn a_refused_registration_at_start_is_an_error() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let first = rig.open(Config::default());
    first.ensure(&wanted(&l)).unwrap();
    drop(first);
    rig.fake
        .fail_account
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        rig.try_open(Config::default(), &BTreeSet::new()),
        Err(Error::Store(_))
    ));
}
