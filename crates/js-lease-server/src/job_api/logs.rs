//! `GetLogs` with a stream: one page of a job's stdout or stderr, read from the log store
//! with the server's read key (`docs/design/live-logs.md`, sections 3.6 and 8). The group's
//! state is [`Ctx`], set through [`JobApi::with_logs`]; `read.rs` reads the job's row and
//! hands a request that names a stream here ([`get_logs`]).
//!
//! The bytes pass through: each page is read from the store for the answer and kept nowhere,
//! neither in the job tables nor in memory after the answer. The object keys are built from
//! the configured log prefix, the host the attempt read ran on (its host's prefix,
//! `js_logmint::host_prefix`, the one the host's credential covers), the attempt's number and
//! the job's id alone (`crate::logs::attempt`), never from a client's text. A job that was never placed has
//! no host and printed nothing. No lock of the server is held: the row is read through the job tables'
//! read path, and every store request runs on the pool for blocking calls.
//!
//! # Answers
//!
//! | case | gRPC | gateway |
//! |---|---|---|
//! | the server has `sink = "none"` | `FAILED_PRECONDITION`, `no-log-sink` | 409 `no-log-sink` |
//! | a job with no live logs: its id has no log prefix, or it ended with a result that reports no stream lengths (its start carried no sink) | `FAILED_PRECONDITION`, `no-live-logs` | 409 `no-live-logs` |
//! | an attempt with no live logs: the log prefix leaves no room for its prefix, or it is below the current attempt and neither the record nor a row holds it | `FAILED_PRECONDITION`, `no-live-logs` | 409 `no-live-logs` |
//! | `max_bytes` of 0 | `INVALID_ARGUMENT`, `field: maxBytes` | 400 `invalid-argument`, `field` `maxBytes` |
//! | `attempt` of 0 | `INVALID_ARGUMENT`, `field: attempt` | 400 `invalid-argument`, `field` `attempt` |
//! | `attempt` above the job's current attempt | `OUT_OF_RANGE`, `field: attempt` | 400 `out-of-range`, `field` `attempt` |
//! | `from_offset` past the length of an ended job's stream | `OUT_OF_RANGE`, `field: fromOffset` | 400 `out-of-range`, `field` `fromOffset` |
//! | [`Ctx`]'s bound of waiting follows reached | `RESOURCE_EXHAUSTED`, `too-many-follows` | 429 `too-many-follows` |
//! | the store unreachable, slow, failing, or refusing the read key | `UNAVAILABLE`, `log-store-unavailable` | 503 `log-store-unavailable` |
//!
//! A job that has not ended is never refused `no-live-logs`: its record does not say whether
//! its start carried a sink, so its pages are empty until it writes or ends.
//!
//! The job's row is read first, as `GetJob` reads it, so an unknown or swept job is
//! `unknown-job` and one past its retention end is `expired`, whatever the store still holds.
//! A store failure is never answered with the store's words: the answer is the code above,
//! and the log line says only whether the store refused the read key (an `error`: the
//! operator's configuration) or could not be reached (a `warn`). The call is idempotent, so a
//! client retries it.
//!
//! # Attempts
//!
//! The request's `attempt` names the attempt to read, from 1; absent, the job's current
//! attempt ([`Attempts`]), chosen once per call. Where it is read from (`locate`):
//!
//! - **The current attempt** is read from the job's own record, exactly as a job is read
//!   when it has one attempt: its host, placement time, state and result.
//! - **An ended attempt** (an earlier one, and the current one once the record no longer holds
//!   its host: the job waits for its next attempt, or ended while it waited) is read from the
//!   host and times the record lists for it, or from its attempt row
//!   (`js_jobs::JobStore::attempt_row`) once the record's list has dropped it. It is read as
//!   a job that ended without a result: its end is where its stored bytes end, objects older
//!   than its placement (less the reuse margin) are not its own, and no follow waits.
//!
//! Each attempt's prefix comes from its host and number (`crate::logs::attempt`), so an
//! attempt reads only its own bytes; one the log prefix leaves no room for had no sink and is
//! `no-live-logs`, as its start carried none.
//!
//! # A page
//!
//! A queued job, on its current attempt, answers an empty page at the asked offset, not at the end, without reading
//! the store. Otherwise the page is what [`page::read`] finds: the gap from the asked offset
//! to the page's offset, then at most the page's size of contiguous bytes, exactly as stored,
//! with the first-byte time of the chunk they begin in; `eof` once the job has ended and the
//! page reaches the stream's length. The page's size is `max_bytes`: [`DEFAULT_PAGE_BYTES`]
//! when absent, cut to [`MAX_PAGE_BYTES`].
//!
//! # Follow
//!
//! With `follow`, a page that says nothing new (no gap, no data, not at the end) of the
//! current attempt of a job that has not ended waits: [`FOLLOW_POLL`] at a time on [`Ctx`]'s [`Waiter`], reading the
//! job's row and the store again after each, for at most [`FOLLOW_POLLS`] polls
//! ([`MAX_FOLLOW_WAIT`]); it answers as soon as a page says something or the job ends, and
//! at the cap answers the empty page. A job that has ended, and an ended attempt, answer at
//! once. At most
//! `max_follow` calls wait at once; one more is refused before it waits. A waiting call holds
//! its slot only while it waits: an answer, an error, or the client going away (the call's
//! future dropped) gives it back at once.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use js_jobs::{AttemptRow, JobId, JobRecord, JobState, JobStoreError};
use js_logmint::LogPrefix;
use js_logstore::{Credential, JobPrefix, Store, StoreError, Stream, Transport, UreqTransport};
use js_proto::v1::{self, GetLogsRequest, GetLogsResponse, LogGap};
use js_types::Clock as _;
use tokio::sync::Semaphore;
use tonic::Status;

