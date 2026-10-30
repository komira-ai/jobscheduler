//! The line the orphan sweep logs when it requeues a lease. The test is the only one of its
//! binary, so no parallel test reaches the callsite while its subscriber is installed.

mod common;

use std::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_sched::Request;
use js_store::test_support::TestCodec;
use js_store::{Orphans, Rows};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Resources, WorkKey, WorkerId,
};

use common::{Racing, frontend_with};

/// Log lines written by the test's subscriber on this thread.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Asserts that the requeue of an orphaned lease is logged with the operation, the lease and
/// the node.
#[test]
fn a_requeued_orphan_is_logged_with_its_operation_lease_and_node() {
    let lines = Lines::default();
    let writer = lines.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    let wait = Duration::from_secs(30);
    let store = Racing::new();
    let rows: Rows = frontend_with(&store, 1, &Arc::new(TestCodec::new()));
    let request = Request {
        key: WorkKey::new("main", "ab/1"),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
        whole_worker: false,
    };
    rows.submit(OperationId(4), &request, &[], None)
        .expect("submit");
    let grant = LeaseGrant {
        lease: LeaseId::new(5, 0),
        operation: OperationId(4),
        worker: WorkerId::new("node-a"),
        cost: LeaseCost::default(),
    };
    rows.granted(&grant).expect("grant");
    let (nobody, mut seen) = (BTreeSet::new(), Orphans::default());
    rows.sweep(wait, &nobody, &mut seen).expect("a look");
    store.advance(wait);

    rows.sweep(wait, &nobody, &mut seen).expect("a requeue");

    let text = String::from_utf8(lines.0.lock().expect("lock").clone()).expect("UTF-8 logs");
    assert!(text.contains("the operation is queued again"), "{text}");
    for field in ["operation=", "lease=", "node=node-a"] {
        assert!(text.contains(field), "{field}: {text}");
    }
}
