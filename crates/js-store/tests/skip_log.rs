//! The log line a start writes for a row it skips. This test is the only one in its test
//! binary: tracing caches each callsite's interest for the whole process, and a test on
//! another thread that reaches the same callsite with no subscriber can leave it cached as
//! never while this test's subscriber is installed. With no other test in the process,
//! nothing races the cache.

mod common;

use std::io;
use std::sync::{Arc, Mutex};

use js_state::{Cond, Store, Txn};
use js_store::Rows;
use js_store::test_support::TestCodec;
use js_types::sched::WorkerId;

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

const LEASE_UNKNOWN: &str = "lease/0000000000000005.0000000000000001";

/// Asserts that a start logs the key of a row it skips, and that the search for the
/// operations a node holds leases for, which a worker's arrival makes, passes over the same
/// row without logging it.
#[test]
fn only_a_start_logs_the_rows_it_skips() {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let rows: Rows = frontend_with(&store, 100, &codec);
    // A value the test codec reads as one a later binary wrote.
    let put = Txn::new().put(LEASE_UNKNOWN, b"later command".to_vec(), Cond::Any);
    store.txn(put).expect("store").expect("put");
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