use super::JobApi;
use super::read::{self, view};
use super::roles::Caller;
use crate::clock::SharedClock;
use crate::file::{LogsConfig, ObjectStoreLogs};
use crate::job_store::ServerJobs;
use crate::logs::StoreClock;
use crate::logs::attempt::{Attempts, attempt_prefix};
use crate::util::blocking;

pub(crate) mod page;

/// The page size of a request without `max_bytes`: 256 KiB.
pub(crate) const DEFAULT_PAGE_BYTES: u32 = 262_144;

/// The largest page: a larger `max_bytes` is cut to it, not refused.
pub(crate) const MAX_PAGE_BYTES: u32 = 1_048_576;

/// How long one follow waits between two reads of the store.
pub(crate) const FOLLOW_POLL: Duration = Duration::from_secs(1);

/// The longest one follow waits.
pub(crate) const MAX_FOLLOW_WAIT: Duration = Duration::from_secs(20);

/// How many times one follow waits [`FOLLOW_POLL`] before it answers an empty page:
/// [`MAX_FOLLOW_WAIT`] in all.
pub(crate) const FOLLOW_POLLS: u32 = (MAX_FOLLOW_WAIT.as_millis() / FOLLOW_POLL.as_millis()) as u32;

/// The longest one request to the store may take, connect to answer; a longer one is
/// `log-store-unavailable`.
pub(crate) const STORE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the read key's expiry is set to: an operator's long-lived key has none.
const NEVER: Duration = Duration::from_secs(1 << 40);

