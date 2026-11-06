//! The rig of the logs group's tests (`logs_tests.rs`, `logs_follow_tests.rs`): the job
//! service with an object-store sink over js-logstore's fake store, its objects written by
//! js-logstore's own client and layout, the store's clock and the service's one manual clock,
//! and follows that wait on [`Steps`], which a test releases, so nothing sleeps.
//!
//! Every request the reader sends goes through [`Tap`], which can run a test's hook just
//! before a request whose URL holds a given text (the agent deleting a chunk after the reader
//! listed it), refuse a request at the transport, or answer one with a store's error.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_jobs::{JobEnd, JobId, LogLengths, Output};
use js_logmint::{Bucket, LogPrefix, RoleArn, StsEndpoint, host_prefix};
use js_logstore::fake::{Action, FakeStore, Op, Policy};
use js_logstore::layout::{FIRST_BYTE_AT_MS, encode_gap};
use js_logstore::{
    Endpoint, HttpRequest, HttpResponse, JobPrefix, Metadata, Store, Stream, Transport,
    TransportError,
};
use js_proto::v1::{self, GetLogsRequest, GetLogsResponse};
use tokio::sync::{Semaphore, watch};
use tonic::Status;

use super::JobApi;
use super::logs::{self, Waiter};
use super::read_tests::{self, clock, over_fake, service_clock};
use crate::file::{AccessKey, Minting, ObjectStoreLogs};
use crate::job_store::ServerJobs;
use crate::logs::StoreClock;

/// The log store's URL.
pub(super) const ENDPOINT: &str = "https://objects.example";

/// The log store's region.
const REGION: &str = "us-east-1";

/// The log store's bucket.
const BUCKET: &str = "logs";

/// The configured log prefix: every job's objects are under `js/<job>/`.
pub(super) const PREFIX: &str = "js";

/// A hook a test runs before one request.
type Hook = Box<dyn FnOnce() + Send>;

/// How [`Tap`] fails a request.
#[derive(Debug, Clone, Copy)]
pub(super) enum Failure {
    /// No answer: the connection was refused.
    Refused,
    /// The store answers this status with this S3 error code.
    Answer(u16, &'static str),
}

/// What [`Tap`] does to the requests to come.
#[derive(Default)]
struct Script {
    /// A hook, and the text of the URL whose request runs it first.
    before: Option<(String, Hook)>,
    /// How many requests pass before one fails, and how.
    fail: Option<(usize, Failure)>,
}

/// The reader's transport: the fake store, with a test's hook and failures in front.
pub(super) struct Tap {
    store: FakeStore,
    script: Mutex<Script>,
}

impl Tap {
    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `hook` just before the first later request whose URL holds `text`.
    pub(super) fn before(&self, text: &str, hook: impl FnOnce() + Send + 'static) {
        self.script().before = Some((text.to_owned(), Box::new(hook)));
    }

    /// Lets `passing` requests through, then fails the next one as `failure` says.
    pub(super) fn fail_after(&self, passing: usize, failure: Failure) {
        self.script().fail = Some((passing, failure));
    }
}

impl Transport for Tap {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let (hook, failure) = {
            let mut script = self.script();
            let due = script
                .before
                .as_ref()
                .is_some_and(|(text, _)| request.url.contains(text.as_str()));
            let hook = due.then(|| script.before.take()).flatten();
            let failure = match script.fail.take() {
                Some((0, failure)) => Some(failure),
                Some((passing, failure)) => {
                    script.fail = Some((passing - 1, failure));
                    None
                }
                None => None,
            };
            (hook, failure)
        };
        if let Some((_, hook)) = hook {
            hook();
        }
        match failure {
            Some(Failure::Refused) => {
                Err(TransportError::Io(std::io::ErrorKind::ConnectionRefused))
            }
            Some(Failure::Answer(status, code)) => Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body: format!(
                    "<Error><Code>{code}</Code><Message>s3://logs/js/x secret</Message></Error>"
                )
                .into_bytes(),
            }),
            None => self.store.send(request),
        }
    }
}

/// The follows' waiter: each wait is held until the test releases it ([`Steps::release`]);
/// how many wait now, how many began and how long they waited in all are kept.
#[derive(Clone)]
pub(super) struct Steps(Arc<Counts>);

/// What [`Steps`] keeps.
struct Counts {
    waiting: watch::Sender<usize>,
    begun: AtomicUsize,
    waited_ms: AtomicU64,
    released: Semaphore,
}

/// Counts one wait out of [`Counts::waiting`] when it ends or is dropped.
struct Leaving(Arc<Counts>);

impl Drop for Leaving {
    fn drop(&mut self) {
        self.0.waiting.send_modify(|waiting| *waiting -= 1);
    }
}

