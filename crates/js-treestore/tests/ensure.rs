//! Making a tree from a layer blob, and pinning it for runs.

mod common;

use std::collections::BTreeSet;

use common::{Rig, SEAL_FAILS, SEAL_VANISHES, layer, run, sized, wanted};
use js_treestore::{Config, Error, TREE_PRIORITY};

#[test]
fn a_layer_becomes_a_sealed_registered_tree_and_its_blob_pin_is_let_go() {
    let rig = Rig::new();
    let l = layer(&[("bin/tool", b"#!/bin/sh\n"), ("data", b"hello")]);
    rig.have(&l);
    let store = rig.open(Config::default());
    let ready = store.ensure(&wanted(&l)).unwrap();
    assert_eq!(ready.diff_id, l.diff_id);
    assert_eq!(std::fs::read(ready.path.join("data")).unwrap(), b"hello");
    assert_eq!(rig.sealer.seal_count(), 1);
    assert_eq!(rig.fake.accounted(), 15);
    assert_eq!(rig.fake.priorities(), [TREE_PRIORITY]);
    assert!(rig.fake.holders_pinning().is_empty());
    assert!(rig.names("stage").is_empty());
    assert_eq!(rig.names("trees").len(), 2);
}

#[test]
fn a_present_tree_is_not_extracted_again() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let store = rig.open(Config::default());
    let first = store.ensure(&wanted(&l)).unwrap();
    let second = store.ensure(&wanted(&l)).unwrap();
    assert_eq!(first, second);
    assert_eq!(rig.sealer.seal_count(), 1);
    assert_eq!(rig.fake.entries(), 1);
}

#[test]
fn a_blob_that_is_not_the_layer_its_diff_id_names_leaves_nothing() {
    let rig = Rig::new();
    let named = sized("f", 10, 1);
    let other = sized("f", 10, 2);
    rig.have(&other);
    let mut want = wanted(&other);
    want.diff_id = named.diff_id.clone();
    let store = rig.open(Config::default());
    let error = store.ensure(&want).unwrap_err();
    assert!(matches!(error, Error::Unpack(_)));
    assert!(rig.names("stage").is_empty());
    assert!(rig.names("trees").is_empty());
    assert!(rig.fake.holders_pinning().is_empty());
    assert_eq!(rig.fake.entries(), 0);
    assert_eq!(store.stats().in_flight, 0);
}

#[test]
fn a_refused_seal_leaves_nothing() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    rig.sealer.set(SEAL_FAILS);
    let store = rig.open(Config::default());
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Seal(_))));
    assert!(rig.names("stage").is_empty());
    assert!(rig.names("trees").is_empty());
    assert!(rig.fake.holders_pinning().is_empty());
}

#[test]
fn a_blob_the_store_cannot_give_is_an_error() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    let store = rig.open(Config::default());
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Store(_))));
    assert!(rig.names("stage").is_empty());
}

#[test]
fn a_stage_that_vanishes_before_the_rename_is_an_io_error() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    rig.sealer.set(SEAL_VANISHES);
    let store = rig.open(Config::default());
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Io { .. })));
    assert!(rig.fake.holders_pinning().is_empty());
}

#[test]
fn a_blob_pin_that_cannot_be_released_does_not_fail_a_good_tree() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    rig.fake
        .fail_release
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let store = rig.open(Config::default());
    store.ensure(&wanted(&l)).unwrap();
    assert_eq!(rig.fake.holders_pinning().len(), 1);
}

#[test]
fn a_refused_registration_is_an_error_and_the_next_ensure_registers_the_tree_on_disk() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    rig.fake
        .fail_account
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let store = rig.open(Config::default());
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Store(_))));
    rig.fake
        .fail_account
        .store(false, std::sync::atomic::Ordering::SeqCst);
    store.ensure(&wanted(&l)).unwrap();
    assert_eq!(rig.fake.accounted(), 10);
    assert_eq!(rig.names("stage").len(), 0);
}

