//! Single flight on (diff id, blob digest).

use std::sync::atomic::Ordering;
use std::thread;

use crate::common::{Rig, layer, sized, tar_layer, wanted};
use js_treestore::Error;

#[test]
fn two_callers_for_one_blob_share_one_extraction() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let gate = rig.source.hold_fetches();
    let store = rig.open(rig.cfg());
    let want = wanted(&l);
    thread::scope(|scope| {
        let first = scope.spawn(|| store.ensure(&want));
        rig.source.wait_entered(1);
        let second = scope.spawn(|| store.ensure(&want));
        while store.stats().waiting < 1 {
            thread::yield_now();
        }
        assert_eq!(store.stats().in_flight, 1);
        gate.open();
        let a = first.join().unwrap().unwrap();
        let b = second.join().unwrap().unwrap();
        assert_eq!(a, b);
    });
    assert_eq!(rig.source.fetches(), 1);
    assert_eq!(rig.sealer.seal_count(), 1);
    assert_eq!(store.stats().trees, 1);
    assert_eq!(store.stats().waiting, 0);
}

#[test]
fn a_waiting_caller_tries_for_itself_after_the_leader_fails() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let gate = rig.source.hold_fetches();
    rig.source.0.fail_fetches.store(1, Ordering::SeqCst);
    let store = rig.open(rig.cfg());
    let want = wanted(&l);
    thread::scope(|scope| {
        let first = scope.spawn(|| store.ensure(&want));
        rig.source.wait_entered(1);
        let second = scope.spawn(|| store.ensure(&want));
        while store.stats().waiting < 1 {
            thread::yield_now();
        }
        gate.open();
        assert!(matches!(first.join().unwrap(), Err(Error::Fetch(_))));
        assert!(second.join().unwrap().is_ok());
    });
    assert_eq!(rig.source.fetches(), 2);
}

#[test]
fn a_failing_blob_is_not_shared_with_a_different_blob_for_the_same_diff_id() {
    let rig = Rig::new();
    let good = sized("f", 10, 1);
    let bad = sized("f", 10, 2);
    rig.have(&good);
    rig.have(&bad);
    let gate = rig.source.hold_fetches();
    let store = rig.open(rig.cfg());
    let honest = wanted(&good);
    // A blob that claims the good layer's diff id.
    let mut liar = wanted(&bad);
    liar.diff_id = good.diff_id.clone();
    thread::scope(|scope| {
        let a = scope.spawn(|| store.ensure(&liar));
        let b = scope.spawn(|| store.ensure(&honest));
        // Both are in flight at once: different blobs do not share a flight.
        rig.source.wait_entered(2);
        assert_eq!(store.stats().in_flight, 2);
        assert_eq!(store.stats().waiting, 0);
        gate.open();
        // The liar's mismatch is its own; the honest caller has its tree.
        assert!(matches!(a.join().unwrap(), Err(Error::Unpack(_))));
        assert!(b.join().unwrap().is_ok());
    });
    let data = std::fs::read(rig.root().join("trees").join(good.diff_id.hex()).join("f")).unwrap();
    assert_eq!(data, vec![1u8; 10]);
}

#[test]
fn two_blobs_of_one_tree_extract_alike_and_enter_the_table_once() {
    let rig = Rig::new();
    let files: &[(&str, &[u8])] = &[("f", b"same content")];
    let zstd = layer(files);
    let tar = tar_layer(files);
    assert_eq!(zstd.diff_id, tar.diff_id);
    assert_ne!(zstd.digest, tar.digest);
    rig.have(&zstd);
    rig.have(&tar);
    let gate = rig.source.hold_fetches();
    let store = rig.open(rig.cfg());
    let (a, b) = (wanted(&zstd), wanted(&tar));
    thread::scope(|scope| {
        let x = scope.spawn(|| store.ensure(&a));
        let y = scope.spawn(|| store.ensure(&b));
        rig.source.wait_entered(2);
        gate.open();
        assert_eq!(x.join().unwrap().unwrap(), y.join().unwrap().unwrap());
    });
    assert_eq!(rig.sealer.seal_count(), 2);
    assert_eq!(store.stats().trees, 1);
    assert_eq!(store.stats().used, 12);
    assert!(rig.names("stage").is_empty());
    assert!(rig.names("tmp").is_empty());
}
