//! What a start reads of the state store (the unfinished operations, each with its lease, loss
//! count, fate, parcel and size key, the first ids to hand out and the cordons), how a lease is
//! found for an operation another request joins, and the reads an intake poll makes of the
//! queue.

mod common;

use std::sync::Arc;
use std::time::Duration;

use js_sched::{Fate, Request};
use js_state::{Cond, Store, Txn};
use js_store::test_support::TestCodec;
use js_store::{
    EpochPolicy, ID_BLOCK, RowCodec, Rows, StateError, Stored, StoredCordon, StoredOperation,
    Submitted, keys,
};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    FarmFault, FarmTime, LeaseGrant, LeaseId, LeaseKind, LostWhy, MemoryKill, MemoryRun,
    OperationId, Qos, Resources, WaiterId, WorkKey, WorkerId,
};

use common::{Racing, frontend_with, open};

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

/// A store, the codec its frontends share, and a handle for each of the frontends numbered
/// `ids`.
fn frontends<const N: usize>(ids: [u64; N]) -> (Arc<Racing>, Arc<TestCodec>, [Rows; N]) {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let handles = ids.map(|id| frontend_with(&store, id, &codec));
    (store, codec, handles)
}

fn overwrite(store: &Racing, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

fn remove(store: &Racing, key: &str) {
    let gone = Txn::new().delete(key, Cond::Any);
    store.txn(gone).expect("store").expect("delete");
}

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

/// Puts the lease `(5, seq)` of `operation` on `worker` in the store, as a grant leaves it: the
/// queue row gone, the lease row, and (if `indexed`) the `lease-by-op/` row.
fn lease(store: &Racing, codec: &TestCodec, operation: u64, seq: u64, worker: &str, indexed: bool) {
    let grant = LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new(worker),
        cost: LeaseCost::default(),
    };
    let row = codec.lease(&grant).expect("encode");
    let mut txn = Txn::new()
        .delete(keys::queue(grant.operation), Cond::Any)
        .put(keys::lease(grant.lease), row, Cond::Any);
    if indexed {
        let value = [5_u64.to_be_bytes(), seq.to_be_bytes()].concat();
        txn = txn.put(keys::lease_by_op(grant.operation), value, Cond::Any);
    }
    store.txn(txn).expect("store").expect("the grant");
}

fn grant_of(operation: u64, seq: u64) -> LeaseGrant {
    LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: node(),
        cost: LeaseCost::default(),
    }
}

/// Asserts that an operation is written at submit (a restart remembers work the client was
/// told is queued), that its place in the queue, its parcel and the block of ids it was
/// numbered in are kept (a start numbers past the block), and that there are no lease
/// sequences yet.
#[test]
fn a_submitted_operation_is_loaded_queued() {
    let (_, _, [rows]) = frontends([1]);
    let id = rows.reserve_operation(OperationId(4)).expect("an id");
    assert_eq!(id, OperationId(4));
    rows.submit(id, &request(1), &[1, 2], None).expect("submit");

    let stored = rows.load().expect("load");

    assert_eq!(
        stored,
        Stored {
            next_operation: 4 + ID_BLOCK,
            next_seq: 0,
            operations: vec![StoredOperation {
                operation: OperationId(4),
                request: request(1),
                lease: None,
                cost_kept: true,
                losses: 0,
                fate: None,
                parcel: Some(vec![1, 2]),
                size_key: None,
                max_wall: None,
            }],
            floors: Vec::new(),
            key_floors: Vec::new(),
            key_names: Vec::new(),
            cordons: Vec::new(),
        }
    );
}

