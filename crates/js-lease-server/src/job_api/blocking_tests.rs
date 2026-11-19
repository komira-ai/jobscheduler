//! Every call of the job service that reads or writes the job tables does so on the
//! runtime's pool for blocking calls, never on the worker that runs the call: the store's
//! calls block (on a slow database, up to its statement timeout), and a worker they hold
//! serves no connection meanwhile.
//!
//! The tests run on a one-thread runtime, so the worker is the test's own thread; the store
//! under the tables records the thread of each of its calls, and every recorded thread must
//! be another one. A call made on the worker is seen at once, with no wait on time.
//!
//! The gate on those calls ([`JobApi::on_tables`]) is tested the same way, with a store that
//! can be stalled: its calls wait until it is opened, and it counts how many wait at once.

use std::future::Future;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread::{self, ThreadId};
use std::time::Duration;

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

/// The fake store, recording the thread each of its calls ran on; while stalled, each call
/// waits until it is opened.
struct Probe {
    inner: Fake,
    threads: Mutex<Vec<ThreadId>>,
    /// Whether calls wait, how many are waiting, and the most that ever waited at once.
    stall: Mutex<(bool, usize, usize)>,
    changed: Condvar,
}

impl Probe {
    fn note(&self) {
        let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        threads.push(thread::current().id());
        drop(threads);
        let mut stall = self.stall.lock().unwrap_or_else(PoisonError::into_inner);
        stall.1 += 1;
        stall.2 = stall.2.max(stall.1);
        self.changed.notify_all();
        let mut stall = self
            .changed
            .wait_while(stall, |stall| stall.0)
            .unwrap_or_else(PoisonError::into_inner);
        stall.1 -= 1;
    }

    /// Makes every call from now on wait (`true`) or lets them all go (`false`).
    fn stalled(&self, stalled: bool) {
        self.stall.lock().unwrap_or_else(PoisonError::into_inner).0 = stalled;
        self.changed.notify_all();
    }

