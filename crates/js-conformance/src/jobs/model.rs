//! [`ReferenceJobs`]: the job service in memory, answering as the real one does.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_proto::refusal::refusal_status;
use js_proto::v1::job_service_client::JobServiceClient;
use js_proto::v1::job_service_server::{JobService, JobServiceServer};
use js_proto::v1::{
    self, CancelJobRequest, CancelJobResponse, CordonHostRequest, CordonHostResponse,
    GetJobRequest, GetJobResponse, GetLogsRequest, GetLogsResponse, GetResultRequest,
    GetResultResponse, GetUsageRequest, GetUsageResponse, ListHostsRequest, ListHostsResponse,
    ListJobsRequest, ListJobsResponse, ListPoolsRequest, ListPoolsResponse, ListProvidersRequest,
    ListProvidersResponse, RefusalCode, SubmitJobRequest, SubmitJobResponse, UncordonHostRequest,
    UncordonHostResponse,
};
use tonic::transport::Channel;
use tonic::{Request, Response, Status};

use super::{JobHarness, Refused, Step, channel_to};

/// The default retention window: 60 seconds, the shortest the real service accepts.
const RETENTION: Duration = Duration::from_secs(60);

/// The name the service records for a client unless [`ReferenceJobs::new`] is given another.
const CLIENT: &str = "conformance-client";

/// The host a placed job shows.
const HOST: &str = "conformance-host";

/// How much earlier than its bound [`Defect::ResultExpiresEarly`] expires a result.
const EARLY_MS: u64 = 1_000;

/// The one rule a broken [`ReferenceJobs`] breaks. Each variant is documented with the
/// rule, and the tests of this crate name the scenarios each one fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defect {
    /// A submit of the same id and the same spec is refused as a conflict instead of
    /// answering the job already recorded.
    ResubmitConflicts,
    /// A submit of an id with another spec replaces the recorded job instead of being
    /// refused.
    ConflictOverwrites,
    /// A job whose process exits with a nonzero code ends as succeeded instead of failed.
    ExitCodeIgnored,
    /// The state machine takes any step from any state, an ended job's included, instead of
    /// refusing a step it has no edge for.
    AnyStepIsTaken,
    /// A cancel of a running job ends it as cancelled at once instead of only marking
    /// `cancel_requested`.
    CancelEndsRunning,
    /// A job that ended after its cancel was asked for, a queued job cancelled at once
    /// included, still shows `cancel_requested` instead of clearing it at the end.
    CancelKeptOnEnd,
    /// `GetResult` of a job that has not ended answers an empty result instead of
    /// `FAILED_PRECONDITION`.
    ResultBeforeEnd,
    /// The stored result of an exited job has no exit code.
    ResultLosesExitCode,
    /// A result expires one second before its retention bound instead of at it.
    ResultExpiresEarly,
    /// The retention sweep deletes nothing.
    SweepKeepsJobs,
    /// A job with a wall limit of 0 is accepted instead of refused.
    WallZeroAccepted,
    /// A job records no client.
    ClientNotRecorded,
    /// An id no job has answers `INVALID_ARGUMENT` instead of `NOT_FOUND`.
    UnknownJobIsInvalid,
}

/// One recorded job: what the service answers for it, and what it keeps beside.
#[derive(Debug)]
struct Stored {
    /// The job as every read shows it.
    job: v1::Job,
    /// The state of `job`, as the enum.
    state: v1::JobState,
    /// The submitted spec with its id set and its priority cleared: what a resubmit is
    /// compared with.
    key: v1::JobSpec,
    /// The result, once the job has ended.
    result: Option<v1::JobResult>,
    /// The instant the result expires and the sweep deletes the job, once it has ended.
    retention_ends: Option<u64>,
}

impl Stored {
    fn ended(&self) -> bool {
        self.result.is_some()
    }