/// Asserts that a load returns the unfinished operations in id order, whatever order they
/// were submitted in, with the cordons and the first lease sequence.
#[test]
fn operations_load_in_id_order_with_the_cordons_and_the_sequences() {
    let (_, _, [rows]) = frontends([1]);
    for (id, tag) in [(5, 5), (3, 3), (4, 4)] {
        rows.submit(OperationId(id), &request(tag), &[], None)
            .expect("submit");
    }
    rows.reserve_seqs(7).expect("sequences");
    rows.cordoned(&node(), Some(StoredCordon::Cordoned))
        .expect("a cordon");

    let stored = rows.load().expect("load");

    let ids: Vec<_> = stored.operations.iter().map(|op| op.operation.0).collect();
    assert_eq!(ids, [3, 4, 5]);
    assert_eq!(stored.next_seq, 7 + ID_BLOCK);
    assert_eq!(stored.cordons, [(node(), StoredCordon::Cordoned)]);
}

/// Catches: an operation whose writer recorded no parcel (an older build) failing the load
/// instead of loading with none.
#[test]
fn an_operation_without_a_parcel_loads_with_none() {
    let (store, _, [rows]) = frontends([1]);
    rows.submit(OperationId(4), &request(1), &[1], None)
        .expect("submit");
    remove(&store, "parcel/0000000000000004");

    let stored = rows.load().expect("load");

    assert_eq!(stored.operations[0].parcel, None);
}

/// Asserts that a load returns an operation's lease with it, and leaves a lease whose
/// operation has no `op/` row out.
#[test]
fn a_lease_loads_with_its_operation() {
    let (store, codec, [rows]) = frontends([1]);
    rows.submit(OperationId(1), &request(1), &[], None)
        .expect("submit");
    rows.submit(OperationId(2), &request(2), &[], None)
        .expect("submit");
    lease(&store, &codec, 1, 0, "node-a", true);
    lease(&store, &codec, 9, 1, "node-a", true);

    let stored = rows.load().expect("load");

    let held: Vec<_> = stored
        .operations
        .iter()
        .map(|op| (op.operation.0, op.lease.clone()))
        .collect();
    assert_eq!(held, [(1, Some(grant_of(1, 0))), (2, None)]);
}

/// The size key of an operation is a row of its own, written with the submission: it reads
/// back with the operation, leaves the request row what an unkeyed operation's is (an older
/// reader sees the same request), and an operation given no key has no row.
#[test]
fn a_size_key_is_a_row_of_its_own_written_with_the_submission_and_read_with_the_operation() {
    let (store, codec, [rows]) = frontends([1]);
    // Two runs of one request that cannot join each other.
    let mut networked = request(1);
    networked.hermetic = false;
    rows.submit(OperationId(1), &networked, &[], None)
        .expect("submit");
    rows.submit(OperationId(2), &networked, &[], Some(7))
        .expect("submit");
    let opkeys: Vec<String> = store
        .scan_prefix("opkey/")
        .expect("scan")
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(opkeys, ["opkey/0000000000000002"]);
    let requests: Vec<_> = store
        .scan_prefix("op/")
        .expect("scan")
        .into_iter()
        .map(|(key, row)| codec.read(&key, &row.value).expect("a request"))
        .collect();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1], "the request is the same");

    let stored = rows.load().expect("load");

    let found: Vec<_> = stored.operations.iter().map(|o| o.size_key).collect();
    assert_eq!(found, [None, Some(7)]);
}

/// Catches a size key row read as anything but what was written: another version, a row cut
/// short or with bytes after it, a name that is not an operation id.
#[test]
fn an_opkey_row_that_is_not_one_is_refused() {
    let (store, _, [rows]) = frontends([1]);
    rows.submit(OperationId(1), &request(1), &[], None)
        .expect("submit");
    let row = [1, 1, 2, 3, 4, 5, 6, 7, 8].to_vec();
    overwrite(&store, "opkey/0000000000000001", row.clone());
    let stored = rows.load().expect("a size key");
    assert_eq!(stored.operations[0].size_key, Some(0x0102_0304_0506_0708));

    let mut longer = row.clone();
    longer.push(0);
    let mut other_version = row.clone();
    other_version[0] = 2;
    let mut bad: Vec<Vec<u8>> = (0..row.len()).map(|cut| row[..cut].to_vec()).collect();
    bad.extend([longer, other_version]);
    for value in bad {
        overwrite(&store, "opkey/0000000000000001", value.clone());
        assert!(rows.load().is_err(), "{value:?}");
        let looked = rows.stored_operation(OperationId(1));
        assert!(looked.is_err(), "{value:?}");
    }

    overwrite(&store, "opkey/0000000000000001", row.clone());
    overwrite(&store, "opkey/not-an-id", row);
    assert!(rows.load().is_err());
}

