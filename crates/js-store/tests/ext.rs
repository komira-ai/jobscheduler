//! The rows of an embedding server ([`Ext`]) as the test rows keep them: what they add to the
//! transactions this crate builds, and how they read back. Every implementation of the trait
//! keeps to the same rules, and the test rows are the example the other tests of this crate run
//! on.

mod common;

use std::time::Duration;

use js_state::{Cond, Op, Store, Txn};
use js_store::test_support::TestExt;
use js_store::{Ext, StateError, keys};
use js_types::sched::OperationId;

use common::Racing;

fn put(store: &Racing, txn: Txn) {
    store.txn(txn).expect("store").expect("applied");
}

/// The key every operation of `txn` names.
fn named(txn: &Txn) -> Vec<String> {
    let keys = txn.ops().iter().map(|op| op.key_cond());
    keys.map(|named| named.expect("a key").0.to_owned())
        .collect()
}

fn assert_not_owned(txn: &Txn) {
    for key in named(txn) {
        let owned = keys::ALL.iter().any(|prefix| key.starts_with(prefix));
        assert!(!owned, "the rows of the embedder include {key}");
    }
}

/// Asserts that the rows of a submission are put only if absent, in rows of the embedder's own
/// prefix, and that the parcel is read back with the operation, alone or among all.
#[test]
fn a_submitted_parcel_is_kept_in_a_row_of_its_own_and_read_back() {
    let store = Racing::new();
    let operation = OperationId(4);

    let txn = TestExt.submitted(Txn::new(), operation, &[1, 2, 3]);

    assert_not_owned(&txn);
    assert!(matches!(
        txn.ops(),
        [Op::Put {
            cond: Cond::Absent,
            ..
        }]
    ));
    put(&store, txn);
    put(&store, TestExt.submitted(Txn::new(), OperationId(5), &[9]));
    assert_eq!(
        TestExt.parcel(&*store, operation).expect("read"),
        Some(vec![1, 2, 3])
    );
    assert_eq!(TestExt.parcel(&*store, OperationId(6)).expect("read"), None);
    let parcels = TestExt.parcels(&*store).expect("read");
    let found: Vec<_> = parcels.into_iter().collect();
    assert_eq!(
        found,
        [(OperationId(4), vec![1, 2, 3]), (OperationId(5), vec![9])]
    );
    let again = store.txn(TestExt.submitted(Txn::new(), operation, &[7]));
    assert!(matches!(again, Ok(Err(_))), "a second submission was taken");
}

/// Asserts that the end of an operation removes its parcel, whatever the row's version, and
/// leaves the others.
#[test]
fn a_settled_operation_leaves_no_parcel() {
    let store = Racing::new();
    put(&store, TestExt.submitted(Txn::new(), OperationId(4), &[1]));
    put(&store, TestExt.submitted(Txn::new(), OperationId(5), &[2]));

    let txn = TestExt.settled(Txn::new(), OperationId(4));

    assert_not_owned(&txn);
    put(&store, txn);
    assert_eq!(TestExt.parcel(&*store, OperationId(4)).expect("read"), None);
    assert_eq!(
        TestExt.parcel(&*store, OperationId(5)).expect("read"),
        Some(vec![2])
    );
}

/// Asserts that the parcel kept with a result is put over whatever is there, expires with the
/// result's retention, and reads back by the operation's id.
#[test]
fn an_ended_parcel_is_kept_for_the_retention_of_the_result() {
    let store = Racing::new();
    let operation = OperationId(4);
    let ttl = Duration::from_secs(600);

    let txn = TestExt.ended(Txn::new(), operation, &[4, 5], ttl);
    assert_not_owned(&txn);
    put(&store, txn);
    put(&store, TestExt.ended(Txn::new(), operation, &[6], ttl));

    assert_eq!(
        TestExt.ended_parcel(&*store, operation).expect("read"),
        Some(vec![6])
    );
    assert_eq!(
        TestExt.ended_parcel(&*store, OperationId(5)).expect("read"),
        None
    );
    let row = store.get("ended-parcel/0000000000000004").expect("read");
    assert!(row.expect("a row").expires_at.is_some());
    store.advance(ttl + Duration::from_secs(1));
    store.reap_expired().expect("reaped");
    assert_eq!(
        TestExt.ended_parcel(&*store, operation).expect("read"),
        None
    );
}

/// Asserts that a row that is not a parcel is refused, naming the row, whichever way it is
/// read.
#[test]
fn a_row_that_is_not_a_parcel_is_refused_naming_it() {
    let store = Racing::new();
    let bad = |key: &str, value: Vec<u8>| {
        put(&store, Txn::new().put(key, value, Cond::Any));
    };
    bad("parcel/0000000000000004", vec![255]);
    bad("ended-parcel/0000000000000004", Vec::new());

    let one = TestExt.parcel(&*store, OperationId(4));
    let all = TestExt.parcels(&*store);
    let ended = TestExt.ended_parcel(&*store, OperationId(4));

    for (read, key) in [
        (one.map(|_| ()), "parcel/0000000000000004"),
        (all.map(|_| ()), "parcel/0000000000000004"),
        (ended.map(|_| ()), "ended-parcel/0000000000000004"),
    ] {
        let Err(StateError::Corrupt { key: named, .. }) = read else {
            panic!("{key} was not refused");
        };
        assert_eq!(named, key);
    }
    bad("parcel/zz", vec![1]);
    bad("parcel/0000000000000004", vec![1]);
    let all = TestExt.parcels(&*store);
    assert!(matches!(all, Err(StateError::Corrupt { .. })), "{all:?}");
}