    /// Ends the job in `state` at `now`, with a result of `reason` and `exit_code`. The job no
    /// longer shows `cancel_requested`: the model has no lost end, the one end that keeps it.
    fn end(
        &mut self,
        state: v1::JobState,
        exit_code: Option<i32>,
        reason: v1::ResultReason,
        clock: (u64, u64),
    ) {
        let (now, retention_ms) = clock;
        self.state = state;
        self.job.state = state.into();
        let mut times = self.job.times.unwrap_or_default();
        times.ended_at_ms = Some(now);
        self.job.times = Some(times);
        self.result = Some(v1::JobResult {
            state: state.into(),
            exit_code,
            reason: reason.into(),
            times: Some(times),
            ..v1::JobResult::default()
        });
        self.retention_ends = Some(now.saturating_add(retention_ms));
        self.job.cancel_requested = false;
    }
}

/// The service's state, behind the lock of [`ReferenceJobs`].
#[derive(Debug)]
struct Inner {
    client: String,
    defect: Option<Defect>,
    retention_ms: u64,
    now_ms: u64,
    minted: u64,
    jobs: BTreeMap<String, Stored>,
}

/// Whether `text` is a job id: 1 to 63 bytes of letters, digits, `.`, `_` and `-`.
fn valid_id(text: &str) -> bool {
    let bytes_ok = text
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() | matches!(byte, b'.' | b'_' | b'-'));
    (1..=63).contains(&text.len()) & bytes_ok
}

fn invalid(field: &str) -> Status {
    Status::invalid_argument(format!("field: {field}"))
}

fn conflict() -> Status {
    refusal_status(
        RefusalCode::JobIdConflict,
        "the job id names a different job",
    )
}

impl Inner {
    fn has(&self, defect: Defect) -> bool {
        self.defect == Some(defect)
    }

    /// `(now, retention)` in milliseconds.
    fn clock(&self) -> (u64, u64) {
        (self.now_ms, self.retention_ms)
    }

    fn mint(&mut self) -> String {
        self.minted += 1;
        format!("{:032x}", self.minted)
    }

    /// The job named `text`, with the clock: refused when `text` is no job id, and
    /// `unknown-job` when no job has it.
    fn find(&mut self, text: &str) -> Result<(&mut Stored, (u64, u64)), Status> {
        if !valid_id(text) {
            return Err(invalid("jobId"));
        }
        let gone = match self.has(Defect::UnknownJobIsInvalid) {
            true => Status::invalid_argument("unknown-job"),
            false => Status::not_found("unknown-job"),
        };
        let clock = self.clock();
        let found = self.jobs.get_mut(text);
        found.map(|stored| (stored, clock)).ok_or(gone)
    }

    fn submit(&mut self, request: SubmitJobRequest) -> Result<v1::Job, Status> {
        let spec = request.spec.ok_or_else(|| invalid("spec"))?;
        if !spec.job_id.is_empty() && !valid_id(&spec.job_id) {
            return Err(invalid("spec.jobId"));
        }
        let walls = spec.wall_limit_seconds.unwrap_or(0) == 0;
        if walls && !self.has(Defect::WallZeroAccepted) {
            return Err(refusal_status(
                RefusalCode::WallTimeNotPositive,
                "the job has no wall limit, or one of 0",
            ));
        }
        let id = match spec.job_id.is_empty() {
            true => self.mint(),
            false => spec.job_id.clone(),
        };
        let key = v1::JobSpec {
            job_id: id.clone(),
            priority: 0,
            ..spec.clone()
        };
        if let Some(held) = self.jobs.get(&id) {
            let same = held.key == key;
            if same && !self.has(Defect::ResubmitConflicts) {
                return Ok(held.job.clone());
            }
            if !self.has(Defect::ConflictOverwrites) {
                return Err(conflict());
            }
        }
        let client = Some(self.client.clone()).filter(|_| !self.has(Defect::ClientNotRecorded));
        let job = v1::Job {
            id: id.clone(),
            spec: Some(v1::JobSpec {
                job_id: id.clone(),
                ..spec
            }),
            state: v1::JobState::Queued.into(),
            times: Some(v1::JobTimes {
                queued_at_ms: self.now_ms,
                ..v1::JobTimes::default()
            }),
            client,
            ..v1::Job::default()
        };
        let stored = Stored {
            job: job.clone(),
            state: v1::JobState::Queued,
            key,
            result: None,
            retention_ends: None,
        };
        self.jobs.insert(id, stored);
        Ok(job)
    }

