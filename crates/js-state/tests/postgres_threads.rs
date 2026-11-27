//! Where a real Postgres store's calls run, as the store counts them
//! ([`PostgresStore::threads_started`]): the client blocks on a runtime of its own, so a call
//! made on a thread a tokio runtime runs on is made away from it; a call made from the pool for
//! blocking calls of a multi-thread runtime, as the lease server makes every one, runs on that
//! thread and starts none.
//!
//! The tests need a server (`KBF_TEST_POSTGRES_URL` names it) and fail without one. No
//! assertion depends on elapsed time.
#![cfg(feature = "postgres")]

use std::sync::Arc;

use js_state::postgres::{Flaw, PostgresBackend, PostgresStore};
use js_state::{Cond, Store, Txn};

/// A store on a fresh schema of the test server.
fn store() -> PostgresStore {
    let backend = PostgresBackend::new(&js_testkit::database::server_url(), Flaw::None);
    PostgresStore::open(&backend.fresh_config()).expect("the test database opens")
}

/// A write and a read of `key` in `store`: a transaction's statements and a read's.
fn write_and_read(store: &PostgresStore, key: &str) {
    let txn = Txn::new().put(key, b"v".to_vec(), Cond::Any);
    store
        .txn(txn)
        .expect("the store is reachable")
        .expect("the write commits");
    let read = store.get(key).expect("the store is reachable");
    assert_eq!(read.map(|row| row.value), Some(b"v".to_vec()));
}

// Catches a store that starts a thread for each call made from the pool for blocking calls of a
// multi-thread runtime: there the client's own runtime may run, so the calls, a transaction's and
// a read's, send their statements and start no thread.
#[test]
fn calls_from_the_pool_for_blocking_calls_start_no_thread() {
    let store = Arc::new(store());
    let opened = store.threads_started();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("a runtime");
    let (calling, sent) = (Arc::clone(&store), store.statements());
    runtime
        .block_on(async {
            tokio::task::spawn_blocking(move || write_and_read(&calling, "pooled")).await
        })
        .expect("the calls end");
    assert!(store.statements() > sent);
    assert_eq!(store.threads_started() - opened, 0);
}

// Catches a store whose count misses the threads it does start: a call made on the thread of a
// current-thread runtime, where the client cannot block on its own, runs on a thread of its own,
// and each such call is counted. Outside any runtime a call starts none.
#[test]
fn calls_on_a_current_thread_runtime_start_a_thread_each_and_are_counted() {
    let store = store();
    let opened = store.threads_started();
    write_and_read(&store, "outside");
    assert_eq!(store.threads_started(), opened);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    let sent = store.statements();
    runtime.block_on(async { write_and_read(&store, "inside") });
    let calls = store.statements() - sent;
    assert!(calls > 0);
    assert!(store.threads_started() - opened >= calls);
}