/// Asserts that size keys are read unless the reader says not to: a row of an earlier run
/// that cannot be read neither fails a start nor is loaded then, and fails it again once the
/// keys are read.
#[test]
fn size_keys_are_read_unless_the_reader_says_not_to() {
    let (store, _, [rows]) = frontends([1]);
    rows.submit(OperationId(1), &request(1), &[], Some(7))
        .expect("submit");
    overwrite(&store, "opkey/0000000000000001", vec![255]);

    rows.set_read_size_keys(false);
    let stored = rows.load().expect("size keys are not read");
    assert_eq!(stored.operations[0].size_key, None);
    let looked = rows.stored_operation(OperationId(1)).expect("not read");
    assert_eq!(looked.expect("an operation").size_key, None);

    rows.set_read_size_keys(true);
    assert!(rows.load().is_err());
}

/// The release before size keys reads a store holding `opkey/` and `keyfloor/` rows and
/// ignores them: the scheduler reads rows by prefix, and neither prefix is one of the others
/// (nor begins with it), so every scan that release makes sees the store as it was, and
/// everything else (an operation's request among them) is in the rows it always was in. The
/// store here has an operation, a lease, a floor and both new kinds of row.
#[test]
fn a_store_with_size_key_rows_reads_as_it_did_without_them() {
    let (store, codec, [rows]) = frontends([1]);
    rows.submit(OperationId(1), &request(1), &[], Some(7))
        .expect("submit");
    lease(&store, &codec, 1, 0, "node-a", true);
    overwrite(&store, "floor/6d61696e/01/1", vec![1]);
    overwrite(&store, "keyfloor/0000000000000007", vec![1]);
    let all: Vec<String> = store
        .scan_prefix("")
        .expect("scan")
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert!(all.iter().any(|key| key.starts_with("opkey/")));
    assert!(all.iter().any(|key| key.starts_with("keyfloor/")));

    // The prefixes the release without the feature scans.
    let known = [
        "op/",
        "queue/",
        "lease/",
        "session/",
        "loss/",
        "fate/",
        "floor/",
        "parcel/",
        "done/",
        "cordon/",
        "inflight/",
    ];
    let new = |key: &String| key.starts_with("opkey/") || key.starts_with("keyfloor/");
    for prefix in known {
        let scanned = store.scan_prefix(prefix).expect("scan");
        assert!(
            scanned.iter().all(|(key, _)| !new(key)),
            "a scan of {prefix} meets a new row"
        );
    }
    // What those scans find is what a store without the feature holds: the request reads as
    // a request.
    let ops = store.scan_prefix("op/").expect("scan");
    assert!(matches!(
        codec.read(&ops[0].0, &ops[0].1.value),
        Ok(js_store::Value::Submit(_))
    ));
}

