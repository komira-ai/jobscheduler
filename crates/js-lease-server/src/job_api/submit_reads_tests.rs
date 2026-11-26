//! What `SubmitJob` reads of the store outside its one transaction: the job's id once (step 2),
//! and no clock, since the job's submit time is the store's clock as the transaction that
//! records it reads it. The reads are counted on a recording store over the in-memory fake,
//! with an admission source that reads nothing, so the counts are the call's own.

use std::sync::Arc;

use js_jobs::JobState;
use js_proto::v1::{self, SubmitJobRequest};
use js_state::fake::{Fake, Flaw};
use js_state::recording::{Read, RecordingStore};
use js_state::{ManualClock, Store};
use js_types::Timestamp;
use tonic::{Code, Request, Status};

use super::{Admission, Ctx, submit_job};
use crate::clock::SharedClock;
use crate::config::Role;
use crate::job_api::JobApi;
use crate::job_api::read_tests::tables;
use crate::job_api::roles::Caller;
use crate::job_api::writes_rig::{CEILING, Fixed, id, wire};

/// The job service over a recording store, admitting through `admission`, the record, cleared
/// once the tables are open, and the store's clock.
fn recorded(admission: Arc<dyn Admission>) -> (JobApi, Arc<RecordingStore>, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::default());
    let fake: Arc<dyn Store> = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let recording = Arc::new(RecordingStore::new(fake));
    let jobs = tables(recording.clone());
    let shared = SharedClock::new(js_types::ManualClock::new(Timestamp::from_millis(0)));
    let api = JobApi::new(jobs, shared).with_submit(Ctx::served(CEILING, admission));
    // What opening the tables read and wrote (the schema row, the backfill) is not the call's.
    recording.clear();
    (api, recording, clock)
}

/// The answer to submitting `spec` to `api` as an operator.
async fn submit(api: &JobApi, spec: v1::JobSpec) -> Result<v1::Job, Status> {
    let caller = Caller {
        name: js_jobs::ClientName::default_client(),
        role: Role::Operator,
    };
    let request = Request::new(SubmitJobRequest { spec: Some(spec) });
    Ok(submit_job(api, &caller, request)
        .await?
        .into_inner()
        .job
        .unwrap())
}

/// Asserts that a submit of a new id reads the store once outside its transaction, a get of
/// the job's row, and reads no clock, and that the job it records is submitted at the store's
/// clock as the transaction read it. Before, the call read the job's row twice (the handler,
/// then the insert) and the clock once before the transaction. It catches a second read of the
/// id and a clock read outside the transaction.
#[tokio::test]
async fn a_submit_of_a_new_id_reads_its_row_once_and_no_clock() {
    let (api, recording, clock) = recorded(Fixed::new(true));
    let before = js_state::Clock::now(&*clock);
    let job = submit(&api, wire("job-n")).await.unwrap();
    assert_eq!(job.state(), v1::JobState::Queued);
    assert_eq!(recording.reads(), [Read::Get("job/job-n".to_owned())]);
    let stored = api.jobs.get_job(&id("job-n")).unwrap().unwrap();
    assert_eq!(stored.state, JobState::Queued);
    // The fake moves its clock on at each commit's sync, after the transaction read it.
    assert_eq!(stored.submitted_at, before);
}

/// Asserts that a retry of a job already recorded, whose admission now refuses (nothing could
/// serve it any more), is still answered with the job: admission is asked only for a job that
/// would be created, and the answer comes from the one read of the job's row, with nothing
/// written. It catches a retry refused `no-capable-host`, and a retry that reads the row again.
#[tokio::test]
async fn a_retry_of_a_held_job_whose_admission_now_refuses_is_answered_with_the_job() {
    let admits = Fixed::new(true);
    let (api, recording, _) = recorded(admits);
    submit(&api, wire("job-h")).await.unwrap();
    let refuses = Fixed::new(false);
    let api = api.with_submit(Ctx::served(CEILING, refuses.clone()));
    recording.clear();
    let again = submit(&api, wire("job-h")).await.unwrap();
    assert_eq!(
        (again.id.as_str(), again.state()),
        ("job-h", v1::JobState::Queued)
    );
    assert_eq!(refuses.asked(), 0);
    assert_eq!(recording.reads(), [Read::Get("job/job-h".to_owned())]);
    assert_eq!(recording.writes(), Vec::new());
}

/// Asserts that a new job nothing could serve is refused `no-capable-host` and records nothing:
/// no row, no write, and no read past the one get of its id. It catches a refusal that writes,
/// and an admission skipped for a new id.
#[tokio::test]
async fn a_new_job_admission_refuses_is_refused_and_records_nothing() {
    let refuses = Fixed::new(false);
    let (api, recording, _) = recorded(refuses.clone());
    let refused = submit(&api, wire("job-r")).await.unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert_eq!(refuses.asked(), 1);
    assert_eq!(recording.writes(), Vec::new());
    assert_eq!(recording.reads(), [Read::Get("job/job-r".to_owned())]);
    assert!(api.jobs.get_job(&id("job-r")).unwrap().is_none());
}
