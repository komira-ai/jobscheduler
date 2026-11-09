//! The read calls: `GetJob`, `ListJobs`, `GetResult` and `GetLogs`, served from the job
//! tables. The group's own state is [`Ctx`], set through [`JobApi::with_read`].
//!
//! Every call reads the tables through [`Reads`], which offers the two reads they need (one
//! job, one page of jobs) and no write, so no read call changes any state. None takes a lock:
//! the service holds no handle on the registry. Time is read from the
//! service's clock, and only to tell an ended job whose retention end has passed.
//!
//! # Answers
//!
//! | case | gRPC | gateway |
//! |---|---|---|
//! | a `job_id` that is not a job id | `INVALID_ARGUMENT`, `field: jobId` | 400 `invalid-argument`, `field` `jobId` |
//! | no row for the job: never submitted, or swept after its retention window | `NOT_FOUND`, `unknown-job` | 404 `unknown-job` |
//! | `GetResult` or `GetLogs` of a job whose row is still there with its retention end passed (the sweep runs on the tick) | `NOT_FOUND`, `expired` | 410 `expired` |
//! | `GetResult` of a job that has not ended | `FAILED_PRECONDITION`, `not-finished` | 409 `not-finished` |
//! | a `page_token` no listing gave | `INVALID_ARGUMENT`, `field: pageToken` | 400 `invalid-argument`, `field` `pageToken` |
//! | a read of the store that failed | `INTERNAL`, `internal` | 500 `internal` |
//!
//! A job of another client, asked for by a caller of the role `client`, is answered as one
//! with no row (`unknown-job`, before any other check of the job), and a listing of such a
//! caller keeps its own jobs only ([`super::roles`]).
//!
//! A swept job has no tombstone: it is the same unknown job as one that never existed.
//! `GetJob` and `ListJobs` show a job until it is swept, its retention end passed or not.
//!
//! # Host silence
//!
//! `GetJob` and `ListJobs` show, for a job that has not ended and has a host, since when that
//! host has missed its grace (`host_silent_since_ms`, design section 6.3), as the context's
//! [`Silence`] source says (the registry's, [`Ctx::with_silence`]); the field is absent while
//! the host is heard, for a job with no host or one that has ended, and without a source. It is
//! read at each call and drops the moment the host is heard again. No clock turns silence into
//! a change of the job's state.
//!
//! # `GetLogs` without a stream
//!
//! A request that names no stream (or a stream this version does not know) is the call as
//! it was before streams were added. The answer is the job's state and the tails of its
//! result: for an ended job, the last 8 KiB of each stream it stored and whether either was
//! cut; for a job that has not ended, and for one that ended without a result (lost, or
//! failed to start), empty tails and `truncated` false, never an error, and the fields of a
//! stream's page empty. Live output, of a running job too, is read with a stream: the row is
//! read here, by the rules above, and the page by the logs group (`logs.rs`), with its own
//! answers.
//!
//! # `ListJobs`
//!
//! Filters: the states (any of them; a state this server does not know matches no job), the
//! labels (every one, with the same value), the client that submitted the job (a job recorded
//! with no client matches no client), and the time a job was queued, half-open:
//! `queued_after_ms` is in, `queued_before_ms` is out. The jobs come in the order of the
//! table's keys, which is the order of their ids. A page holds [`DEFAULT_PAGE_SIZE`] jobs
//! when the request asks for 0, and at most [`MAX_PAGE_SIZE`]: a larger page size is cut
//! to it, not refused.
//!
//! A call reads the table in windows of [`SCAN_WINDOW`] rows, keeps the jobs that pass every
//! filter, and reads the next window until the page is full, the table ends, or it has read
//! [`SCAN_BOUND`] rows. So a page is short only at the end of the table or at the bound,
//! and a page cut at the bound carries a next page token although it may hold few jobs or
//! none. When only one state is asked for, the table's scan filters by it.
//!
//! The page token is a key of the job table (`job/<id>`): the page's last job when the page
//! filled inside a read, else the last row the call read. For a caller of the role `client`
//! the token is always the page's last job, which is its own: the last row read may be
//! another client's, and its key would name that job. A client's page cut at the bound
//! with none of its jobs in it reads on past the bound until it holds one of its jobs or the
//! table ends, so such a listing may read the whole table. The next page starts after it; a
//! token that is not such a key is refused. A job submitted or swept
//! between pages is listed by a later page if its id sorts after the token, and no job is
//! listed twice.