/// Catches: a row the scheduler cannot read skipped (work silently lost at a restart) instead
/// of refused, and a refusal that does not name the row.
#[test]
fn an_unreadable_row_is_refused_naming_it() {
    let (store, codec, [rows]) = frontends([1]);
    let submit = codec.submit(&request(1)).expect("encode");
    let lease = codec.lease(&grant_of(1, 0)).expect("encode");
    let big = u64::MAX.to_be_bytes().to_vec();
    let cases: [(&str, Vec<u8>, &str); 14] = [
        (
            "op/0000000000000001",
            b"junk".to_vec(),
            "not a row of the test codec",
        ),
        ("op/0000000000000001", vec![0xff, 0xff], "not text"),
        ("op/0000000000000001", lease.clone(), "not a request"),
        ("op/zz", submit.clone(), "not an operation id"),
        (
            "lease/0000000000000005.0000000000000000",
            submit,
            "not a lease grant",
        ),
        ("loss/zz", vec![0; 8], "not an operation id"),
        ("loss/0000000000000001", vec![1, 2], "not an 8-byte number"),
        ("loss/0000000000000001", big, "not a loss count"),
        ("fate/zz", vec![0; 8], "not an operation id"),
        (
            "fate/0000000000000001",
            vec![255],
            "not a version 1 or 2 fate",
        ),
        ("opkey/zz", vec![1; 9], "not an operation id"),
        ("meta/next_op", vec![1, 2, 3], "not an 8-byte number"),
        ("meta/next_seq", vec![1, 2, 3], "not an 8-byte number"),
        ("cordon/node-b", vec![7], "not a cordon"),
    ];
    for (key, value, why) in cases {
        overwrite(&store, key, value);
        let error = rows.load().expect_err(key);
        let text = error.to_string();
        assert!(text.contains(key) && text.contains(why), "{key}: {text}");
        remove(&store, key);
    }
}

/// The row of a version 2 fate with one run killed for memory and nothing else.
fn fate_row() -> Vec<u8> {
    [
        &[2][..],
        &(16_u64 << 30).to_be_bytes(),
        &1_u32.to_be_bytes(),
        &6_u16.to_be_bytes(),
        b"node-a",
        &(4_u64 << 30).to_be_bytes(),
        &[0],
        &[0; 12],
    ]
    .concat()
}

/// What a start finds of the fate row `row` of operation 0: the fate, or why not.
fn fate_found(row: Vec<u8>) -> Result<Option<Fate>, String> {
    let (store, _, [rows]) = frontends([1]);
    rows.submit(OperationId(0), &request(1), &[], None)
        .expect("submit");
    overwrite(&store, "fate/0000000000000000", row);
    let stored = rows.load().map_err(|error| error.to_string())?;
    Ok(stored.operations[0].fate.clone())
}

/// Catches: a fate row read as anything but what was written: another version, a row cut
/// short, bytes after the last field, an unknown kind of kill, of farm fault or of loss, a
/// worker name that is not text. The farm refuses to start over such a store rather than book
/// a guess.
#[test]
fn a_fate_row_that_is_not_one_is_refused() {
    let row = fate_row();
    let fate = fate_found(row.clone()).expect("a fate").expect("one");
    assert_eq!(fate.booked_bytes, 16 << 30);
    assert_eq!(fate.memory_runs.len(), 1);
    let mut other = row.clone();
    other[0] = 3;
    let mut longer = row.clone();
    longer.push(0);
    // The last memory run's kind: 13 bytes from the end (the kind, then the three empty
    // budgets, two counts of 4 bytes and a number of 4).
    let mut kind = row.clone();
    let at = kind.len() - 12 - 1;
    kind[at] = 9;
    // Version 2 with one farm run: [version, booked 8, memory runs 4, farm runs 4, name 2+1,
    // kind, (reason), avoid 4, not started 4]
    let one_run = |tail: &[u8]| {
        [
            &[2][..],
            &1_u64.to_be_bytes(),
            &0_u32.to_be_bytes(),
            &1_u32.to_be_bytes(),
            &1_u16.to_be_bytes(),
            b"n",
            tail,
            &[0; 8],
        ]
        .concat()
    };
    let named = [
        &[2][..],
        &1_u64.to_be_bytes(),
        &[0; 8],
        &1_u32.to_be_bytes(),
        &1_u16.to_be_bytes(),
        &[0xff],
        &[0; 4],
    ]
    .concat();
    let rows = [
        (other, "not a version 1 or 2 fate"),
        (row[..row.len() - 1].to_vec(), "the row ends early"),
        (longer, "bytes after the last field"),
        (kind, "not a kind of memory kill"),
        (Vec::new(), "the row ends early"),
        (one_run(&[9]), "not a kind of farm fault"),
        (one_run(&[1, 9]), "not a reason the farm lost a run"),
        (named, "a worker name is not text"),
    ];
    for (bad, why) in rows {
        let error = fate_found(bad.clone()).expect_err("refused");
        assert!(error.contains(why), "{bad:?}: {error}");
    }
}