    /// Waits until `n` calls are waiting in the store.
    fn until_waiting(&self, n: usize) {
        let stall = self.stall.lock().unwrap_or_else(PoisonError::into_inner);
        drop(
            self.changed
                .wait_while(stall, |stall| stall.1 < n)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    /// The most calls that ever waited in the store at once.
    fn most(&self) -> usize {
        self.stall.lock().unwrap_or_else(PoisonError::into_inner).2
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

    fn now(&self) -> Result<StoreTime, StoreError> {
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
        stall: Mutex::default(),
        changed: Condvar::new(),
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

/// The service over `jobs` with a gate of `size` places and `wait`, holding the job `job-g`.
async fn gated(jobs: Arc<ServerJobs>, size: usize, wait: Duration) -> Arc<JobApi> {
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(0)));
    let api = JobApi::new(jobs, clock)
        .with_submit(Ctx::served(CEILING, Fixed::new(true)))
        .with_store_calls(size, wait);
    let request = Request::new(SubmitJobRequest {
        spec: Some(wire("job-g")),
    });
    submit::submit_job(&api, &operator(), request)
        .await
        .unwrap();
    Arc::new(api)
}

/// A `GetJob` of `job-g`.
fn get_g() -> Request<GetJobRequest> {
    Request::new(GetJobRequest {
        job_id: "job-g".to_owned(),
    })
}

/// Asserts that a stalled store holds no more of the job service's calls than its gate has
/// places: of ten reads at once against a store that never answers, three (the places) wait in
/// the store and the seven others answer at once `UNAVAILABLE` `store-unavailable`, without
/// reaching it; when the store answers, the three do. Catches calls on the job tables with no
/// bound, each of which parks a thread of the blocking pool for as long as the store stalls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_store_holds_no_more_calls_than_the_gate_has_places() {
    let (probe, jobs) = probed();
    let api = gated(jobs, 3, Duration::ZERO).await;
    probe.stalled(true);
    let (done, mut answers) = tokio::sync::mpsc::unbounded_channel();
    for _ in 0..10 {
        let (api, done) = (Arc::clone(&api), done.clone());
        tokio::spawn(async move {
            let answer = read::get_job(&api, &operator(), get_g()).await;
            let _ = done.send(
                answer
                    .map(drop)
                    .map_err(|status| (status.code(), status.message().to_owned())),
            );
        });
    }
    let waiting = Arc::clone(&probe);
    tokio::task::spawn_blocking(move || waiting.until_waiting(3))
        .await
        .unwrap();
    for _ in 0..7 {
        assert_eq!(
            answers.recv().await.unwrap(),
            Err((tonic::Code::Unavailable, "store-unavailable".to_owned()))
        );
    }
    probe.stalled(false);
    for _ in 0..3 {
        assert_eq!(answers.recv().await.unwrap(), Ok(()));
    }
    assert_eq!(probe.most(), 3, "more calls than places reached the store");
}

/// Asserts that a call that finds every place taken waits for one, and past the gate's wait
/// answers as a busy store: a write `UNAVAILABLE` `state-write-failed`, a read `UNAVAILABLE`
/// `store-unavailable`, neither reaching the store; with the place back both are served.
#[tokio::test]
async fn a_call_with_no_place_free_answers_unavailable() {
    let (probe, jobs) = probed();
    let api = gated(jobs, 1, Duration::ZERO).await;
    probe.take();
    let held = Arc::clone(&api.gate.slots).try_acquire_owned().unwrap();
    let read = read::get_job(&api, &operator(), get_g()).await.unwrap_err();
    assert_eq!(
        (read.code(), read.message()),
        (tonic::Code::Unavailable, "store-unavailable")
    );
    let request = Request::new(CancelJobRequest {
        job_id: "job-g".to_owned(),
    });
    let write = cancel::cancel_job(&api, &operator(), request)
        .await
        .unwrap_err();
    assert_eq!(
        (write.code(), write.message()),
        (tonic::Code::Unavailable, "state-write-failed")
    );
    assert!(
        probe.take().is_empty(),
        "a call without a place reached the store"
    );
    drop(held);
    read::get_job(&api, &operator(), get_g()).await.unwrap();
}

/// Asserts that a call still waiting for a place when its client gives up (its future is
/// dropped) never runs: the store sees no call from it, and its place is not taken. Catches a
/// call handed to the blocking pool before it has a place, which a stall would leave parked.
#[tokio::test]
async fn a_waiting_call_whose_client_is_gone_never_runs() {
    let (probe, jobs) = probed();
    let api = gated(jobs, 1, Duration::from_secs(3600)).await;
    probe.take();
    read::get_job(&api, &operator(), get_g()).await.unwrap();
    let one_read = probe.take().len();
    let held = Arc::clone(&api.gate.slots).try_acquire_owned().unwrap();
    {
        let caller = operator();
        let waiting = read::get_job(&api, &caller, get_g());
        let mut waiting = std::pin::pin!(waiting);
        let polled = waiting
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(polled, Poll::Pending), "the call did not wait");
    }
    drop(held);
    assert_eq!(api.gate.slots.available_permits(), 1);
    read::get_job(&api, &operator(), get_g()).await.unwrap();
    assert_eq!(
        probe.take().len(),
        one_read,
        "the dropped call reached the store"
    );
}

/// Asserts that a call that finds every place taken waits, and runs as soon as a place comes
/// back: polled with the place held it is still waiting and has not reached the store; once
/// the place is given back it is served. Catches a wait that gives up before its time, and a
/// place given back that does not wake the call waiting for it.
#[tokio::test]
async fn a_waiting_call_runs_when_a_place_comes_back() {
    let (probe, jobs) = probed();
    let api = gated(jobs, 1, Duration::from_secs(3600)).await;
    probe.take();
    let held = Arc::clone(&api.gate.slots).try_acquire_owned().unwrap();
    let caller = operator();
    let waiting = read::get_job(&api, &caller, get_g());
    let mut waiting = std::pin::pin!(waiting);
    let polled = waiting
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()));
    assert!(matches!(polled, Poll::Pending), "the call did not wait");
    assert!(
        probe.take().is_empty(),
        "the waiting call reached the store"
    );
    drop(held);
    let job = waiting.await.unwrap().into_inner();
    assert_eq!(job.job.unwrap().id, "job-g");
}
