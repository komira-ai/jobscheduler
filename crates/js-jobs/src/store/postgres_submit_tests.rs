//! Recording a job on a real Postgres server (feature `postgres`; `KBF_TEST_POSTGRES_URL` names
//! the server, and the test fails without one): the statements a submit's insert sends, and the
//! answer when the id was taken after the caller's read.

use std::sync::Arc;

use js_state::ManualClock;
use js_state::conformance::Backend;
use js_state::postgres::{Flaw, PostgresBackend};

use crate::suite::{full_spec, whole_host_spec};
use crate::{JobId, JobStore, JobStoreError, NewJob, RandomIds};

fn new_job(id: &str, spec: crate::JobSpec) -> NewJob {
    NewJob {
        id: Some(JobId::parse(id).expect("an id")),
        spec,
        priority: 0,
        client: None,
    }
}

/// Asserts that recording a job of a new id sends eight statements to Postgres: the get of its
/// id, and its transaction's seven (`BEGIN`, the clock the job is stamped with, the reservation
/// of its keys, `LOCK TABLE head`, the head's move, the writes, `COMMIT`). Before, it sent ten:
/// the get, a clock read of its own, and eight in the transaction, one of them a load of keys
/// the reservation had found absent. It also asserts that the conflict path still
/// answers `job-id-conflict`: a different job under an id taken after the caller's read finds
/// the row in the transaction, which writes nothing, and the stored job is read and judged.
#[test]
fn a_new_job_is_eight_statements_and_an_id_taken_since_the_read_is_still_a_conflict() {
    let backend = PostgresBackend::new(&js_testkit::database::server_url(), Flaw::None);
    let store = Arc::new(backend.open(Arc::new(ManualClock::default())));
    let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("the job store opens");
    let before = store.statements();
    let made = jobs
        .insert_queued(new_job("pg-a", full_spec()))
        .expect("the job is recorded");
    assert!(made.created);
    assert_eq!(store.statements() - before, 8);
    let stored = jobs.get_job(&made.record.id).expect("reads");
    assert_eq!(stored.as_ref(), Some(&made.record));

    let other = jobs.insert_queued_after_get(new_job("pg-a", whole_host_spec()), None);
    assert_eq!(
        other,
        Err(JobStoreError::JobIdConflict {
            id: made.record.id.clone()
        })
    );
    assert_eq!(jobs.get_job(&made.record.id).expect("reads"), stored);
}