use std::num::NonZeroUsize;
use std::sync::Arc;

use js_jobs::{ClientName, JobId, JobPage, JobRecord, JobState, MAX_PAGE, keys};
use js_proto::v1::{
    self, GetJobRequest, GetJobResponse, GetLogsRequest, GetLogsResponse, GetResultRequest,
    GetResultResponse, ListJobsRequest, ListJobsResponse,
};
use js_state::{ResumeToken, StoreTime};
use js_types::Clock;
use js_types::HostId;
use tonic::{Request, Response, Status};

use super::fleet::Handle;
use super::roles::{Caller, hidden};
use super::submit::unavailable_if_fenced;
use super::{JobApi, logs};

pub(crate) mod view;

/// The page size of a listing that asks for 0.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// The largest page of a listing: a request for more is answered this many at most.
pub const MAX_PAGE_SIZE: u32 = 1000;

/// The most rows one read of the table returns while a listing is served: the store's own
/// page limit.
pub const SCAN_WINDOW: NonZeroUsize = MAX_PAGE;

/// The most rows one `ListJobs` call reads: ten windows.
pub const SCAN_BOUND: NonZeroUsize = NonZeroUsize::MIN.saturating_add(9_999);

/// The field a bad job id is named by: `job_id`, by its JSON name.
const JOB_ID_FIELD: &str = "jobId";

/// The field a bad page token is named by: `page_token`, by its JSON name.
const PAGE_TOKEN_FIELD: &str = "pageToken";

/// Since when a host has been silent past its grace (design section 6.3), as the read calls
/// show it.
pub(crate) trait Silence: Send + Sync {
    /// The instant `host` missed its grace, in milliseconds since the Unix epoch; `None` while
    /// the host is heard within it, and for a host no one holds.
    fn silent_since(&self, host: &HostId) -> Option<u64>;
}

/// The state of the read group: how a listing reads the table, and where host silence is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ctx {
    /// The most rows one read of the table returns: [`SCAN_WINDOW`].
    pub(super) scan_window: NonZeroUsize,
    /// The most rows one `ListJobs` call reads: [`SCAN_BOUND`].
    pub(super) scan_bound: NonZeroUsize,
    /// Where a job's host silence is read; `None` shows none.
    pub(super) silence: Option<Handle<dyn Silence>>,
    /// A value only tests set, so that a test can tell a context it gave from the default.
    #[cfg(test)]
    pub(super) mark: u32,
}

impl Default for Ctx {
    fn default() -> Ctx {
        Ctx {
            scan_window: SCAN_WINDOW,
            scan_bound: SCAN_BOUND,
            silence: None,
            #[cfg(test)]
            mark: 0,
        }
    }
}

impl Ctx {
    /// The same context reading a job's host silence from `silence`.
    #[must_use]
    pub(crate) fn with_silence(self, silence: Arc<dyn Silence>) -> Ctx {
        Ctx {
            silence: Some(Handle(silence)),
            ..self
        }
    }
}

/// `job` as the read calls show it: its record, with the silence of its host when it has not
/// ended and has one, as `ctx` reads it.
fn shown(ctx: &Ctx, job: &JobRecord) -> v1::Job {
    let holding = job.host.as_ref().filter(|_| !job.state.is_ended());
    let source = ctx.silence.as_ref();
    let silent = holding
        .zip(source)
        .and_then(|(host, source)| source.0.silent_since(host));
    v1::Job {
        host_silent_since_ms: silent,
        ..view::job(job)
    }
}

