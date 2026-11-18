//! Every call of the job service that reads or writes the job tables does so on the
//! runtime's pool for blocking calls, never on the worker that runs the call: the store's
//! calls block (on a slow database, up to its statement timeout), and a worker they hold
//! serves no connection meanwhile.
//!
//! The tests run on a one-thread runtime, so the worker is the test's own thread; the store
//! under the tables records the thread of each of its calls, and every recorded thread must
//! be another one. A call made on the worker is seen at once, with no wait on time.

use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, ThreadId};

use js_jobs::{ClientName, JobStore};
use js_proto::v1::{
    CancelJobRequest, GetJobRequest, GetLogsRequest, GetResultRequest, ListJobsRequest,
    SubmitJobRequest,
};
use js_state::fake::{Fake, Flaw};
use js_state::{
    Changes, Claim, Commit, CommitStats, Conflict, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, TxnRead, Versioned,
};
use js_types::{ManualClock, Timestamp};
use tonic::Request;

use super::submit::Ctx;
use super::writes_rig::{CEILING, Fixed, NEVER, wire};
use super::{JobApi, cancel, read, submit};
use crate::agent_api::job_rig::clocked_rig;
use crate::clock::SharedClock;
use crate::config::Role;
use crate::job_api::roles::Caller;
use crate::job_store::{ServerIds, ServerJobs};

/// The fake store, recording the thread each of its calls ran on.
struct Probe {
    inner: Fake,
    threads: Mutex<Vec<ThreadId>>,
}

impl Probe {
    fn note(&self) {
        let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        threads.push(thread::current().id());
    }

    /// The threads recorded since the last take.
    fn take(&self) -> Vec<ThreadId> {
        let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut threads)
    }
}

impl Store for Probe {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.note();
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.note();
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.note();
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.note();
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.note();
        self.inner.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.note();
        self.inner.trim_feed(before)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.note();
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }

    fn txn(&self, txn: Txn) -> Result<Result<Commit, Conflict>, StoreError> {
        self.note();
        self.inner.txn(txn)
    }

    fn head(&self) -> Result<Option<Revision>, StoreError> {
        self.note();
        self.inner.head()
    }

    fn claim_decide(
        &self,
        claim: &[String],
        decide: &mut dyn FnMut(&mut dyn TxnRead) -> Option<Txn>,
    ) -> Result<Claim, StoreError> {
        self.note();
        self.inner.claim_decide(claim, decide)
    }
}

/// Job tables over a fresh [`Probe`], and the probe.
fn probed() -> (Arc<Probe>, Arc<ServerJobs>) {
    let probe = Arc::new(Probe {
        inner: Fake::with_flaw(Arc::default(), Flaw::None),
        threads: Mutex::default(),
    });
    let store: Arc<dyn Store> = probe.clone();
    let jobs = JobStore::new(Arc::new(store), ServerIds::random()).unwrap();
    probe.take();
    (probe, Arc::new(jobs))
}

/// The operator the calls are made as.
fn operator() -> Caller {
    Caller {
        name: ClientName::default_client(),
        role: Role::Operator,
    }
}

/// Asserts that `probe` recorded at least one call since its last take, and none on this
/// thread (the runtime's only worker); `call` names the call in the message.
fn off_the_worker(probe: &Probe, call: &str) {
    let here = thread::current().id();
    let threads = probe.take();
    assert!(!threads.is_empty(), "{call} read no job table");
    assert!(
        threads.iter().all(|thread| *thread != here),
        "{call} called the store on the runtime's worker"
    );
}

/// Asserts that `SubmitJob` makes every call of the job tables off the worker, on both of its
/// paths: a job with an id into a placing service (the id's lookup, the read of the queue, the
/// commit, and the read of the job after the pass), and a job without an id into one that does
/// not place (the commit alone).
#[tokio::test]
async fn submit_calls_the_store_off_the_worker() {
    let rig = clocked_rig("submit_off_worker", true, NEVER);
    let (probe, jobs) = probed();
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(0)));
    let placing = Ctx::served(CEILING, Fixed::new(true)).placing(Arc::clone(&rig.agents));
    let api = JobApi::new(Arc::clone(&jobs), clock.clone()).with_submit(placing);
    let request = Request::new(SubmitJobRequest {
        spec: Some(wire("job-a")),
    });
    submit::submit_job(&api, &operator(), request)
        .await
        .unwrap();
    off_the_worker(&probe, "SubmitJob with an id, placing");

    let api = JobApi::new(jobs, clock).with_submit(Ctx::served(CEILING, Fixed::new(true)));
    let request = Request::new(SubmitJobRequest {
        spec: Some(wire("")),
    });
    submit::submit_job(&api, &operator(), request)
        .await
        .unwrap();
    off_the_worker(&probe, "SubmitJob without an id");
}

/// Asserts that the read calls (`GetJob`, `GetResult`, `GetLogs` without a stream, `ListJobs`)
/// and `CancelJob` (its read and its commit) call the job tables off the worker.
#[tokio::test]
async fn the_reads_and_the_cancel_call_the_store_off_the_worker() {
    let (probe, jobs) = probed();
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(0)));
    let api = JobApi::new(jobs, clock).with_submit(Ctx::served(CEILING, Fixed::new(true)));
    let caller = operator();
    let request = Request::new(SubmitJobRequest {
        spec: Some(wire("job-r")),
    });
    submit::submit_job(&api, &caller, request).await.unwrap();
    probe.take();
    let id = "job-r".to_owned();

    let request = Request::new(GetJobRequest { job_id: id.clone() });
    read::get_job(&api, &caller, request).await.unwrap();
    off_the_worker(&probe, "GetJob");

    let request = Request::new(GetResultRequest { job_id: id.clone() });
    let not_finished = read::get_result(&api, &caller, request).await.unwrap_err();
    assert_eq!(not_finished.message(), "not-finished");
    off_the_worker(&probe, "GetResult");

    let request = Request::new(GetLogsRequest {
        job_id: id.clone(),
        ..GetLogsRequest::default()
    });
    read::get_logs(&api, &caller, request).await.unwrap();
    off_the_worker(&probe, "GetLogs");

    let request = Request::new(ListJobsRequest::default());
    let listed = read::list_jobs(&api, &caller, request).await.unwrap();
    assert_eq!(listed.into_inner().jobs.len(), 1);
    off_the_worker(&probe, "ListJobs");

    let request = Request::new(CancelJobRequest { job_id: id });
    cancel::cancel_job(&api, &caller, request).await.unwrap();
    off_the_worker(&probe, "CancelJob");
}