    fn result(&mut self, text: &str) -> Result<v1::JobResult, Status> {
        let early = match self.has(Defect::ResultExpiresEarly) {
            true => EARLY_MS,
            false => 0,
        };
        let empty = self.has(Defect::ResultBeforeEnd);
        let (stored, (now, _)) = self.find(text)?;
        if stored.retention_ends.is_some_and(|end| now + early >= end) {
            return Err(Status::not_found("expired"));
        }
        match (&stored.result, empty) {
            (Some(result), _) => Ok(result.clone()),
            (None, true) => Ok(v1::JobResult::default()),
            (None, false) => Err(Status::failed_precondition("not-finished")),
        }
    }

    fn cancel(&mut self, text: &str) -> Result<v1::Job, Status> {
        let ends_running = self.has(Defect::CancelEndsRunning);
        let kept = self.has(Defect::CancelKeptOnEnd);
        let (stored, clock) = self.find(text)?;
        if !stored.ended() && !stored.job.cancel_requested {
            stored.job.cancel_requested = true;
            let ends = stored.state == v1::JobState::Queued || ends_running;
            if ends {
                let cancelled = v1::JobState::Cancelled;
                stored.end(cancelled, None, v1::ResultReason::Cancelled, clock);
                stored.job.cancel_requested = kept;
            }
        }
        Ok(stored.job.clone())
    }

    fn step(&mut self, job: &str, step: Step) -> Result<(), Refused> {
        use v1::JobState::{Failed, Placed, Queued, Running, Starting, Succeeded};
        let any = self.has(Defect::AnyStepIsTaken);
        let ignores_code = self.has(Defect::ExitCodeIgnored);
        let loses_code = self.has(Defect::ResultLosesExitCode);
        let kept = self.has(Defect::CancelKeptOnEnd);
        let (now, retention_ms) = self.clock();
        let stored = self.jobs.get_mut(job).ok_or(Refused)?;
        let to = match step {
            Step::Place => Placed,
            Step::Start => Starting,
            Step::Run => Running,
            Step::Exit(code) if code == 0 || ignores_code => Succeeded,
            Step::Exit(_) => Failed,
        };
        let edge = matches!(
            (stored.state, to),
            (Queued, Placed)
                | (Placed, Starting)
                | (Starting, Running)
                | (Running, Succeeded | Failed)
        );
        if !edge && !any {
            return Err(Refused);
        }
        let mut times = stored.job.times.unwrap_or_default();
        match to {
            Placed => {
                times.placed_at_ms = Some(now);
                stored.job.host_id = HOST.to_string();
            }
            Running => times.started_at_ms = Some(now),
            _ => {}
        }
        stored.job.times = Some(times);
        stored.state = to;
        stored.job.state = to.into();
        if let Step::Exit(code) = step {
            let code = Some(code).filter(|_| !loses_code);
            let clock = (now, retention_ms);
            let asked = stored.job.cancel_requested;
            stored.end(to, code, v1::ResultReason::Exit, clock);
            stored.job.cancel_requested = asked && kept;
        }
        Ok(())
    }

    fn sweep(&mut self) {
        let now = self.now_ms;
        if !self.has(Defect::SweepKeepsJobs) {
            self.jobs
                .retain(|_, stored| stored.retention_ends.is_none_or(|end| now < end));
        }
    }
}

