//! The job store suite on the SQLite store, one test per check. Every store gets a directory
//! of its own under the test target directory, removed when the backend is dropped.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use js_state::conformance::Backend;
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Clock, Conflict, ManualClock, Store, Txn};
use js_testkit::dir::TestDir;

/// The suite's factory for the SQLite store: a restart closes the store (its files are then
/// as a crash leaves them, since every acknowledged group was synced) and opens the same
/// file again.
struct SqliteBackend {
    opened: AtomicUsize,
    /// The directories of the stores `open` returned, kept until the backend is dropped.
    dirs: Mutex<Vec<TestDir>>,
}

fn sqlite() -> SqliteBackend {
    SqliteBackend {
        opened: AtomicUsize::new(0),
        dirs: Mutex::new(Vec::new()),
    }
}

impl Backend for SqliteBackend {
    type Store = SqliteStore;

    fn open(&self, clock: Arc<ManualClock>) -> SqliteStore {
        let n = self.opened.fetch_add(1, Ordering::SeqCst);
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("js-jobs-{n}"));
        let clock: Arc<dyn Clock> = clock;
        let store =
            SqliteStore::open(&dir.join("state.db"), clock, Options::default()).expect("opens");
        self.dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(dir);
        store
    }

    fn restart(&self, store: SqliteStore) -> SqliteStore {
        let path = store.path().to_owned();
        let clock = store.clock();
        drop(store);
        SqliteStore::open(&path, clock, Options::default()).expect("opens again")
    }

    fn commit_out_of_order(
        &self,
        store: &SqliteStore,
        first: Txn,
        second: Txn,
        between: &mut dyn FnMut(),
    ) -> Result<(), Conflict> {
        // One writer commits in the order it is given; the suite does not use this.
        store.txn(first).expect("the store is reachable")?;
        between();
        store.txn(second).expect("the store is reachable")?;
        Ok(())
    }
}

js_jobs::job_store_tests!(sqlite);