impl JobApi {
    /// The service with `read` as the read group's context; every other part is kept.
    #[must_use]
    pub fn with_read(self, read: Ctx) -> JobApi {
        JobApi { read, ..self }
    }
}

/// The answer to a job that has no row: `unknown-job`.
fn unknown_job() -> Status {
    Status::not_found("unknown-job")
}

/// The answer to a result or logs past the retention end: `expired`.
fn expired() -> Status {
    Status::not_found("expired")
}

/// The answer to the result of a job that has not ended: `not-finished`.
fn not_finished() -> Status {
    Status::failed_precondition("not-finished")
}

/// The answer to a request whose `field` (by its JSON name) is refused.
fn invalid(field: &str) -> Status {
    Status::invalid_argument(format!("field: {field}"))
}

/// The answer to a read of the store that failed. The cause is logged, never answered.
/// `UNAVAILABLE` `state-fenced` instead when the store says this process is no longer its
/// writer, which trips the fence ([`super::submit::unavailable_if_fenced`]).
fn read_failed(api: &JobApi, error: &js_jobs::JobStoreError) -> Status {
    tracing::warn!(%error, "a read of the job tables failed");
    unavailable_if_fenced(api, error, Status::internal("internal"))
}

/// The reads the read calls (and the logs group's follows) make of the job tables, and
/// nothing else.
pub(super) struct Reads<'a>(pub(super) &'a JobApi);

impl Reads<'_> {
    /// The job `text` names, as `caller` may read it: refused when `text` is not a job id,
    /// `unknown-job` when there is no row for it, and the same answer, marked hidden, when
    /// the job is not the caller's to read.
    pub(super) fn job(&self, text: &str, caller: &Caller) -> Result<JobRecord, Status> {
        let id = JobId::parse(text).map_err(|_| invalid(JOB_ID_FIELD))?;
        let found = self
            .0
            .jobs
            .get_job(&id)
            .map_err(|error| read_failed(self.0, &error))?;
        let job = found.ok_or_else(unknown_job)?;
        match caller.owns(&job) {
            true => Ok(job),
            false => Err(hidden()),
        }
    }

    /// One window of the table: at most `window` rows after `resume`, keeping the jobs in
    /// `state` when one is given.
    fn window(
        &self,
        state: Option<JobState>,
        resume: Option<&ResumeToken>,
        window: NonZeroUsize,
    ) -> Result<JobPage, Status> {
        self.0
            .jobs
            .list_jobs(state, resume, window)
            .map_err(|error| read_failed(self.0, &error))
    }
}

/// The service's clock, as a reading of the store's clock: both count from the Unix epoch.
fn now(api: &JobApi) -> StoreTime {
    StoreTime::from_duration(std::time::Duration::from_millis(api.clock.now().millis()))
}

/// `job`, unless it has ended and its retention end has passed at the service's clock: the
/// same instant the sweep deletes it from.
pub(super) fn unexpired(api: &JobApi, job: JobRecord) -> Result<JobRecord, Status> {
    match js_jobs::sweep([&job], now(api)).is_empty() {
        true => Ok(job),
        false => Err(expired()),
    }
}

/// `GetJob`: the job's record, as long as its row exists.
pub(super) async fn get_job(
    api: &JobApi,
    caller: &Caller,
    request: Request<GetJobRequest>,
) -> Result<Response<GetJobResponse>, Status> {
    let job = Reads(api).job(&request.into_inner().job_id, caller)?;
    Ok(Response::new(GetJobResponse {
        job: Some(shown(&api.read, &job)),
    }))
}

