//! What the log says about rows a later binary wrote. These tests are the only ones in
//! their test binary, and each installs its subscriber before it reaches the skip-log
//! callsite: tracing caches each callsite's interest for the whole process, and a test that
//! reached the same callsite with no subscriber, on another thread, could leave it cached
//! as never while a subscriber is installed. With no such test in the process, nothing races
//! the cache. Each test that asserts a line is absent also asserts, in the same capture,
//! that a line the start writes is present, so it cannot pass with nothing captured.

mod common;

use std::io;
use std::sync::{Arc, Mutex};

use js_sched::Request;
use js_state::{Cond, Store, Txn};
use js_store::test_support::TestCodec;
use js_store::{RowCodec, Rows, Submitted, keys};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Resources, WorkKey, WorkerId,
};

use common::{Racing, frontend_with};

/// Log lines written by the test's subscriber on this thread.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Lines {
    /// The log of this thread, until the guard drops. The interest cache is rebuilt once
    /// the subscriber is installed, in case a callsite was registered before it.
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

const LEASE_KNOWN: &str = "lease/0000000000000005.0000000000000000";
const LEASE_UNKNOWN: &str = "lease/0000000000000005.0000000000000001";

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

/// Asserts that a request for work another submit queued joins that operation, and carries
/// its lease, when the store also holds a `lease/` row that holds a command this binary does
/// not know: the row is passed over, not refused, and not logged. A start afterwards logs
/// it, in the same capture.
#[test]
fn a_request_joins_an_operation_past_an_unknown_lease_row() {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let rows: Rows = frontend_with(&store, 100, &codec);
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
    rows.load().expect("the rows load");
    let text = lines.text();
    assert!(text.contains(LEASE_UNKNOWN), "{text}");
}

/// Asserts that a start logs the key of a row it skips, and that the search for the
/// operations a node holds leases for, which a worker's arrival makes, passes over the same
/// row without logging it.
#[test]
fn only_a_start_logs_the_rows_it_skips() {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let rows: Rows = frontend_with(&store, 100, &codec);
    put(&store, LEASE_UNKNOWN, unknown());
    let lines = Lines::default();
    let _guard = lines.subscribe();

    rows.held_by(&WorkerId::new("node-a"))
        .expect("the search reads the rows");
    let text = lines.text();
    assert!(!text.contains(LEASE_UNKNOWN), "{text}");
    rows.load().expect("the rows load");

    let text = lines.text();
    assert!(text.contains(LEASE_UNKNOWN), "{text}");
}
