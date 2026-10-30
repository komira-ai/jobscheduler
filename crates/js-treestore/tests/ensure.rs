//! Making a tree from a layer blob, and pinning it for runs.

mod common;

use std::collections::BTreeSet;

use common::{Rig, SEAL_FAILS, SEAL_VANISHES, layer, run, sized, wanted};
use js_treestore::{Config, Digest, Error};

#[test]
fn a_layer_becomes_a_sealed_recorded_tree() {
    let rig = Rig::new();
    let l = layer(&[("bin/tool", b"#!/bin/sh\n"), ("data", b"hello")]);
    rig.have(&l);
    let store = rig.open(rig.cfg());
    let ready = store.ensure(&wanted(&l)).unwrap();
    assert_eq!(ready.diff_id, l.diff_id);
    assert_eq!(std::fs::read(ready.path.join("data")).unwrap(), b"hello");
    assert_eq!(rig.sealer.seal_count(), 1);
    assert_eq!(store.stats().used, 15);
    assert!(rig.present(&l.diff_id));
    assert!(rig.names("stage").is_empty());
    assert!(rig.names("tmp").is_empty());
}

#[test]
fn a_present_tree_is_not_fetched_again() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let store = rig.open(rig.cfg());
    let first = store.ensure(&wanted(&l)).unwrap();
    let second = store.ensure(&wanted(&l)).unwrap();
    assert_eq!(first, second);
    assert_eq!(rig.source.fetches(), 1);
    assert_eq!(rig.sealer.seal_count(), 1);
    assert_eq!(store.stats().trees, 1);
}

#[test]
fn a_blob_that_is_not_the_layer_its_diff_id_names_leaves_nothing() {
    let rig = Rig::new();
    let named = sized("f", 10, 1);
    let other = sized("f", 10, 2);
    rig.have(&other);
    let mut want = wanted(&other);
    want.diff_id = named.diff_id.clone();
    let store = rig.open(rig.cfg());
    assert!(matches!(store.ensure(&want), Err(Error::Unpack(_))));
    assert!(rig.names("stage").is_empty());
    assert!(rig.names("tmp").is_empty());
    assert!(rig.names("trees").is_empty());
    assert_eq!(store.stats().in_flight, 0);
}

#[test]
fn bytes_that_are_not_the_blob_asked_for_are_refused_before_they_are_read() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    let store = rig.open(rig.cfg());
    // The same length, other bytes: the digest differs.
    let mut wrong = l.bytes.clone();
    wrong[0] ^= 1;
    rig.source.add(&l.digest, wrong);
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Blob { .. })));
    // Fewer bytes than declared.
    rig.source
        .add(&l.digest, l.bytes[..l.bytes.len() - 1].to_vec());
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Blob { .. })));
    // More bytes than declared: the sink refuses past the cap.
    let mut long = l.bytes.clone();
    long.push(0);
    rig.source.add(&l.digest, long);
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Fetch(_))));
    assert_eq!(rig.sealer.seal_count(), 0);
    assert!(rig.names("stage").is_empty());
    assert!(rig.names("tmp").is_empty());
}

#[test]
fn a_refused_seal_leaves_nothing() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    rig.sealer.set(SEAL_FAILS);
    let store = rig.open(rig.cfg());
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Seal(_))));
    assert!(rig.names("stage").is_empty());
    assert!(rig.names("trees").is_empty());
    assert!(rig.names("tmp").is_empty());
}

#[test]
fn a_blob_the_source_cannot_give_is_an_error() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    let store = rig.open(rig.cfg());
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Fetch(_))));
    assert!(rig.names("tmp").is_empty());
}

#[test]
fn a_stage_that_vanishes_before_the_rename_is_an_io_error() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    rig.sealer.set(SEAL_VANISHES);
    let store = rig.open(rig.cfg());
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Io { .. })));
}

#[test]
fn a_tree_made_by_an_older_extractor_is_extracted_again() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let old = rig.open(rig.cfg());
    let ready = old.ensure(&wanted(&l)).unwrap();
    drop(old);
    let cfg = Config {
        extractor_version: 2,
        ..rig.cfg()
    };
    let store = rig.open(cfg);
    let again = store.ensure(&wanted(&l)).unwrap();
    assert_eq!(again, ready);
    assert_eq!(rig.sealer.seal_count(), 2);
    assert_eq!(store.stats().trees, 1);
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
    let old = rig.open(rig.cfg());
    old.ensure(&wanted(&l)).unwrap();
    old.pin(&l.diff_id, &run("r1")).unwrap();
    drop(old);
    let cfg = Config {
        extractor_version: 2,
        ..rig.cfg()
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
    let store = rig.open(rig.cfg());
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
    let store = rig.open(rig.cfg());
    store.ensure(&wanted(&l)).unwrap();
    let dir = rig.root().join("pins").join(l.diff_id.hex());
    // A file where the pin directory should be.
    std::fs::write(&dir, b"").unwrap();
    assert!(matches!(
        store.pin(&l.diff_id, &run("r1")),
        Err(Error::Io { .. })
    ));
    // A pin file that is already there, though the store does not know it.
    std::fs::remove_file(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("r2"), b"").unwrap();
    assert!(matches!(
        store.pin(&l.diff_id, &run("r2")),
        Err(Error::Io { .. })
    ));
}

#[test]
fn run_ids_and_digests_are_checked() {
    for good in ["r1", "0", "a.b_c-d"] {
        assert!(js_treestore::RunId::new(good).is_ok());
    }
    let long = "a".repeat(129);
    for bad in ["", ".", "..", "-x", "_x", "A", "a/b", long.as_str()] {
        assert!(js_treestore::RunId::new(bad).is_err(), "{bad}");
    }
    assert_eq!(run("r1").as_str(), "r1");
    let ok = format!("sha256:{}", "a".repeat(64));
    assert_eq!(Digest::parse(&ok).unwrap().as_str(), ok);
    assert_eq!(Digest::parse(&ok).unwrap().to_string(), ok);
    for bad in ["md5:00".to_owned(), format!("sha256:{}", "g".repeat(64))] {
        assert!(Digest::parse(&bad).is_err(), "{bad}");
    }
}