/// Waits a while: the one way a follow lets time pass, so a test drives it without sleeping.
pub(crate) trait Waiter: Send + Sync {
    /// A future that completes after `period`.
    fn wait(&self, period: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// The production waiter: the runtime's timer.
struct Sleep;

impl Waiter for Sleep {
    fn wait(&self, period: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(period))
    }
}

/// Where a server with an object-store sink reads logs: the store, and the log prefix the
/// hosts' prefixes are built under.
struct Source {
    store: page::LogStore,
    /// The configured log prefix, with its trailing `/` (empty for none).
    prefix: LogPrefix,
}

/// How the reader learns a job's attempts from its record: [`Attempts::of`].
pub(crate) type AttemptsOf = fn(&JobRecord) -> Attempts;

/// How the reader reads the row of an ended attempt its record no longer lists:
/// [`stored_row`].
pub(crate) type RowOf = fn(&ServerJobs, &JobId, u32) -> Result<Option<AttemptRow>, JobStoreError>;

/// The state of the logs group: the store (none with `sink = "none"`), the waiter of follows,
/// the slots of waiting follows, and how a job's attempts are read.
#[derive(Clone)]
pub(crate) struct Ctx {
    source: Option<Arc<Source>>,
    waiter: Arc<dyn Waiter>,
    follows: Arc<Semaphore>,
    attempts: AttemptsOf,
    rows: RowOf,
}

impl Default for Ctx {
    /// No sink: every request with a stream answers `no-log-sink`.
    fn default() -> Ctx {
        Ctx {
            source: None,
            waiter: Arc::new(Sleep),
            follows: Arc::new(Semaphore::new(0)),
            attempts: Attempts::of,
            rows: stored_row,
        }
    }
}

impl fmt::Debug for Ctx {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("Ctx")
            .field("sink", &self.source.is_some())
            .field("free_follows", &self.follows.available_permits())
            .finish_non_exhaustive()
    }
}

impl Ctx {
    /// The context of the `[logs]` table `logs`: none for `sink = "none"`; for an object store,
    /// its store over the production transport, signing with its read key at `clock`.
    pub(crate) fn from_config(logs: &LogsConfig, clock: &SharedClock) -> Ctx {
        let LogsConfig::ObjectStore(sink) = logs else {
            return Ctx::default();
        };
        let key = &sink.read_key;
        let credential = Credential {
            access_key_id: key.access_key_id().to_owned(),
            secret_access_key: key.secret_access_key().to_owned(),
            session_token: None,
            expires_at: UNIX_EPOCH + NEVER,
        };
        let transport = Arc::new(UreqTransport::new(STORE_TIMEOUT));
        Ctx::over(transport, sink, credential, clock)
    }

    /// The context of the object-store sink `sink`, reading with `credential` over
    /// `transport` at `clock`, and allowing the sink's `max_follow` waiting follows.
    pub(crate) fn over(
        transport: Arc<dyn Transport>,
        sink: &ObjectStoreLogs,
        credential: Credential,
        clock: &SharedClock,
    ) -> Ctx {
        let store = Store::new(
            page::Port(transport),
            StoreClock(clock.clone()),
            sink.endpoint.clone(),
            credential,
        );
        let slots = usize::try_from(sink.max_follow).unwrap_or(usize::MAX);
        Ctx {
            source: Some(Arc::new(Source {
                store,
                prefix: sink.prefix.clone(),
            })),
            follows: Arc::new(Semaphore::new(slots.min(Semaphore::MAX_PERMITS))),
            ..Ctx::default()
        }
    }

    /// The same context, its follows waiting on `waiter`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_waiter(self, waiter: Arc<dyn Waiter>) -> Ctx {
        Ctx { waiter, ..self }
    }

    /// The same context, learning a job's attempts from `attempts` and reading an attempt's
    /// row with `rows`: a job of several attempts, which this release's record cannot hold.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_attempts(self, attempts: AttemptsOf, rows: RowOf) -> Ctx {
        Ctx {
            attempts,
            rows,
            ..self
        }
    }
}

/// The row of attempt `attempt` of the job `id`, as the job tables hold it.
pub(crate) fn stored_row(
    jobs: &ServerJobs,
    id: &JobId,
    attempt: u32,
) -> Result<Option<AttemptRow>, JobStoreError> {
    jobs.attempt_row(id, attempt)
}

