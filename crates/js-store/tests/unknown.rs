//! Rows a later binary wrote, read back as a start reads them: a row that holds a command
//! this binary does not know is skipped, and the rows it can read load. What the log says
//! about such rows is tested in `skip_log.rs`, alone in its binary.

mod common;

use std::sync::Arc;

use js_sched::Request;
use js_state::{Cond, Store, Txn};
use js_store::test_support::TestCodec;
use js_store::{RowCodec, Rows, StateError};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Resources, WorkKey, WorkerId,
};

use common::{Racing, frontend_with};

const OP_KNOWN: &str = "op/0000000000000001";
const OP_UNKNOWN: &str = "op/0000000000000002";
const LEASE_KNOWN: &str = "lease/0000000000000005.0000000000000000";
const LEASE_UNKNOWN: &str = "lease/0000000000000005.0000000000000001";

/// A store, the codec its frontend writes with, and the frontend.
fn opened() -> (Arc<Racing>, Arc<TestCodec>, Rows) {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let rows = frontend_with(&store, 100, &codec);
    (store, codec, rows)
}

fn put(store: &Racing, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

/// A value the test codec reads as one a later binary wrote.
fn unknown() -> Vec<u8> {
    b"later command".to_vec()
}

fn request(tag: u8) -> Request {
    Request {
        key: WorkKey::new("main", format!("{tag:02x}/{tag}")),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
        whole_worker: false,
    }
}

fn grant() -> LeaseGrant {
    LeaseGrant {
        lease: LeaseId::new(5, 0),
        operation: OperationId(1),
        worker: WorkerId::new("node-a"),
        cost: LeaseCost::default(),
    }
}

/// Asserts that a start skips an `op/` row and a `lease/` row that hold a command this
/// binary does not know, and loads the operation and the lease whose rows it can read.
#[test]
fn unknown_rows_are_skipped_and_the_known_rows_load() {
    let (store, codec, rows) = opened();
    put(&store, OP_KNOWN, codec.submit(&request(1)).expect("encode"));
    put(&store, OP_UNKNOWN, unknown());
    put(&store, LEASE_KNOWN, codec.lease(&grant()).expect("encode"));
    put(&store, LEASE_UNKNOWN, unknown());

    let stored = rows.load().expect("the rows load");

    let loaded: Vec<_> = stored
        .operations
        .iter()
        .map(|op| (op.operation, op.lease.clone()))
        .collect();
    assert_eq!(loaded, [(OperationId(1), Some(grant()))]);
}

/// Asserts that a single `op/` row a write depends on is refused, naming the row, when it
/// holds a command this binary does not know: only a scan skips such a row.
#[test]
fn an_unknown_row_a_write_depends_on_is_refused_naming_it() {
    let (store, _, rows) = opened();
    put(&store, OP_UNKNOWN, unknown());
    let Err(StateError::Corrupt { key, why }) = rows.unpin(Txn::new(), OperationId(2)) else {
        panic!("the row was not refused");
    };
    assert_eq!(key, OP_UNKNOWN);
    assert_eq!(why, "a command this binary does not know");
}