/// The job service in memory, answering as the real one does (`JobApi` of
/// `js-lease-server`) for `SubmitJob`, `GetJob`, `ListJobs`, `CancelJob` and `GetResult`.
/// The other calls (`GetLogs` and the fleet calls) answer `UNIMPLEMENTED`, as the real
/// service does for a call it is not given.
///
/// The model checks only what the scenarios exercise: that a submit has a spec, that the
/// job id is syntactically valid, and that the wall limit is above 0. It does not check
/// the image, the needs or the other fields, has no ceiling on the wall limit, and does no
/// admission. A job is compared on resubmit by its spec as sent (priority aside), where the
/// real service compares the decoded job. `ListJobs` answers every job in id order and
/// ignores filters and page sizes. A job submitted without an id gets 32 hex digits from a
/// counter, so the ids of one service are deterministic.
///
/// The handle is cheap to clone, and every clone is the same service: a fake of another
/// crate serves one clone to its client and calls the hooks ([`Self::step`],
/// [`Self::advance`], [`Self::sweep`]) on another. Time is a clock in milliseconds that
/// moves only by [`Self::advance`].
#[derive(Debug, Clone)]
pub struct ReferenceJobs {
    inner: Arc<Mutex<Inner>>,
}

impl ReferenceJobs {
    /// A service that records `client_name` as the client of every job submitted, with a
    /// retention window of 60 seconds and its clock at 0.
    #[must_use]
    pub fn new(client_name: &str) -> ReferenceJobs {
        ReferenceJobs::build(client_name, None)
    }

    /// A service named as [`Self::new`] names its client, that breaks the rule `defect`.
    #[must_use]
    pub fn broken(defect: Defect) -> ReferenceJobs {
        ReferenceJobs::build(CLIENT, Some(defect))
    }