/// What one request reads: the attempt, the stream, the offset of its first byte and the
/// page's size.
#[derive(Debug, Clone, Copy)]
struct Want {
    attempt: u32,
    stream: Stream,
    from: u64,
    max: u32,
}

/// Where an attempt's bytes are read from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Located {
    /// The job's own record: the attempt is the current one and the record still holds its
    /// host, times and result, or it has not been placed.
    Current,
    /// The host and times of an attempt that ended, from the record's list or the attempt's
    /// row.
    Ended(AttemptRow),
}

/// One page, and whether a follow may wait for more after it: only of the current attempt of
/// a job that has not ended.
struct Read {
    page: page::Page,
    live: bool,
}

impl JobApi {
    /// The service with `logs` as the logs group's context; every other part is kept.
    #[must_use]
    pub(crate) fn with_logs(self, logs: Ctx) -> JobApi {
        JobApi { logs, ..self }
    }
}

/// The stream `request` names; `None` for the call without one (also for a value this
/// version does not know, which reads as unspecified).
pub(super) fn stream_of(request: &GetLogsRequest) -> Option<Stream> {
    match request.stream() {
        v1::LogStream::Stdout => Some(Stream::Stdout),
        v1::LogStream::Stderr => Some(Stream::Stderr),
        v1::LogStream::Unspecified => None,
    }
}

/// `GetLogs` of `stream` for the job `job`, whose row the call read as `caller` may read it:
/// one page of the attempt the request asks for (the current one when it asks for none),
/// waiting for new bytes when the request follows. The attempt is chosen once, so a follow
/// keeps reading it when a later attempt begins meanwhile.
pub(super) async fn get_logs(
    api: &JobApi,
    caller: &Caller,
    job: JobRecord,
    request: &GetLogsRequest,
    stream: Stream,
) -> Result<GetLogsResponse, Status> {
    let Some(source) = api.logs.source.clone() else {
        return Err(no_log_sink());
    };
    let max = page_bytes(request.max_bytes)?;
    let attempt = attempt_asked((api.logs.attempts)(&job).current, request.attempt)?;
    let want = Want {
        attempt,
        stream,
        from: request.from_offset.unwrap_or(0),
        max,
    };
    let mut job = job;
    let mut read = read_attempt(api, &source, &job, want).await?;
    let waits = request.follow.unwrap_or(false) && read.live;
    if !waits || !read.page.is_empty_after(want.from) {
        return Ok(answer(&job, want.from, read.page));
    }
    let Ok(_slot) = api.logs.follows.try_acquire() else {
        return Err(too_many_follows());
    };
    for _ in 0..FOLLOW_POLLS {
        api.logs.waiter.wait(FOLLOW_POLL).await;
        let row = read::Reads(api).job(&request.job_id, caller).await?;
        job = read::unexpired(api, row).await?;
        read = read_attempt(api, &source, &job, want).await?;
        if !read.live || !read.page.is_empty_after(want.from) {
            break;
        }
    }
    Ok(answer(&job, want.from, read.page))
}

/// The attempt a request for `asked` reads of a job whose current attempt is `current`:
/// `current` when it asks for none; 0 is an invalid argument, and one above `current` is out
/// of range.
fn attempt_asked(current: u32, asked: Option<u32>) -> Result<u32, Status> {
    match asked {
        None => Ok(current),
        Some(0) => Err(Status::invalid_argument("field: attempt")),
        Some(asked) if asked > current => Err(Status::out_of_range("field: attempt")),
        Some(asked) => Ok(asked),
    }
}

/// One page of what `want` asks of `job`.
async fn read_attempt(
    api: &JobApi,
    source: &Arc<Source>,
    job: &JobRecord,
    want: Want,
) -> Result<Read, Status> {
    JobPrefix::new("", job.id.as_str()).map_err(|_| no_live_logs())?;
    let attempts = (api.logs.attempts)(job);
    match locate(api, job, &attempts, want.attempt).await? {
        Located::Current => current(api, source, job, want).await,
        Located::Ended(row) => ended_attempt(api, source, job, &row, want).await,
    }
}

