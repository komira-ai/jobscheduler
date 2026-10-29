//! Finding the lease of an operation: through its `lease-by-op/` row, without reading every
//! `lease/` row, and as before when the row is missing or does not stand (a lease granted
//! by a release that wrote none).

mod common;

use std::sync::Arc;
use std::time::Duration;

use js_sched::Request;
use js_state::{Cond, Store, Txn};
use js_store::test_support::TestCodec;
use js_store::{Ending, Rows, StateError, Submitted};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Resources, WorkKey, WorkerId,
};

use common::{Racing, frontend_with};

const INDEX_4: &str = "lease-by-op/0000000000000004";

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

fn opened() -> (Arc<Racing>, Rows) {
    let store = Racing::new();
    let rows = frontend_with(&store, 1, &Arc::new(TestCodec::new()));
    (store, rows)
}

fn overwrite(store: &Racing, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

fn remove(store: &Racing, key: &str) {
    let gone = Txn::new().delete(key, Cond::Any);
    store.txn(gone).expect("store").expect("delete");
}

fn count(store: &Racing, prefix: &str) -> usize {
    store.scan_prefix(prefix).expect("scan").len()
}

fn value_of(store: &Racing, key: &str) -> Vec<u8> {
    let row = store.get(key).expect("read");
    row.expect("a row").value
}

/// The 16-byte row of two numbers, big-endian.
fn pair_row(high: u64, low: u64) -> Vec<u8> {
    [high.to_be_bytes(), low.to_be_bytes()].concat()
}

/// Submits `request(tag)` as operation `operation`, which joins an operation of that action
/// the store holds.
fn submit(rows: &Rows, operation: u64, tag: u8) -> Result<Submitted, StateError> {
    rows.submit(OperationId(operation), &request(tag), &[], None)
}

/// Grants lease `(5, seq)` of operation `operation` to `worker`.
fn grant(rows: &Rows, operation: u64, seq: u64, worker: &str) -> Result<(), StateError> {
    rows.granted(&LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new(worker),
        cost: LeaseCost {
            cpu_millis: 1_000,
            ..LeaseCost::default()
        },
    })
}

/// The lease the operation a request joined holds; `None` for a request that did not join,
/// and for an operation that holds none.
fn lease_found(submitted: Result<Submitted, StateError>) -> Option<LeaseId> {
    match submitted.expect("accepted") {
        Submitted::Joined(op) => op.lease.map(|grant| grant.lease),
        Submitted::Queued(_) => None,
    }
}

/// Operation 4 of `request(1)`, running under lease `(5, 7)` on `node-a`.
fn running(rows: &Rows) {
    submit(rows, 4, 1).expect("queued");
    grant(rows, 4, 7, "node-a").expect("a grant");
}

/// Asserts that a grant puts the lease id under the key of its operation, eight bytes of
/// term and eight of sequence, big-endian, pinned byte for byte because another release
/// reads it.
#[test]
fn a_grant_puts_the_lease_id_under_the_key_of_the_operation() {
    let (store, rows) = opened();

    running(&rows);

    let row = value_of(&store, INDEX_4);
    assert_eq!(row, [0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 7]);
}

/// Asserts that a request that joins a running operation finds its lease through the index,
/// and one that joins a queued operation finds none, and that neither reads the `lease/` rows;
/// and that a request for an action nothing runs queues.
#[test]
fn a_joined_request_finds_its_lease_without_reading_the_lease_rows() {
    let (store, rows) = opened();
    running(&rows);
    submit(&rows, 5, 2).expect("queued");
    let scans = store.lease_scans();

    let on_a_node = lease_found(submit(&rows, 9, 1));
    let waiting = lease_found(submit(&rows, 9, 2));
    let fresh = lease_found(submit(&rows, 9, 3));

    assert_eq!(on_a_node, Some(LeaseId::new(5, 7)));
    assert_eq!((waiting, fresh), (None, None));
    assert_eq!(store.lease_scans(), scans, "the lease rows were read");
}

/// Asserts that a request that joins an operation whose lease has no index row (a release
/// that wrote none granted it) still finds the lease, by reading the `lease/` rows.
#[test]
fn a_lease_with_no_index_row_is_found_by_reading_the_lease_rows() {
    let (store, rows) = opened();
    running(&rows);
    remove(&store, INDEX_4);
    let scans = store.lease_scans();

    let found = lease_found(submit(&rows, 9, 1));

    assert_eq!(found, Some(LeaseId::new(5, 7)));
    assert_eq!(store.lease_scans(), scans + 1);
}

/// Asserts that an index row that names a lease that is not there (left by a release that
/// ended the lease and did not delete it) is not taken for the operation holding none: the
/// lookup reads the `lease/` rows.
#[test]
fn an_index_row_naming_a_lease_that_is_gone_is_not_believed() {
    let (store, rows) = opened();
    running(&rows);
    overwrite(&store, INDEX_4, pair_row(5, 99));
    let scans = store.lease_scans();

    let found = lease_found(submit(&rows, 9, 1));

    assert_eq!(found, Some(LeaseId::new(5, 7)));
    assert_eq!(store.lease_scans(), scans + 1);
}

/// Asserts that an index row that names the lease of another operation is not believed
/// either: the operation's own lease is found.
#[test]
fn an_index_row_naming_the_lease_of_another_operation_is_not_believed() {
    let (store, rows) = opened();
    running(&rows);
    submit(&rows, 5, 2).expect("queued");
    grant(&rows, 5, 8, "node-a").expect("a grant");
    overwrite(&store, INDEX_4, pair_row(5, 8));

    let found = lease_found(submit(&rows, 9, 1));

    assert_eq!(found, Some(LeaseId::new(5, 7)));
}

/// Asserts that an index row that is not a lease id is refused naming the row.
#[test]
fn an_index_row_that_is_not_a_lease_id_is_refused_naming_it() {
    let (store, rows) = opened();
    running(&rows);
    overwrite(&store, INDEX_4, vec![1, 2, 3]);

    let refused = submit(&rows, 9, 1).expect_err("a damaged index row");

    let said = refused.to_string();
    assert!(said.contains(INDEX_4), "{said}");
    assert!(said.contains("not a 16-byte lease id"), "{said}");
}

/// Asserts that ending an operation deletes the index row whoever left it: a refusal of an
/// operation that holds no lease deletes a row a release that predates the index left.
#[test]
fn ending_an_operation_deletes_a_stale_index_row() {
    let (store, rows) = opened();
    submit(&rows, 4, 1).expect("queued");
    overwrite(&store, INDEX_4, pair_row(5, 7));
    let ended = Ending::new(b"refused".to_vec(), Duration::from_secs(1));

    rows.refused(OperationId(4), &ended).expect("refused");

    assert_eq!(count(&store, "lease-by-op/"), 0);
}

/// Asserts that a grant puts its index row whatever is there: the row of a lease a release
/// that predates the index left is written over, not a reason to refuse the grant.
#[test]
fn a_grant_puts_its_index_row_over_whatever_is_there() {
    let (store, rows) = opened();
    submit(&rows, 4, 1).expect("queued");
    overwrite(&store, INDEX_4, pair_row(3, 3));

    grant(&rows, 4, 7, "node-a").expect("a grant");

    assert_eq!(value_of(&store, INDEX_4), pair_row(5, 7));
}
