//! The job service over a state store that says this process is no longer its writer
//! (`StoreError::Fenced`): every call that meets it answers `UNAVAILABLE` `state-fenced`, not
//! `INTERNAL`, and trips the server's fence so the server stops.

use js_proto::v1::{GetJobRequest, GetJobResponse, ListJobsRequest, ListJobsResponse};
use js_state::StoreError;
use tonic::Code;

use crate::job_api::writes_rig::{Fixed, Writes, over_fake, wire};

/// What a call answers when the store has a newer writer.
const FENCED: (Code, &str) = (Code::Unavailable, "state-fenced");

/// A service over a store that answers its next read or commit with `Fenced`.
async fn fenced_service() -> (
    std::sync::Arc<js_jobs::interfere::Interfere<js_state::fake::Fake>>,
    Writes,
) {
    let (store, writes) = over_fake(Fixed::new(true)).await;
    store.fail_with(StoreError::Fenced);
    (store, writes)
}

fn said(status: &tonic::Status) -> (Code, &str) {
    (status.code(), status.message())
}

/// Asserts that a `SubmitJob` whose read of the id meets `Fenced`, and one whose commit does,
/// each answer `UNAVAILABLE` `state-fenced`, write no row, and trip the fence. It catches a
/// fenced store answered `INTERNAL` `state-write-failed`, which a client retries as a fault of
/// the moment, and a server that goes on serving after it.
#[tokio::test]
async fn a_submit_that_meets_a_fenced_store_is_unavailable_and_trips_the_fence() {
    let (store, writes) = fenced_service().await;
    assert!(!writes.fence.is_tripped());
    store.fail_read_after(0);
    let status = writes.submit_grpc(&wire("job-f")).await.unwrap_err();
    assert_eq!(said(&status), FENCED);
    assert!(writes.fence.is_tripped());

    let (store, writes) = fenced_service().await;
    store.fail_next();
    let status = writes.submit_grpc(&wire("")).await.unwrap_err();
    assert_eq!(said(&status), FENCED);
    assert!(writes.fence.is_tripped());
    assert_eq!(crate::job_api::writes_rig::rows(&writes.jobs), 0);
}

/// Asserts that a `CancelJob` whose commit meets `Fenced` answers `UNAVAILABLE`
/// `state-fenced`, leaves the job as it was, and trips the fence. It catches a fenced store
/// answered as a failed write.
#[tokio::test]
async fn a_cancel_that_meets_a_fenced_store_is_unavailable_and_trips_the_fence() {
    let (store, writes) = over_fake(Fixed::new(true)).await;
    writes.submit_grpc(&wire("job-q")).await.unwrap();
    store.fail_with(StoreError::Fenced);
    store.fail_next();
    let status = writes.cancel_grpc("job-q").await.unwrap_err();
    assert_eq!(said(&status), FENCED);
    assert!(writes.fence.is_tripped());
    let job = writes
        .jobs
        .get_job(&crate::job_api::writes_rig::id("job-q"))
        .unwrap()
        .unwrap();
    assert!(!job.cancel_requested);
}

/// Asserts that a `GetJob` and a `ListJobs` whose read meets `Fenced` each answer
/// `UNAVAILABLE` `state-fenced` and trip the fence. It catches a fenced read answered
/// `INTERNAL` `internal`.
#[tokio::test]
async fn a_read_that_meets_a_fenced_store_is_unavailable_and_trips_the_fence() {
    let (store, writes) = fenced_service().await;
    store.fail_read_after(0);
    let request = GetJobRequest {
        job_id: "job-r".to_owned(),
    };
    let status = writes
        .grpc::<_, GetJobResponse>("GetJob", request)
        .await
        .unwrap_err();
    assert_eq!(said(&status), FENCED);
    assert!(writes.fence.is_tripped());

    let (store, writes) = fenced_service().await;
    store.fail_read_after(0);
    let status = writes
        .grpc::<_, ListJobsResponse>("ListJobs", ListJobsRequest::default())
        .await
        .unwrap_err();
    assert_eq!(said(&status), FENCED);
    assert!(writes.fence.is_tripped());
}