/// Where attempt `attempt` of `job`, whose attempts are `attempts`, is read from: the job's
/// own record for the current attempt, unless it has ended and the record no longer holds
/// its host (the job waits for its next attempt, or ended while it waited); else the ended
/// attempt the record lists, else the attempt's row. An attempt below the current one that
/// has no row has no logs the server can find: `no-live-logs`.
async fn locate(
    api: &JobApi,
    job: &JobRecord,
    attempts: &Attempts,
    attempt: u32,
) -> Result<Located, Status> {
    let listed = attempts.ended.get(&attempt);
    if attempt == attempts.current && (listed.is_none() || job.host.is_some()) {
        return Ok(Located::Current);
    }
    if let Some(row) = listed {
        return Ok(Located::Ended(row.clone()));
    }
    let rows = api.logs.rows;
    let id = job.id.clone();
    let row = api.on_tables(move |jobs| rows(jobs, &id, attempt)).await;
    match row {
        Ok(row) => row.map(Located::Ended).ok_or_else(no_live_logs),
        Err(JobStoreError::NotFound { .. }) => Err(read::unknown_job()),
        Err(error) => Err(read::read_failed(&error)),
    }
}

/// One page of the current attempt of `job`, read as a job is read from its own record.
async fn current(
    api: &JobApi,
    source: &Arc<Source>,
    job: &JobRecord,
    want: Want,
) -> Result<Read, Status> {
    let live = !job.state.is_ended();
    if matches!(job.state, JobState::Queued) {
        return Ok(Read {
            page: page::Page {
                offset: want.from,
                ..page::Page::default()
            },
            live,
        });
    }
    let end = match job.state.is_ended() {
        true => Some(ended(job, want.stream)?),
        false => None,
    };
    if end
        .and_then(|end| end.length)
        .is_some_and(|length| want.from > length)
    {
        return Err(past_the_end());
    }
    let Some(host) = job.host.as_ref() else {
        // Never placed (cancelled while queued): it printed nothing, and it has ended.
        return Ok(Read {
            page: page::Page {
                offset: want.from,
                eof: true,
                ..page::Page::default()
            },
            live,
        });
    };
    let prefix = job_prefix(&source.prefix, host.as_str(), job, want.attempt)?;
    let placed_ms = job.placed_at.map_or(0, view::millis);
    let page = fetch(api, source, prefix, want, placed_ms, end).await?;
    Ok(Read { page, live })
}

/// One page of the ended attempt held as `row`, read as a job that ended without a result is
/// read: its length is where its stored bytes end, and no follow waits for it.
async fn ended_attempt(
    api: &JobApi,
    source: &Arc<Source>,
    job: &JobRecord,
    row: &AttemptRow,
    want: Want,
) -> Result<Read, Status> {
    let prefix = job_prefix(&source.prefix, row.host.as_str(), job, want.attempt)?;
    let end = page::End {
        at_ms: view::millis(row.ended_at),
        length: None,
        kept: None,
    };
    let placed_ms = view::millis(row.placed_at);
    let page = fetch(api, source, prefix, want, placed_ms, Some(end)).await?;
    Ok(Read { page, live: false })
}

/// The page `want` asks for under `prefix`, of an attempt placed at `placed_ms` that ends as
/// `end` says, read from the store on the pool for blocking calls.
async fn fetch(
    api: &JobApi,
    source: &Arc<Source>,
    prefix: JobPrefix,
    want: Want,
    placed_ms: u64,
    end: Option<page::End>,
) -> Result<page::Page, Status> {
    let ask = page::Ask {
        prefix,
        stream: want.stream,
        from: want.from,
        max: u64::from(want.max),
        now_ms: api.clock.now().millis(),
        cutoff_ms: placed_ms.saturating_sub(page::duration_millis(page::REUSE_MARGIN)),
        end,
    };
    let source = Arc::clone(source);
    let read = blocking(move || page::read(&source.store, &ask)).await;
    read.map_err(|error| store_failed(&error))
}