/// Catches: a row written before the budgets were (version 1: the booking and the memory runs
/// only) refused, or read with budgets it never held. It loads with every budget empty.
#[test]
fn a_version_1_fate_row_still_loads() {
    let mut row = vec![1];
    row.extend((16_u64 << 30).to_be_bytes());
    row.extend(1_u32.to_be_bytes());
    row.extend(6_u16.to_be_bytes());
    row.extend(b"node-a");
    row.extend((4_u64 << 30).to_be_bytes());
    row.push(0);

    let fate = fate_found(row).expect("a fate");

    assert_eq!(
        fate,
        Some(Fate {
            booked_bytes: 16 << 30,
            memory_runs: vec![MemoryRun {
                worker: node(),
                booked: 4 << 30,
                kill: MemoryKill::NodePressure,
            }],
            ..Fate::default()
        })
    );
}

/// Asserts that every kind of farm fault and of memory kill a version 2 fate row can hold is
/// read: the kinds the golden file's row does not hold on its own.
#[test]
fn a_fate_row_holds_every_kind_of_fault() {
    let row = [
        &[2][..],
        &1_u64.to_be_bytes(),
        &0_u32.to_be_bytes(),
        &3_u32.to_be_bytes(),
        &[0, 1, b'a', 0],
        &[0, 1, b'b', 1, 1],
        &[0, 1, b'c', 2],
        &1_u32.to_be_bytes(),
        &[0, 1, b'z'],
        &4_u32.to_be_bytes(),
    ]
    .concat();

    let fate = fate_found(row).expect("a fate").expect("one");

    let faults: Vec<_> = fate.farm_runs.iter().map(|run| run.fault).collect();
    assert_eq!(
        faults,
        [
            FarmFault::NodeMemoryPressure,
            FarmFault::Lost(LostWhy::Fenced),
            FarmFault::Infra
        ]
    );
    assert_eq!(fate.avoid.len(), 1);
    assert_eq!(fate.not_started, 4);
}

/// Asserts that a stored operation is taken up by the scheduler as it was held: its request,
/// the lease it holds (the lease and the worker), its losses and fate, and the waiter it is
/// held for.
#[test]
fn a_stored_operation_is_recovered_for_a_waiter() {
    let (store, codec, [rows]) = frontends([1]);
    rows.submit(OperationId(1), &request(1), &[], None)
        .expect("submit");
    lease(&store, &codec, 1, 0, "node-a", true);
    overwrite(
        &store,
        "loss/0000000000000001",
        2_u64.to_be_bytes().to_vec(),
    );
    let held = rows
        .stored_operation(OperationId(1))
        .expect("read")
        .expect("an operation");

    let recovered = held.recovered(WaiterId(77));

    assert_eq!(recovered.operation, OperationId(1));
    assert_eq!(recovered.request, request(1));
    assert_eq!(recovered.waiter, WaiterId(77));
    assert_eq!(recovered.losses, 2);
    assert_eq!(recovered.fate, None);
    let lease = recovered.lease.expect("a lease");
    assert_eq!((lease.lease, lease.worker), (LeaseId::new(5, 0), node()));

    rows.submit(OperationId(2), &request(2), &[], None)
        .expect("submit");
    let queued = rows.stored_operation(OperationId(2)).expect("read");
    let unleased = queued.expect("an operation").recovered(WaiterId(78));
    assert_eq!(unleased.lease, None);
}