/// `GetResult`: the stored result of an ended job, until its retention end.
pub(super) async fn get_result(
    api: &JobApi,
    caller: &Caller,
    request: Request<GetResultRequest>,
) -> Result<Response<GetResultResponse>, Status> {
    let job = Reads(api).job(&request.into_inner().job_id, caller)?;
    let job = unexpired(api, job)?;
    match job.state.is_ended() {
        true => Ok(Response::new(GetResultResponse {
            result: Some(view::result(&job)),
        })),
        false => Err(not_finished()),
    }
}

/// `GetLogs`: until the job's retention end, without a stream the job's state and its
/// result's tails (empty while it has no result), and with one a page of that stream
/// (`logs.rs`).
pub(super) async fn get_logs(
    api: &JobApi,
    caller: &Caller,
    request: Request<GetLogsRequest>,
) -> Result<Response<GetLogsResponse>, Status> {
    let request = request.into_inner();
    let job = Reads(api).job(&request.job_id, caller)?;
    let job = unexpired(api, job)?;
    let answer = match logs::stream_of(&request) {
        None => tails(&job),
        Some(stream) => logs::get_logs(api, caller, job, &request, stream).await?,
    };
    Ok(Response::new(answer))
}

/// The tails `GetLogs` answers for `job`: its result's, or none.
fn tails(job: &JobRecord) -> GetLogsResponse {
    let state = view::state(job.state).into();
    match &job.result {
        Some(result) => GetLogsResponse {
            state,
            stdout_tail: result.stdout_tail.as_bytes().to_vec(),
            stderr_tail: result.stderr_tail.as_bytes().to_vec(),
            truncated: result.truncated,
            ..GetLogsResponse::default()
        },
        None => GetLogsResponse {
            state,
            ..GetLogsResponse::default()
        },
    }
}

/// What a listing keeps: the filters of a `ListJobs` request.
struct Filter {
    /// The states, as the values of the job service's enum; empty: every state.
    states: Vec<i32>,
    /// The labels every kept job carries with the same value.
    labels: Vec<(String, String)>,
    /// Kept: queued at or after this time, in milliseconds.
    after: Option<u64>,
    /// Kept: queued before this time, in milliseconds.
    before: Option<u64>,
    /// Kept: submitted by the client of this name.
    client: Option<String>,
    /// Kept: submitted by this client, whatever the request asked: the caller's own name
    /// when it is a client, none for an operator.
    owner: Option<ClientName>,
}

impl Filter {
    /// Whether `job` passes every filter.
    fn keeps(&self, job: &JobRecord) -> bool {
        let queued = view::millis(job.submitted_at);
        let state = i32::from(view::state(job.state));
        let in_state = self.states.is_empty() || self.states.contains(&state);
        let labelled = self
            .labels
            .iter()
            .all(|(key, value)| job.spec.labels.get(key) == Some(value.as_str()));
        let after = self.after.is_none_or(|after| queued >= after);
        let before = self.before.is_none_or(|before| queued < before);
        let by = job.client.as_ref().map(ClientName::as_str);
        let client = self
            .client
            .as_deref()
            .is_none_or(|client| by == Some(client));
        let owned = self
            .owner
            .as_ref()
            .is_none_or(|owner| job.client.as_ref() == Some(owner));
        [in_state, labelled, after, before, client, owned]
            .into_iter()
            .all(|kept| kept)
    }

    /// The one state the table's scan can filter by: the states ask for exactly one, and it
    /// is one state of the table (failed to start is one per start failure there).
    fn scan_state(&self) -> Option<JobState> {
        let [one] = self.states.as_slice() else {
            return None;
        };
        match v1::JobState::try_from(*one) {
            Ok(v1::JobState::Queued) => Some(JobState::Queued),
            Ok(v1::JobState::Placed) => Some(JobState::Placed),
            Ok(v1::JobState::Starting) => Some(JobState::Starting),
            Ok(v1::JobState::Running) => Some(JobState::Running),
            Ok(v1::JobState::Succeeded) => Some(JobState::Succeeded),
            Ok(v1::JobState::Failed) => Some(JobState::Failed),
            Ok(v1::JobState::Cancelled) => Some(JobState::Cancelled),
            Ok(v1::JobState::Lost) => Some(JobState::Lost),
            Ok(v1::JobState::FailedToStart | v1::JobState::Unspecified) | Err(_) => None,
        }
    }
}

