//! Rows a later binary wrote, read back as a start reads them: a row that holds a command
//! this binary does not know is skipped, and the rows it can read load.

mod common;

use std::io;
use std::sync::{Arc, Mutex};

use js_sched::Request;
use js_state::{Cond, Store, Txn};
use js_store::test_support::TestCodec;
use js_store::{RowCodec, Rows, StateError, Submitted, keys};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Resources, WorkKey, WorkerId,
};

use common::{Racing, frontend_with};

/// Log lines written by the test's subscriber on this thread.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Lines {
    /// The log of this thread, until the guard drops. Other tests reach the same callsites
    /// with no subscriber and can leave their interest cached as never, so the cache is
    /// rebuilt once ours is installed.
    fn subscribe(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        guard
    }

    fn text(&self) -> String {
        let bytes = self.0.lock().expect("lock").clone();
        String::from_utf8(bytes).expect("UTF-8 logs")
    }
}

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

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

/// Asserts that a request for work another submit queued joins that operation, and carries
/// its lease, when the store also holds a `lease/` row that holds a command this binary does
/// not know: the row is passed over, not refused, and not logged.
#[test]
fn a_request_joins_an_operation_past_an_unknown_lease_row() {
    let (store, codec, rows) = opened();
    let queued = rows.submit(OperationId(1), &request(1), &[], None);
    assert!(
        matches!(queued, Ok(Submitted::Queued(OperationId(1)))),
        "{queued:?}"
    );
    // A granted operation has left the queue: the lease is looked up only then.
    let granted = Txn::new().delete(keys::queue(OperationId(1)), Cond::Any);
    store
        .txn(granted)
        .expect("the store answers")
        .expect("the queue row goes");
    put(&store, LEASE_KNOWN, codec.lease(&grant()).expect("encode"));
    put(&store, LEASE_UNKNOWN, unknown());
    let lines = Lines::default();
    let _guard = lines.subscribe();

    let joined = rows.submit(OperationId(2), &request(1), &[], None);

    let Ok(Submitted::Joined(op)) = joined else {
        panic!("the request did not join");
    };
    assert_eq!(op.operation, OperationId(1));
    assert_eq!(op.lease, Some(grant()));
    let text = lines.text();
    assert!(!text.contains(LEASE_UNKNOWN), "{text}");
}

/// Asserts that a start logs the key of a row it skips, and that the search for the
/// operations a node holds leases for, which a worker's arrival makes, passes over the same
/// row without logging it.
#[test]
fn only_a_start_logs_the_rows_it_skips() {
    let (store, _, rows) = opened();
    put(&store, LEASE_UNKNOWN, unknown());
    let lines = Lines::default();
    let _guard = lines.subscribe();

    rows.held_by(&grant().worker)
        .expect("the search reads the rows");
    let text = lines.text();
    assert!(!text.contains(LEASE_UNKNOWN), "{text}");
    rows.load().expect("the rows load");

    let text = lines.text();
    assert!(text.contains(LEASE_UNKNOWN), "{text}");
}