/// Asserts that an operation with no `op/` row is not an operation of the store.
#[test]
fn an_operation_with_no_row_is_not_stored() {
    let (_, _, [rows]) = frontends([1]);
    let looked = rows.stored_operation(OperationId(3));
    assert!(matches!(looked, Ok(None)), "{looked:?}");
}

/// The lease of an operation another request joins is found through its `lease-by-op/` row
/// when the row stands.
#[test]
fn a_joined_operation_finds_its_lease_through_the_index() {
    let (store, codec, [first, second]) = frontends([1, 2]);
    first
        .submit(OperationId(1), &request(1), &[], None)
        .expect("submit");
    lease(&store, &codec, 1, 0, "node-a", true);
    // A scan of the lease rows would meet this one first, and not be the operation's.
    lease(&store, &codec, 2, 1, "node-b", false);

    let joined = second.submit(OperationId(100), &request(1), &[], None);

    let Ok(Submitted::Joined(op)) = joined else {
        panic!("the request did not join: {joined:?}");
    };
    assert_eq!(op.lease, Some(grant_of(1, 0)));
}

/// The lease of an operation a release that predates the index granted has no
/// `lease-by-op/` row, and one whose index row is missing, names a lease row that is gone or
/// names another operation's lease is found by reading the lease rows.
#[test]
fn a_lease_the_index_does_not_name_is_found_by_reading_the_lease_rows() {
    let (store, codec, [first, second]) = frontends([1, 2]);
    first
        .submit(OperationId(1), &request(1), &[], None)
        .expect("submit");
    lease(&store, &codec, 1, 0, "node-a", false);
    lease(&store, &codec, 2, 1, "node-b", false);
    let found = |second: &Rows| -> Option<LeaseGrant> {
        let looked = second.stored_operation(OperationId(1));
        looked.expect("read").expect("an operation").lease
    };

    // No index row.
    assert_eq!(found(&second), Some(grant_of(1, 0)));
    // An index row that names a lease row that is gone.
    let gone = [5_u64.to_be_bytes(), 9_u64.to_be_bytes()].concat();
    overwrite(&store, "lease-by-op/0000000000000001", gone);
    assert_eq!(found(&second), Some(grant_of(1, 0)));
    // An index row that names another operation's lease.
    let other = [5_u64.to_be_bytes(), 1_u64.to_be_bytes()].concat();
    overwrite(&store, "lease-by-op/0000000000000001", other);
    assert_eq!(found(&second), Some(grant_of(1, 0)));
}

/// Asserts that an index row that is not a lease id is refused, naming the row, and that an
/// operation in the queue holds no lease whatever the rows say.
#[test]
fn an_index_row_that_is_not_a_lease_id_is_refused_naming_it() {
    let (store, codec, [rows]) = frontends([1]);
    rows.submit(OperationId(1), &request(1), &[], None)
        .expect("submit");
    lease(&store, &codec, 1, 0, "node-a", true);
    overwrite(&store, "lease-by-op/0000000000000001", vec![1, 2, 3]);

    let refused = rows.stored_operation(OperationId(1));

    let Err(StateError::Corrupt { key, why }) = refused else {
        panic!("the row was not refused");
    };
    assert_eq!(key, "lease-by-op/0000000000000001");
    assert_eq!(why, "not a 16-byte lease id");

    // Queued again: the lease rows left over are not the operation's.
    overwrite(&store, "queue/0000000000000001", Vec::new());
    let queued = rows.stored_operation(OperationId(1)).expect("read");
    assert_eq!(queued.expect("an operation").lease, None);
}

