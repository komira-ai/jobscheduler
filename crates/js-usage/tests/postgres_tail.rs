//! The tail counters on Postgres: every scenario of `tail_cases`, each over a schema of its
//! own in the database `KBF_TEST_POSTGRES_URL` names (the test harness's setting; the tests
//! fail, and do not skip, without it). The `state-postgres` CI job runs this file with the
//! `postgres` feature.
#![cfg(feature = "postgres")]

mod tail_cases;

use std::sync::Arc;

use js_state::ManualClock;
use js_state::conformance::Backend;
use js_state::postgres::{Flaw, PostgresBackend};
use tail_cases::Hooked;

/// A fresh, empty Postgres store.
fn postgres() -> Arc<Hooked> {
    let backend = PostgresBackend::new(&js_testkit::database::server_url(), Flaw::None);
    let store = backend.open(Arc::new(ManualClock::default()));
    // The backend drops the store's schema when it is dropped, so it lives as long.
    Hooked::over(Arc::new(store), Some(Box::new(backend)))
}

crate::tail_store_tests!(postgres);