impl Steps {
    fn new() -> Steps {
        Steps(Arc::new(Counts {
            waiting: watch::channel(0).0,
            begun: AtomicUsize::new(0),
            waited_ms: AtomicU64::new(0),
            released: Semaphore::new(0),
        }))
    }

    /// Waits until exactly `n` follows are waiting.
    pub(super) async fn until_waiting(&self, n: usize) {
        let mut waiting = self.0.waiting.subscribe();
        waiting
            .wait_for(|waiting| *waiting == n)
            .await
            .expect("the waiter is kept");
    }

    /// Ends `n` waits, now or as they begin.
    pub(super) fn release(&self, n: usize) {
        self.0.released.add_permits(n);
    }

    /// How many waits began.
    pub(super) fn begun(&self) -> usize {
        self.0.begun.load(Ordering::SeqCst)
    }

    /// How long the ended waits waited in all.
    pub(super) fn waited(&self) -> Duration {
        Duration::from_millis(self.0.waited_ms.load(Ordering::SeqCst))
    }
}

impl Waiter for Steps {
    fn wait(&self, period: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let counts = Arc::clone(&self.0);
        Box::pin(async move {
            counts.begun.fetch_add(1, Ordering::SeqCst);
            counts.waiting.send_modify(|waiting| *waiting += 1);
            let _leaving = Leaving(Arc::clone(&counts));
            counts
                .released
                .acquire()
                .await
                .expect("the waiter is open")
                .forget();
            let ms = u64::try_from(period.as_millis()).expect("a period fits");
            counts.waited_ms.fetch_add(ms, Ordering::SeqCst);
        })
    }
}

/// The log store's endpoint.
fn endpoint() -> Endpoint {
    Endpoint::parse(ENDPOINT, REGION, BUCKET, true).expect("an endpoint")
}

/// The `[logs]` table of an object-store sink allowing `max_follow` waiting follows.
pub(super) fn sink(max_follow: u64) -> ObjectStoreLogs {
    let key = br#"{"access_key_id": "READKEY", "secret_access_key": "read-secret"}"#;
    let key = AccessKey::parse(key).expect("a key");
    ObjectStoreLogs {
        bucket: Bucket::new(BUCKET).expect("a bucket"),
        prefix: LogPrefix::new(PREFIX).expect("a prefix"),
        endpoint: endpoint(),
        read_key: key.clone(),
        minting: Minting::Sts {
            endpoint: StsEndpoint::parse(ENDPOINT, REGION).expect("an STS endpoint"),
            role_arn: RoleArn::new("arn:aws:iam::example:role/js-logs").expect("a role"),
            key,
        },
        credential_ttl: Duration::from_secs(3600),
        max_follow,
    }
}

/// The job service with an object-store sink over the fake store, and its two transports.
pub(super) struct Rig {
    /// The calls over gRPC and the gateway, the job tables and the clock.
    pub(super) calls: read_tests::Rig,
    /// The bucket.
    pub(super) store: FakeStore,
    /// The reader's transport.
    pub(super) tap: Arc<Tap>,
    /// The follows' waiter.
    pub(super) steps: Steps,
    /// The client the test writes objects with, as the agent does.
    pub(super) writer: Arc<Store<FakeStore, StoreClock>>,
}

impl Rig {
    /// The rig whose sink allows 256 waiting follows.
    pub(super) async fn new() -> Rig {
        Rig::following(256).await
    }

    /// The rig whose sink allows `max_follow` waiting follows.
    pub(super) async fn following(max_follow: u64) -> Rig {
        let clock = clock();
        let jobs = over_fake(&clock);
        let shared = service_clock(&clock);
        let store = FakeStore::new(endpoint(), Arc::new(StoreClock(shared.clone())));
        let read = store.issue(Policy {
            actions: vec![Action::Get, Action::List],
            prefix: format!("{PREFIX}/"),
        });
        let write = store.issue(Policy {
            actions: vec![Action::Put, Action::Delete],
            prefix: String::new(),
        });
        let writer = Store::new(store.clone(), StoreClock(shared.clone()), endpoint(), write);
        let tap = Arc::new(Tap {
            store: store.clone(),
            script: Mutex::new(Script::default()),
        });
        let steps = Steps::new();
        let transport: Arc<dyn Transport> = tap.clone();
        let ctx = logs::Ctx::over(transport, &sink(max_follow), read, &shared)
            .with_waiter(Arc::new(steps.clone()));
        let api = JobApi::new(Arc::clone(&jobs), shared).with_logs(ctx);
        let calls = read_tests::Rig::serving(Arc::new(api), jobs, clock).await;
        Rig {
            calls,
            store,
            tap,
            steps,
            writer: Arc::new(writer),
        }
    }

    /// The job tables.
    pub(super) fn jobs(&self) -> &ServerJobs {
        &self.calls.jobs
    }

    /// Moves the clocks forward by `by`.
    pub(super) fn advance(&self, by: Duration) {
        self.calls.advance(by);
    }