/// Asserts that the operations a node holds leases for are the node's, each as the store holds
/// it, and that a lease row of another kind is refused naming the row.
#[test]
fn the_operations_a_node_holds_are_found_by_the_lease_rows() {
    let (store, codec, [rows]) = frontends([1]);
    for tag in [1, 2, 3] {
        rows.submit(OperationId(u64::from(tag)), &request(tag), &[], None)
            .expect("submit");
    }
    lease(&store, &codec, 1, 0, "node-a", true);
    lease(&store, &codec, 2, 1, "node-b", true);
    lease(&store, &codec, 3, 2, "node-a", true);

    let held = rows.held_by(&node()).expect("read");

    let ids: Vec<_> = held.iter().map(|op| op.operation.0).collect();
    assert_eq!(ids, [1, 3]);
    assert!(held.iter().all(|op| op.lease.is_some()));
    let submit = codec.submit(&request(1)).expect("encode");
    overwrite(&store, "lease/0000000000000005.0000000000000007", submit);
    let refused = rows.held_by(&node());
    let Err(StateError::Corrupt { key, why }) = refused else {
        panic!("the row was not refused");
    };
    assert_eq!(key, "lease/0000000000000005.0000000000000007");
    assert_eq!(why, "not a lease grant");
}

/// Asserts that the queue is read with the version of each row, that an operation taken out
/// of it is gone from the next read, and that one queued again has a newer version.
#[test]
fn the_queue_is_read_with_the_version_of_each_row() {
    let (store, codec, [rows]) = frontends([1]);
    for tag in [1, 2] {
        rows.submit(OperationId(u64::from(tag)), &request(tag), &[], None)
            .expect("submit");
    }
    let version_of = |operation: u64| {
        let row = store.get(&keys::queue(OperationId(operation)));
        row.expect("read").expect("a row").version
    };

    let queued = rows.queued().expect("read");

    assert_eq!(
        queued
            .iter()
            .map(|(id, version)| (id.0, *version))
            .collect::<Vec<_>>(),
        [(1, version_of(1)), (2, version_of(2))]
    );
    let first = rows.queue_version(OperationId(1)).expect("read");
    assert_eq!(first, Some(version_of(1)));
    lease(&store, &codec, 1, 0, "node-a", true);
    assert_eq!(rows.queue_version(OperationId(1)).expect("read"), None);
    assert_eq!(rows.queued().expect("read").len(), 1);
    // Given up and queued again.
    overwrite(&store, "queue/0000000000000001", Vec::new());
    let again = rows.queue_version(OperationId(1)).expect("read");
    assert_ne!(again, first, "a new queue row has the old version");
}

/// Asserts that a key under `queue/` that is not an operation id is refused, naming the row,
/// so that a poll which cannot read the queue takes nothing in on a guess.
#[test]
fn a_queue_key_that_is_not_an_operation_id_is_refused() {
    let (store, _, [rows]) = frontends([1]);
    rows.submit(OperationId(1), &request(1), &[], None)
        .expect("submit");
    overwrite(&store, "queue/zz", Vec::new());

    let refused = rows.queued();

    let Err(StateError::Corrupt { key, .. }) = refused else {
        panic!("the row was not refused");
    };
    assert_eq!(key, "queue/zz");
    remove(&store, "queue/zz");
    assert_eq!(rows.queued().expect("read").len(), 1);
}

/// Asserts that a node is known once a frontend has owned it: its `owner/` row is there.
#[test]
fn a_node_is_known_once_a_frontend_has_owned_it() {
    let (_, _, [rows]) = frontends([1]);
    assert!(!rows.node_known(&node()).expect("read"));

    rows.arrive(&node(), FarmTime::from_millis(0))
        .expect("a Hello");

    assert!(rows.node_known(&node()).expect("read"));
    assert!(!rows.node_known(&WorkerId::new("node-b")).expect("read"));
}

/// Asserts that the store's clock is read in milliseconds, whole ones, as a drain's deadline
/// is written.
#[test]
fn the_store_clock_is_read_in_milliseconds() {
    let (store, _, [rows]) = frontends([1]);
    let before = rows.store_ms();

    store.advance(Duration::from_millis(3_250));

    assert_eq!(rows.store_ms() - before, 3_250);
}