#[test]
fn a_tree_made_by_an_older_extractor_is_extracted_again() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let old = rig.open(Config::default());
    let ready = old.ensure(&wanted(&l)).unwrap();
    drop(old);
    rig.fake.restart(&BTreeSet::new());
    let cfg = Config {
        extractor_version: 2,
        ..Config::default()
    };
    let store = rig.open(cfg);
    assert_eq!(rig.fake.accounted(), 10);
    let again = store.ensure(&wanted(&l)).unwrap();
    assert_eq!(again, ready);
    assert_eq!(rig.sealer.seal_count(), 2);
    assert_eq!(rig.fake.entries(), 1);
    assert!(!rig.sealer.unsealed.lock().unwrap().is_empty());
    let record = std::fs::read_to_string(
        rig.root()
            .join("trees")
            .join(format!("{}.sealed", l.diff_id.hex())),
    )
    .unwrap();
    assert!(record.contains("\"extractor_version\":2"));
    // The new tree is current: no third extraction.
    store.ensure(&wanted(&l)).unwrap();
    assert_eq!(rig.sealer.seal_count(), 2);
}

#[test]
fn a_stale_tree_that_a_run_pins_is_not_replaced() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let old = rig.open(Config::default());
    old.ensure(&wanted(&l)).unwrap();
    old.pin(&l.diff_id, &run("r1")).unwrap();
    drop(old);
    rig.fake.restart(&BTreeSet::new());
    let cfg = Config {
        extractor_version: 2,
        ..Config::default()
    };
    let store = rig.open_live(cfg, &BTreeSet::from([run("r1")]));
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Busy)));
    assert!(matches!(
        store.pin(&l.diff_id, &run("r2")),
        Err(Error::Absent(_))
    ));
    assert_eq!(rig.sealer.seal_count(), 1);
}

#[test]
fn a_run_pins_a_tree_once_and_ends_the_pin() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let store = rig.open(Config::default());
    assert!(matches!(
        store.pin(&l.diff_id, &run("r1")),
        Err(Error::Absent(_))
    ));
    store.ensure(&wanted(&l)).unwrap();
    let handle = store.pin(&l.diff_id, &run("r1")).unwrap();
    assert_eq!(handle.diff_id(), &l.diff_id);
    assert!(handle.path().join("f").is_file());
    let pin_file = rig.root().join("pins").join(l.diff_id.hex()).join("r1");
    assert!(pin_file.is_file());
    assert!(matches!(
        store.pin(&l.diff_id, &run("r1")),
        Err(Error::AlreadyPinned)
    ));
    assert_eq!(store.stats().pinned, 1);
    store.unpin(handle);
    assert!(!pin_file.exists());
    assert_eq!(store.stats().pinned, 0);
    store.pin(&l.diff_id, &run("r1")).unwrap();
}

#[test]
fn a_pin_file_that_cannot_be_made_is_an_io_error() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let store = rig.open(Config::default());
    store.ensure(&wanted(&l)).unwrap();
    // A file where the pin directory should be.
    std::fs::write(rig.root().join("pins").join(l.diff_id.hex()), b"").unwrap();
    assert!(matches!(
        store.pin(&l.diff_id, &run("r1")),
        Err(Error::Io { .. })
    ));
    // A pin file that is already there, though the store does not know it.
    std::fs::remove_file(rig.root().join("pins").join(l.diff_id.hex())).unwrap();
    let dir = rig.root().join("pins").join(l.diff_id.hex());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("r2"), b"").unwrap();
    assert!(matches!(
        store.pin(&l.diff_id, &run("r2")),
        Err(Error::Io { .. })
    ));
}

#[test]
fn run_ids_are_file_names() {
    for good in ["r1", "0", "a.b_c-d"] {
        assert!(js_treestore::RunId::new(good).is_ok());
    }
    let long = "a".repeat(129);
    for bad in ["", ".", "..", "-x", "_x", "A", "a/b", long.as_str()] {
        assert!(js_treestore::RunId::new(bad).is_err(), "{bad}");
    }
    assert_eq!(run("r1").as_str(), "r1");
}
