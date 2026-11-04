//! The job store suite on Postgres, one test per check. Every store is a schema of its own in
//! the database `KBF_TEST_POSTGRES_URL` names (the test harness's setting; the tests fail,
//! and do not skip, without it). The `state-postgres` CI job runs this file with the
//! `postgres` feature.
#![cfg(feature = "postgres")]

use js_state::postgres::{Flaw, PostgresBackend};

fn postgres() -> PostgresBackend {
    PostgresBackend::new(&js_testkit::database::server_url(), Flaw::None)
}

js_jobs::job_store_tests!(postgres);