/// Asserts that the stored epoch is the epoch row's number, as an operator's bump raises it
/// after a handle opened, and 0 for a store with no epoch row.
#[test]
fn the_stored_epoch_follows_the_epoch_row() {
    let store = Racing::new();
    let (rows, epoch) = open(store.clone(), EpochPolicy::Keep, 42).expect("open");
    assert_eq!(epoch, 42);
    assert_eq!(rows.stored_epoch().expect("read"), 42);

    rows.bump_epoch().expect("a bump");
    assert_eq!(rows.stored_epoch().expect("read"), 43);

    remove(&store, "meta/epoch");
    assert_eq!(rows.stored_epoch().expect("read"), 0);
}

/// What a grant of these tests was charged: every figure is set.
const CHARGED: LeaseCost = LeaseCost {
    cpu_millis: 1_500,
    memory_bytes: 3 << 30,
    limit_bytes: 4 << 30,
    cold: true,
    retry: true,
    key_hash: 0x00c0_ffee,
};

/// Asserts that a lease granted with a cost is recovered with that cost, whether the start
/// loads it or a look at the operation (or at the node's leases) finds it: the lease row
/// keeps the whole grant, and the scheduler is told `Some(cost)`.
#[test]
fn a_granted_lease_is_recovered_with_the_cost_it_was_granted() {
    let (_, _, [rows]) = frontends([1]);
    rows.submit(OperationId(4), &request(1), &[], None)
        .expect("submit");
    let grant = LeaseGrant {
        cost: CHARGED,
        ..grant_of(4, 0)
    };
    rows.granted(&grant).expect("grant");

    let mut stored = rows.load().expect("load");
    let loaded = stored.operations.remove(0);
    let looked = rows.stored_operation(OperationId(4)).expect("read");
    let held = rows.held_by(&node()).expect("read").remove(0);

    for op in [loaded, looked.expect("an operation"), held] {
        assert_eq!(op.lease.as_ref(), Some(&grant));
        assert!(op.cost_kept);
        let recovered = op.recovered(WaiterId(1)).lease.expect("a lease");
        assert_eq!(recovered.lease, grant.lease);
        assert_eq!(recovered.cost, Some(CHARGED));
    }
}

/// Asserts that a lease row written before costs were kept is recovered with no cost (`None`,
/// which the scheduler charges as the request), not with the placeholder the codec read, and
/// that the lease itself is still recovered.
#[test]
fn a_lease_row_without_a_cost_is_recovered_with_none() {
    let (store, codec, [rows]) = frontends([1]);
    rows.submit(OperationId(4), &request(1), &[], None)
        .expect("submit");
    let grant = LeaseGrant {
        cost: CHARGED,
        ..grant_of(4, 0)
    };
    let row = codec.legacy_lease(&grant);
    let index = [5_u64.to_be_bytes(), 0_u64.to_be_bytes()].concat();
    let put = Txn::new()
        .delete(keys::queue(grant.operation), Cond::Any)
        .put(keys::lease(grant.lease), row, Cond::Any)
        .put(keys::lease_by_op(grant.operation), index, Cond::Any);
    store.txn(put).expect("store").expect("the old grant");

    let mut stored = rows.load().expect("load");
    let loaded = stored.operations.remove(0);
    let looked = rows.stored_operation(OperationId(4)).expect("read");
    let held = rows.held_by(&node()).expect("read").remove(0);

    for op in [loaded, looked.expect("an operation"), held] {
        assert!(!op.cost_kept);
        let recovered = op.recovered(WaiterId(1)).lease.expect("a lease");
        assert_eq!((recovered.lease, recovered.worker), (grant.lease, node()));
        assert_eq!(recovered.cost, None);
    }
}
