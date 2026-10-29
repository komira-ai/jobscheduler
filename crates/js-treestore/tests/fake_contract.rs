//! The callback rules of the blob store's contract, as the fake enforces them, and what
//! the tree store's callback does under them.

mod common;

use std::sync::{Arc, Mutex};

use common::{Rig, run, sized, wanted};
use js_treestore::Config;
use js_treestore::blobstore::{
    AccountHandle, BlobKey, BlobStore, Busy, Holder, Principal, Release, Source, StoreError, Wanted,
};

struct Greedy {
    store: Arc<common::FakeStore>,
    seen: Mutex<Vec<String>>,
}

impl Release for Greedy {
    fn release(&self) -> Result<u64, Busy> {
        let holder = Holder::new("greedy").unwrap();
        let key = BlobKey::parse(&format!("sha256:{}", "0".repeat(64))).unwrap();
        let want = Wanted {
            key: key.clone(),
            size: None,
            max_size: 1,
            source: Source("none".into()),
            principal: Principal("lease-1".into()),
        };
        let release: Arc<dyn Release> = Arc::new(Gone);
        let mut seen = self.seen.lock().unwrap();
        let results = [
            self.store.ensure(&want, &holder).map(|_| ()),
            self.store.ensure_all(&[want], &holder).map(|_| ()),
            self.store
                .put(&key, 0, &mut std::io::empty(), &holder)
                .map(|_| ()),
            self.store.release(&holder),
            self.store.account(&holder, "x", 1, 0, release).map(|_| ()),
        ];
        for result in results {
            seen.push(match result {
                Err(StoreError::Reentrant) => "reentrant".to_owned(),
                other => format!("{other:?}"),
            });
        }
        Err(Busy)
    }
}

struct Gone;

impl Release for Gone {
    fn release(&self) -> Result<u64, Busy> {
        Ok(0)
    }
}

struct Frees(u64);

impl Release for Frees {
    fn release(&self) -> Result<u64, Busy> {
        Ok(self.0)
    }
}

#[test]
fn a_release_that_calls_back_into_the_store_gets_reentrant() {
    let rig = Rig::new();
    let owner = Holder::new("owner").unwrap();
    let greedy = Arc::new(Greedy {
        store: rig.fake.clone(),
        seen: Mutex::new(Vec::new()),
    });
    rig.fake
        .account(&owner, "t", 10, 0, greedy.clone())
        .unwrap();
    assert_eq!(rig.fake.pressure(0), 10);
    assert_eq!(*greedy.seen.lock().unwrap(), ["reentrant"; 5]);
    // Outside a callback the same calls work.
    let _ = rig.fake.account(&owner, "t", 1, 0, Arc::new(Gone)).unwrap();
}

#[test]
fn an_entry_is_removed_once_by_release_ok_or_drop_entry() {
    let rig = Rig::new();
    let owner = Holder::new("owner").unwrap();
    let a = rig
        .fake
        .account(&owner, "t", 10, 0, Arc::new(Frees(10)))
        .unwrap();
    let _b = rig
        .fake
        .account(&owner, "t", 20, 5, Arc::new(Gone))
        .unwrap();
    assert_eq!(rig.fake.accounted(), 30);
    // Release returned Ok for the entry: it is gone; its own drop_entry afterwards removes
    // nothing more.
    assert_eq!(rig.fake.pressure(25), 0);
    assert_eq!(rig.fake.accounted(), 20);
    a.drop_entry();
    a.drop_entry();
    assert_eq!(rig.fake.accounted(), 20);
    // The other way round: drop_entry first, and the entry is not asked again.
    let c = rig
        .fake
        .account(&owner, "t", 7, 0, Arc::new(Frees(7)))
        .unwrap();
    c.drop_entry();
    assert_eq!(rig.fake.accounted(), 20);
    c.update(99);
    c.touch();
    assert_eq!(rig.fake.accounted(), 20);
}

#[test]
fn the_tree_stores_release_on_a_pinned_tree_is_busy() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let store = rig.open(Config::default());
    store.ensure(&wanted(&l)).unwrap();
    let handle = store.pin(&l.diff_id, &run("r1")).unwrap();
    assert_eq!(rig.fake.first_release().release(), Err(Busy));
    store.unpin(handle);
    assert_eq!(rig.fake.first_release().release(), Ok(10));
}

#[test]
fn updating_a_handle_changes_the_bytes_counted() {
    let rig = Rig::new();
    let owner = Holder::new("owner").unwrap();
    let handle = rig
        .fake
        .account(&owner, "t", 10, 0, Arc::new(Gone))
        .unwrap();
    handle.update(40);
    assert_eq!(rig.fake.accounted(), 40);
    assert_eq!(rig.fake.stats().accounted, 40);
    let long = "a".repeat(129);
    for bad in ["Bad Label", "", long.as_str()] {
        assert!(Holder::new(bad).is_err(), "{bad}");
    }
    assert!(BlobKey::parse("md5:00").is_err());
    assert!(BlobKey::parse(&format!("sha256:{}", "g".repeat(64))).is_err());
    assert_eq!(
        BlobKey::parse(&format!("sha256:{}", "a".repeat(64)))
            .unwrap()
            .to_string(),
        format!("sha256:{}", "a".repeat(64))
    );
    assert_eq!(Holder::new("ok").unwrap().as_str(), "ok");
}

#[test]
fn ending_a_lease_leaves_no_admission_for_it_and_ends_its_pins() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let store = rig.open(Config::default());
    store.ensure(&wanted(&l)).unwrap();
    let other = Principal("lease-2".into());
    let mut second = wanted(&l);
    second.blob.principal = other.clone();
    rig.fake
        .ensure(&second.blob, &Holder::new("other").unwrap())
        .unwrap();
    let lease = Principal("lease-1".into());
    assert_eq!(rig.fake.admissions_of(&lease), 1);
    let handle = store.pin(&l.diff_id, &run("job-1")).unwrap();
    let pin_file = rig.root().join("pins").join(l.diff_id.hex()).join("job-1");
    store.end_lease(&lease, vec![handle]);
    assert_eq!(rig.fake.admissions_of(&lease), 0);
    assert_eq!(rig.fake.admissions_of(&other), 1);
    assert!(!pin_file.exists());
    assert_eq!(store.stats().pinned, 0);
    // A put is admitted for its lease too.
    let holder = Holder::new("putter").unwrap();
    let key = BlobKey::parse(&format!("sha256:{}", "1".repeat(64))).unwrap();
    rig.fake
        .put(&key, 0, &mut std::io::empty(), &holder, &lease)
        .unwrap();
    assert_eq!(rig.fake.admissions_of(&lease), 1);
    rig.fake.end_principal(&lease);
    assert_eq!(rig.fake.admissions_of(&lease), 0);
}
