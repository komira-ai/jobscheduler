//! The `opwall/` row of an operation: the longest it may run, written in the transaction that
//! queues it, read back by a start and by a look at one operation, and removed in the
//! transaction that ends it. The bytes are the farm server's (`tests/golden/submit-rows.txt`
//! pins them for a whole store; the tests here pin the codec's edges).

mod common;

use std::sync::Arc;
use std::time::Duration;

use js_sched::Request;
use js_state::{Cond, Op, Store, Txn};
use js_store::keys;
use js_store::{Ending, Rows, StateError};
use js_types::sched::{LeaseId, LeaseKind, OperationId, Qos, Resources, WorkKey};

use common::{Racing, frontend_with};

/// A store, and a handle on it.
fn one() -> (Arc<Racing>, Rows) {
    let store = Racing::new();
    let codec = Arc::new(js_store::test_support::TestCodec::new());
    let rows = frontend_with(&store, 1, &codec);
    (store, rows)
}

/// The request for the work `tag`: no two tags are the same work.
fn request(tag: u8) -> Request {
    Request {
        key: WorkKey::new("main", format!("{tag:02x}/{tag}")),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    }
}

/// Queues operation `operation` (the work of the tag of its number) with the longest run
/// `wall`.
fn queue(rows: &Rows, operation: u64, wall: Option<Duration>) {
    let tag = u8::try_from(operation).expect("a small id");
    rows.submit_walled(OperationId(operation), &request(tag), &[], None, wall)
        .expect("submit");
}

fn ending() -> Ending {
    Ending::new(b"ended".to_vec(), Duration::from_secs(1))
}

/// The keys of the rows under `prefix`.
fn keys_under(store: &Racing, prefix: &str) -> Vec<String> {
    let rows = store.scan_prefix(prefix).expect("scan");
    rows.into_iter().map(|(key, _)| key).collect()
}

/// Replaces the row `key` with `value`, as a writer of its own.
fn overwrite(store: &Racing, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("no conflict");
}

/// Whether any operation of `txns` deletes a row under `prefix`.
fn deletes_under(txns: &[Txn], prefix: &str) -> bool {
    let mut ops = txns.iter().flat_map(Txn::ops);
    ops.any(|op| matches!(op, Op::Delete { key, .. } if key.starts_with(prefix)))
}