    fn build(client_name: &str, defect: Option<Defect>) -> ReferenceJobs {
        let inner = Inner {
            client: client_name.to_string(),
            defect,
            retention_ms: duration_millis(RETENTION),
            now_ms: 0,
            minted: 0,
            jobs: BTreeMap::new(),
        };
        ReferenceJobs {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The name recorded as the client of every job submitted.
    #[must_use]
    pub fn client_name(&self) -> String {
        self.lock().client.clone()
    }

    /// How long an ended job's result is kept: 60 seconds unless
    /// [`Self::set_retention`] set another.
    #[must_use]
    pub fn retention(&self) -> Duration {
        Duration::from_millis(self.lock().retention_ms)
    }

    /// Sets the retention window of the jobs that end from now on. The real service
    /// refuses a window under 60 seconds; this does not check.
    pub fn set_retention(&self, window: Duration) {
        self.lock().retention_ms = duration_millis(window);
    }

    /// Moves the clock forward by `by`, in whole milliseconds.
    pub fn advance(&self, by: Duration) {
        let mut inner = self.lock();
        inner.now_ms = inner.now_ms.saturating_add(duration_millis(by));
    }

    /// Deletes every ended job whose retention end the clock has reached; a deleted job is
    /// unknown, and its id is free again.
    pub fn sweep(&self) {
        self.lock().sweep();
    }

    /// Makes one move of the state machine of job `job`: queued to placed by
    /// [`Step::Place`], placed to starting by [`Step::Start`], starting to running by
    /// [`Step::Run`], running to succeeded by [`Step::Exit`] of 0 and to failed by any
    /// other code. A placement stamps the host and the placed time, running stamps the
    /// started time, and an exit ends the job with its result and its retention end.
    ///
    /// # Errors
    /// [`Refused`] when the job is in no state the step starts from, or no job has the id.
    pub fn step(&self, job: &str, step: Step) -> Result<(), Refused> {
        self.lock().step(job, step)
    }
}

/// `duration` in whole milliseconds, the largest value when it does not fit.
fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl ReferenceJobs {
    /// A fresh service (client name `conformance-client`) and the harness over it.
    pub fn harness() -> impl Future<Output = ReferenceHarness> {
        std::future::ready(ReferenceHarness::of(ReferenceJobs::new(CLIENT)))
    }

    /// A fresh service that breaks the rule `defect`, and the harness over it.
    pub fn broken_harness(defect: Defect) -> impl Future<Output = ReferenceHarness> {
        std::future::ready(ReferenceHarness::of(ReferenceJobs::broken(defect)))
    }
}

/// A [`JobHarness`] over a [`ReferenceJobs`]: the client talks to the service through a
/// channel from [`channel_to`], and the hooks call the service's own.
#[derive(Debug, Clone)]
pub struct ReferenceHarness {
    jobs: ReferenceJobs,
    client: JobServiceClient<Channel>,
}

impl ReferenceHarness {
    /// The harness over `jobs`. Call it inside a tokio runtime: the service is served by a
    /// task of it.
    #[must_use]
    pub fn of(jobs: ReferenceJobs) -> ReferenceHarness {
        let channel = channel_to(JobServiceServer::new(jobs.clone()));
        ReferenceHarness {
            jobs,
            client: JobServiceClient::new(channel),
        }
    }

    /// The service the harness drives.
    #[must_use]
    pub fn jobs(&self) -> &ReferenceJobs {
        &self.jobs
    }
}

impl JobHarness for ReferenceHarness {
    fn client(&mut self) -> JobServiceClient<Channel> {
        self.client.clone()
    }

    fn client_name(&self) -> String {
        self.jobs.client_name()
    }

    fn retention(&self) -> Duration {
        self.jobs.retention()
    }

    fn step(&mut self, job: &str, step: Step) -> Result<(), Refused> {
        self.jobs.step(job, step)
    }

    fn advance(&mut self, by: Duration) {
        self.jobs.advance(by);
    }

    fn sweep(&mut self) {
        self.jobs.sweep();
    }
}

/// The answer of a call the model does not serve.
fn unserved<T>() -> Result<Response<T>, Status> {
    Err(Status::unimplemented("unimplemented"))
}

#[tonic::async_trait]
impl JobService for ReferenceJobs {
    async fn submit_job(
        &self,
        request: Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        let job = self.lock().submit(request.into_inner())?;
        Ok(Response::new(SubmitJobResponse { job: Some(job) }))
    }

    async fn get_job(
        &self,
        request: Request<GetJobRequest>,
    ) -> Result<Response<GetJobResponse>, Status> {
        let mut inner = self.lock();
        let (stored, _) = inner.find(&request.into_inner().job_id)?;
        let job = Some(stored.job.clone());
        Ok(Response::new(GetJobResponse { job }))
    }

    async fn list_jobs(
        &self,
        _: Request<ListJobsRequest>,
    ) -> Result<Response<ListJobsResponse>, Status> {
        let jobs = self
            .lock()
            .jobs
            .values()
            .map(|stored| stored.job.clone())
            .collect();
        Ok(Response::new(ListJobsResponse {
            jobs,
            next_page_token: String::new(),
        }))
    }

    async fn cancel_job(
        &self,
        request: Request<CancelJobRequest>,
    ) -> Result<Response<CancelJobResponse>, Status> {
        let job = self.lock().cancel(&request.into_inner().job_id)?;
        Ok(Response::new(CancelJobResponse { job: Some(job) }))
    }

    async fn get_result(
        &self,
        request: Request<GetResultRequest>,
    ) -> Result<Response<GetResultResponse>, Status> {
        let result = self.lock().result(&request.into_inner().job_id)?;
        Ok(Response::new(GetResultResponse {
            result: Some(result),
        }))
    }

    async fn get_logs(
        &self,
        _: Request<GetLogsRequest>,
    ) -> Result<Response<GetLogsResponse>, Status> {
        unserved()
    }

    async fn list_hosts(
        &self,
        _: Request<ListHostsRequest>,
    ) -> Result<Response<ListHostsResponse>, Status> {
        unserved()
    }

    async fn cordon_host(
        &self,
        _: Request<CordonHostRequest>,
    ) -> Result<Response<CordonHostResponse>, Status> {
        unserved()
    }

    async fn uncordon_host(
        &self,
        _: Request<UncordonHostRequest>,
    ) -> Result<Response<UncordonHostResponse>, Status> {
        unserved()
    }

    async fn list_pools(
        &self,
        _: Request<ListPoolsRequest>,
    ) -> Result<Response<ListPoolsResponse>, Status> {
        unserved()
    }

    async fn list_providers(
        &self,
        _: Request<ListProvidersRequest>,
    ) -> Result<Response<ListProvidersResponse>, Status> {
        unserved()
    }

    async fn get_usage(
        &self,
        _: Request<GetUsageRequest>,
    ) -> Result<Response<GetUsageResponse>, Status> {
        unserved()
    }
}