/// The prefix `P` of attempt `attempt` of `job` placed on `host`, under the log prefix
/// `prefix`, as the attempt's start named it to the agent; `no-live-logs` when the layout
/// builds none, as the start then carried no sink.
fn job_prefix(
    prefix: &LogPrefix,
    host: &str,
    job: &JobRecord,
    attempt: u32,
) -> Result<JobPrefix, Status> {
    attempt_prefix(prefix, host, job.id.as_str(), attempt).ok_or_else(no_live_logs)
}

/// How the stream `stream` of the ended job `job` ends: its length and dropped bytes from its
/// result; a length of 0 for a job never placed; no length for one that ended without a
/// result (lost, or failed to start). A result without lengths had no live logs.
fn ended(job: &JobRecord, stream: Stream) -> Result<page::End, Status> {
    let at_ms = job.ended_at.map_or(0, view::millis);
    let lengths = match (&job.result, job.placed_at) {
        (_, None) => Some((0, 0)),
        (Some(result), Some(_)) => {
            let logs = result.logs.ok_or_else(no_live_logs)?;
            Some(match stream {
                Stream::Stdout => (logs.stdout_bytes, logs.stdout_dropped),
                Stream::Stderr => (logs.stderr_bytes, logs.stderr_dropped),
            })
        }
        (None, Some(_)) => None,
    };
    Ok(page::End {
        at_ms,
        length: lengths.map(|(length, _)| length),
        kept: lengths
            .filter(|(_, dropped)| *dropped > 0)
            .map(|(length, dropped)| length.saturating_sub(dropped)),
    })
}

/// The answer of `page`, read from `from`, for `job`: its state, the gap from `from` to the
/// page's offset if there is one, and the page; the tails empty.
fn answer(job: &JobRecord, from: u64, page: page::Page) -> GetLogsResponse {
    let gaps = (page.offset > from)
        .then(|| LogGap {
            offset: from,
            length: page.offset - from,
        })
        .into_iter()
        .collect();
    GetLogsResponse {
        state: view::state(job.state).into(),
        gaps,
        next_offset: page.next_offset(),
        offset: page.offset,
        data: page.data,
        eof: page.eof,
        first_byte_at_ms: page.first_byte_at_ms,
        ..GetLogsResponse::default()
    }
}

/// The page size a request for `asked` bytes gets.
fn page_bytes(asked: Option<u32>) -> Result<u32, Status> {
    match asked {
        None => Ok(DEFAULT_PAGE_BYTES),
        Some(0) => Err(Status::invalid_argument("field: maxBytes")),
        Some(asked) => Ok(asked.min(MAX_PAGE_BYTES)),
    }
}

/// The answer of a server with no sink: `no-log-sink`.
fn no_log_sink() -> Status {
    Status::failed_precondition("no-log-sink")
}

/// The answer of a job with no live logs: `no-live-logs`.
fn no_live_logs() -> Status {
    Status::failed_precondition("no-live-logs")
}

/// The answer of an offset past the end of an ended job's stream.
fn past_the_end() -> Status {
    Status::out_of_range("field: fromOffset")
}

/// The answer of a follow over the bound: `too-many-follows`.
fn too_many_follows() -> Status {
    Status::resource_exhausted("too-many-follows")
}

/// The answer of a store request that failed: `log-store-unavailable`, logged by its kind
/// alone (never a key, a URL or the store's words).
fn store_failed(error: &StoreError) -> Status {
    let refused = matches!(
        error,
        StoreError::Denied { .. } | StoreError::Expired { .. } | StoreError::NotFound { .. }
    );
    match refused {
        true => tracing::error!(
            "the log store refused the read key: check logs.read_key_file and logs.url"
        ),
        false => tracing::warn!("the log store could not be read"),
    }
    Status::unavailable("log-store-unavailable")
}