/// Asserts that a wall time is written in the transaction that queues the operation, as a
/// version byte, the seconds as eight bytes and the nanoseconds as four, both big-endian.
/// Catches: a row put in a transaction of its own (a crash between them loses the wall time of
/// an operation the client was told is queued), and any other layout (the farm server would
/// refuse the row or read another time).
#[test]
fn a_wall_time_is_one_row_in_the_submit_transaction_in_the_farm_servers_bytes() {
    let (store, rows) = one();
    let before = store.commit_stats().txns;

    queue(&rows, 4, Some(Duration::new(0x0102, 0x0304_0506)));

    assert_eq!(store.commit_stats().txns, before + 1);
    let row = store.get(&keys::opwall(OperationId(4))).expect("read");
    let row = row.expect("an opwall row");
    assert_eq!(row.value, [1, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(keys_under(&store, "opwall/"), ["opwall/0000000000000004"]);
}

/// Asserts that an operation submitted without a wall time leaves the rows a submit without
/// the parameter leaves. Catches: an `opwall/` row written for an operation that has no wall
/// time, or any other row changed by the new entry point.
#[test]
fn a_submit_without_a_wall_time_writes_the_rows_of_a_submit_without_the_parameter() {
    let (plain, plain_rows) = one();
    let (walled, walled_rows) = one();

    plain_rows
        .submit(OperationId(4), &request(4), &[1, 2], Some(7))
        .expect("submit");
    walled_rows
        .submit_walled(OperationId(4), &request(4), &[1, 2], Some(7), None)
        .expect("submit");

    let all = |store: &Racing| {
        let rows = store.scan_prefix("").expect("scan");
        rows.into_iter()
            .map(|(key, row)| (key, row.value))
            .collect::<Vec<_>>()
    };
    assert_eq!(all(&plain), all(&walled));
    assert!(keys_under(&walled, "opwall/").is_empty());
}

/// Asserts that a start and a look at one operation return the wall time of each operation
/// that has one, and none for an operation that has none, down to a zero wall time. Catches: a
/// wall time dropped on the load (a restarted frontend drains by the default), one operation's
/// time given to another, and a zero time read as no time.
#[test]
fn a_load_and_a_look_return_each_operations_own_wall_time() {
    let (_, rows) = one();
    let long = Duration::new(3_600, 250_000_000);
    queue(&rows, 3, Some(long));
    queue(&rows, 4, None);
    queue(&rows, 5, Some(Duration::ZERO));

    let loaded = rows.load().expect("load");

    let walls: Vec<_> = loaded.operations.iter().map(|op| op.max_wall).collect();
    assert_eq!(walls, [Some(long), None, Some(Duration::ZERO)]);
    for (operation, wall) in [(3, Some(long)), (4, None), (5, Some(Duration::ZERO))] {
        let looked = rows.stored_operation(OperationId(operation));
        assert_eq!(looked.expect("read").expect("held").max_wall, wall);
    }
}

/// Asserts that a request for work the store already holds an operation for joins that
/// operation and writes no wall time of its own. Catches: a joining request overwriting the
/// wall time of the operation it joined.
#[test]
fn a_request_that_joins_an_operation_leaves_its_wall_time() {
    let (store, rows) = one();
    queue(&rows, 4, Some(Duration::from_secs(60)));

    let again = rows
        .submit_walled(
            OperationId(5),
            &request(4),
            &[],
            None,
            Some(Duration::from_secs(999)),
        )
        .expect("submit");

    let js_store::Submitted::Joined(op) = again else {
        panic!("the request should join: {again:?}");
    };
    assert_eq!(op.max_wall, Some(Duration::from_secs(60)));
    assert_eq!(keys_under(&store, "opwall/"), ["opwall/0000000000000004"]);
}

/// Asserts that an `opwall/` row that is not a wall time is refused by a start and by a look
/// at the operation, whether its value is short, long, of another version or has a billion
/// nanoseconds, and that a key that names no operation is refused too. Catches: a bad row read
/// as some wall time (a drain deadline from garbage), and one skipped (the next release would
/// read it differently from this one).
#[test]
fn an_opwall_row_that_is_not_one_is_refused() {
    let (store, rows) = one();
    queue(&rows, 1, Some(Duration::new(0x0102, 0x0304_0506)));
    let row = [1, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6].to_vec();
    let key = keys::opwall(OperationId(1));

    let mut longer = row.clone();
    longer.push(0);
    let mut other_version = row.clone();
    other_version[0] = 2;
    let mut billion = row.clone();
    billion[9..].copy_from_slice(&1_000_000_000u32.to_be_bytes());
    let mut bad: Vec<Vec<u8>> = (0..row.len()).map(|cut| row[..cut].to_vec()).collect();
    bad.extend([longer, other_version, billion]);
    for value in bad {
        overwrite(&store, &key, value.clone());
        assert!(
            matches!(rows.load(), Err(StateError::Corrupt { .. })),
            "{value:?}"
        );
        let looked = rows.stored_operation(OperationId(1));
        assert!(
            matches!(looked, Err(StateError::Corrupt { .. })),
            "{value:?}"
        );
    }

    let mut just_under = row.clone();
    just_under[9..].copy_from_slice(&999_999_999u32.to_be_bytes());
    overwrite(&store, &key, just_under);
    let loaded = rows.load().expect("999999999 nanoseconds is a time");
    let wall = loaded.operations[0].max_wall;
    assert_eq!(wall, Some(Duration::new(0x0102, 999_999_999)));

    overwrite(&store, &key, row.clone());
    overwrite(&store, "opwall/not-an-id", row);
    assert!(matches!(rows.load(), Err(StateError::Corrupt { .. })));
}

/// Asserts that the result of an operation removes its `opwall/` row in the same transaction
/// as the operation's other rows, whether the result comes from the lease, from the queue (the
/// lease was given up in the same feed) or is a refusal. Catches: a row left behind for every
/// finished operation (the rows of a long-running store grow without end).
#[test]
fn ending_an_operation_removes_its_opwall_row() {
    let (store, rows) = one();
    let wall = Some(Duration::from_secs(60));
    for operation in [4, 5, 6] {
        queue(&rows, operation, wall);
    }
    let grant = |operation: u64, seq: u64| {
        use js_types::sched::usage::LeaseCost;
        use js_types::sched::{LeaseGrant, WorkerId};
        rows.granted(&LeaseGrant {
            lease: LeaseId::new(5, seq),
            operation: OperationId(operation),
            worker: WorkerId::new("node-a"),
            cost: LeaseCost {
                cpu_millis: 1_000,
                memory_bytes: 1 << 30,
                limit_bytes: 1 << 30,
                cold: false,
                retry: false,
                key_hash: 0,
            },
        })
        .expect("grant");
    };
    grant(4, 0);
    assert_eq!(keys_under(&store, "opwall/").len(), 3);

    rows.finished(OperationId(4), LeaseId::new(5, 0), &ending())
        .expect("finished from the lease");
    assert_eq!(keys_under(&store, "opwall/").len(), 2);

    rows.finished(OperationId(5), LeaseId::new(5, 1), &ending())
        .expect("finished from the queue");
    assert_eq!(keys_under(&store, "opwall/"), ["opwall/0000000000000006"]);

    rows.refused(OperationId(6), &ending()).expect("refused");
    assert!(keys_under(&store, "opwall/").is_empty());
}

/// Asserts that ending an operation that has no `opwall/` row deletes none: the transaction is
/// the one an operation without a wall time always ended in. Catches: a delete of a row that
/// is not there in the end of every operation (a different transaction for stores that never
/// had a wall time), and the same for an operation that has one: the delete is there.
#[test]
fn an_operation_without_a_wall_time_ends_without_deleting_an_opwall_row() {
    let (store, rows) = one();
    queue(&rows, 4, None);
    queue(&rows, 5, Some(Duration::from_secs(60)));
    store.asked();

    rows.refused(OperationId(4), &ending()).expect("refused");
    let plain = store.asked();
    rows.refused(OperationId(5), &ending()).expect("refused");
    let walled = store.asked();

    assert!(!deletes_under(&plain, "opwall/"));
    assert!(deletes_under(&walled, "opwall/"));
}