/// The page size a request for `asked` jobs gets.
fn page_size(asked: u32) -> usize {
    let size = match asked {
        0 => DEFAULT_PAGE_SIZE,
        asked => asked.min(MAX_PAGE_SIZE),
    };
    usize::try_from(size).unwrap_or(usize::MAX)
}

/// The place a page token names: empty for the first page, else the key of a job row.
fn resume_of(token: &str) -> Result<Option<ResumeToken>, Status> {
    if token.is_empty() {
        return Ok(None);
    }
    let id = token
        .strip_prefix(keys::JOB)
        .and_then(|id| JobId::parse(id).ok())
        .ok_or_else(|| invalid(PAGE_TOKEN_FIELD))?;
    Ok(Some(ResumeToken::after(keys::job(&id))))
}

/// The page token that resumes after the job `last`; none when there is no job.
fn token_after(last: Option<&JobRecord>) -> Option<ResumeToken> {
    last.map(|job| ResumeToken::after(keys::job(&job.id)))
}

/// `ListJobs`: a page of the jobs that pass the filters, in id order.
pub(super) async fn list_jobs(
    api: &JobApi,
    caller: &Caller,
    request: Request<ListJobsRequest>,
) -> Result<Response<ListJobsResponse>, Status> {
    let request = request.into_inner();
    let resume = resume_of(&request.page_token)?;
    let size = page_size(request.page_size);
    let filter = Filter {
        states: request.states,
        labels: request.labels.into_iter().collect(),
        after: request.queued_after_ms,
        before: request.queued_before_ms,
        client: request.client,
        owner: caller.only_jobs_of().cloned(),
    };
    let (jobs, next) = list(&api.read, Reads(api), &filter, resume, size)?;
    Ok(Response::new(ListJobsResponse {
        jobs: jobs.iter().map(|job| shown(&api.read, job)).collect(),
        next_page_token: next.map(|token| token.key().to_owned()).unwrap_or_default(),
    }))
}

/// Up to `size` jobs that `filter` keeps, from `resume` on, reading windows of the table
/// until the page is full, the table ends, or the context's bound of rows is read; with the
/// token of the next page, or none when the table has ended.
fn list(
    ctx: &Ctx,
    reads: Reads<'_>,
    filter: &Filter,
    mut resume: Option<ResumeToken>,
    size: usize,
) -> Result<(Vec<JobRecord>, Option<ResumeToken>), Status> {
    let scan_state = filter.scan_state();
    let bound = ctx.scan_bound.get();
    let mut read = 0;
    let mut jobs = Vec::new();
    loop {
        let window = match NonZeroUsize::new(bound.saturating_sub(read)) {
            Some(rest) => ctx.scan_window.min(rest),
            None => ctx.scan_window,
        };
        let page = reads.window(scan_state, resume.as_ref(), window)?;
        for job in page.jobs.into_iter().filter(|job| filter.keeps(job)) {
            if jobs.len() == size {
                let next = token_after(jobs.last());
                return Ok((jobs, next));
            }
            jobs.push(job);
        }
        let Some(next) = page.resume else {
            return Ok((jobs, None));
        };
        read += window.get();
        let cut = jobs.len() == size || read >= bound;
        match (cut, filter.owner.is_some(), jobs.last()) {
            (false, _, _) => {}
            (true, false, _) => return Ok((jobs, Some(next))),
            (true, true, Some(last)) => {
                let after = token_after(Some(last));
                return Ok((jobs, after));
            }
            // Nothing of the caller's yet: the token would name a row of another client's,
            // so the scan reads on past the bound.
            (true, true, None) => {}
        }
        resume = Some(next);
    }
}