    /// Puts the chunk of `stream` of `job` at `offset`, with `first_ms` as its first-byte
    /// time.
    pub(super) fn chunk(
        &self,
        job: &str,
        stream: Stream,
        offset: u64,
        bytes: &[u8],
        first_ms: u64,
    ) {
        let metadata = Metadata::from([(FIRST_BYTE_AT_MS.to_owned(), first_ms.to_string())]);
        let key = prefix(job).chunk_key(stream, offset);
        self.writer
            .put(&key, bytes.to_vec(), &metadata)
            .expect("the chunk is put");
    }

    /// Puts the gap marker of `stream` of `job` at `offset`, `length` bytes long.
    pub(super) fn marker(&self, job: &str, stream: Stream, offset: u64, length: u64) {
        let gap = js_logstore::Gap::new(offset, length).expect("a gap");
        self.put(&prefix(job).gap_key(stream, offset), &encode_gap(&gap));
    }

    /// Puts the final file of `stream` of `job`.
    pub(super) fn final_file(&self, job: &str, stream: Stream, bytes: &[u8]) {
        self.put(&prefix(job).final_key(stream), bytes);
    }

    /// Puts `bytes` at `key`, with no metadata.
    pub(super) fn put(&self, key: &str, bytes: &[u8]) {
        self.writer
            .put(key, bytes.to_vec(), &Metadata::new())
            .expect("the object is put");
    }

    /// Every key the reader read and every prefix it listed, in order.
    pub(super) fn asked(&self) -> Vec<String> {
        self.store
            .operations()
            .into_iter()
            .filter_map(|op| match op {
                Op::Get { key } => Some(key),
                Op::List { prefix } => Some(prefix),
                Op::Put { .. } | Op::Delete { .. } => None,
            })
            .collect()
    }

    /// `GetLogs` of `request` over both transports, which must agree.
    pub(super) async fn read(&self, request: GetLogsRequest) -> Result<GetLogsResponse, Status> {
        let uri = uri(&request);
        self.calls.read("GetLogs", request, &uri).await
    }

    /// `GetLogs` of `request` over gRPC alone.
    pub(super) async fn grpc(&self, request: GetLogsRequest) -> Result<GetLogsResponse, Status> {
        self.calls.grpc("GetLogs", request).await
    }
}

/// The host every test job is placed on (`read_tests::place`).
pub(super) const HOST: &str = "host-a";

/// The job prefix of `job` on [`HOST`]: the job under the host's prefix under [`PREFIX`], as
/// the job's start names it.
pub(super) fn prefix(job: &str) -> JobPrefix {
    let logs = LogPrefix::new(PREFIX).expect("a log prefix");
    let base = host_prefix(&logs, HOST).expect("a host prefix");
    JobPrefix::new(base.as_str().trim_end_matches('/'), job).expect("a job prefix")
}

/// The request for `stream` of `job` from `from`, with no page size and no follow.
pub(super) fn ask(job: &str, stream: v1::LogStream, from: u64) -> GetLogsRequest {
    GetLogsRequest {
        job_id: job.to_owned(),
        stream: stream.into(),
        from_offset: Some(from),
        ..GetLogsRequest::default()
    }
}

/// The gateway's URI of `request`: its fields as query parameters, by their JSON names.
pub(super) fn uri(request: &GetLogsRequest) -> String {
    let mut pairs = vec![format!("stream={}", request.stream().as_str_name())];
    pairs.extend(request.from_offset.map(|from| format!("fromOffset={from}")));
    pairs.extend(request.max_bytes.map(|max| format!("maxBytes={max}")));
    pairs.extend(request.follow.map(|follow| format!("follow={follow}")));
    format!("/v1/jobs/{}/logs?{}", request.job_id, pairs.join("&"))
}

/// Ends the running job `text` with exit 0, its result reporting `stdout` and `stderr` as
/// (length, dropped) of each stream.
pub(super) fn end(jobs: &ServerJobs, text: &str, stdout: (u64, u64), stderr: (u64, u64)) {
    let logs = LogLengths {
        stdout_bytes: stdout.0,
        stderr_bytes: stderr.0,
        stdout_dropped: stdout.1,
        stderr_dropped: stderr.1,
    };
    let output = Output {
        logs: Some(logs),
        ..Output::default()
    };
    let end = JobEnd::Exited {
        exit_code: 0,
        output,
    };
    jobs.end_job(&read_tests::id(text), end)
        .expect("the job ends");
}

/// Ends the running job `id` as `end` says.
pub(super) fn end_as(jobs: &ServerJobs, id: &JobId, end: JobEnd) {
    jobs.end_job(id, end).expect("the job ends");
}

/// `n` bytes of a pattern that repeats only every 251 bytes and holds every byte value.
pub(super) fn pattern(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| u8::try_from((i * 7 + i / 251) % 256).expect("a byte"))
        .collect()
}
