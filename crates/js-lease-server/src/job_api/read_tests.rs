//! The read calls (`read.rs`) over both transports, in one process: gRPC through a tonic
//! client over an in-memory duplex stream, the gateway through its router's `oneshot`. Jobs
//! are put in their states by calling the job tables directly. The store's clock is manual and
//! the service reads the same clock, so a test moves time for both at once; nothing sleeps.
//!
//! Every answer a test reads goes through [`Rig::read`], which asks both transports and
//! asserts that they agree: the gateway's JSON decodes, by the proto3 JSON mapping, to the
//! very message gRPC answered, and a refusal is the gateway's row for the gRPC status.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request as HttpRequest, StatusCode, header};
use js_api_gateway::Answer;
use js_jobs::interfere::Interfere;
use js_jobs::{
    Booked, Constraints, EndReason, JobEnd, JobId, JobSpec, JobState, JobStore, Needs,
    NetworkPolicy, NewJob, Output, RandomIds, ResourceUsage,
};
use js_lease::Timestamp;
use js_proto::v1::{
    self, GetJobRequest, GetJobResponse, GetLogsRequest, GetLogsResponse, GetResultRequest,
    GetResultResponse, ListJobsRequest, ListJobsResponse,
};
use js_state::fake::{Fake, Flaw};
use js_state::recording::RecordingStore;
use js_state::sqlite::{Options, SqliteStore};
use js_state::{ManualClock, Store, StoreTime};
use js_testkit::dir::TestDir;
use js_types::{HostId, ImageDigestRef, Labels, Resources};
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{Value, json};
use tonic::client::Grpc;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};
use tonic_prost::ProstCodec;
use tonic_prost::prost::Message;
use tower::ServiceExt;

use super::tests::{SERVICE, TOKEN, connect, scratch, token};
use super::{JobApi, gateway, grpc, read};
use crate::adapt::SharedClock;
use crate::job_store::ServerJobs;

/// The time the clocks start at, in milliseconds since the Unix epoch.
pub(super) const START_MS: u64 = 1_800_000_000_000;

/// A value above 2^53, which a JSON number would round: the gateway writes it as a string.
pub(super) const BIG: u64 = (1 << 53) + 1;

/// The service's view of the store's clock: the same reading, in milliseconds.
struct ServiceClock(Arc<ManualClock>);

impl js_lease::Clock for ServiceClock {
    fn now(&self) -> Timestamp {
        let since = js_state::Clock::now(&*self.0).as_duration();
        Timestamp::from_millis(u64::try_from(since.as_millis()).expect("the time fits"))
    }
}

/// A store clock at [`START_MS`].
pub(super) fn clock() -> Arc<ManualClock> {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    clock
}

/// Job tables over `store`, as the server holds them.
pub(super) fn tables(store: Arc<dyn Store>) -> Arc<ServerJobs> {
    Arc::new(JobStore::new(Arc::new(store), RandomIds).expect("the job tables open"))
}

/// Job tables over the in-memory fake store on `clock`.
pub(super) fn over_fake(clock: &Arc<ManualClock>) -> Arc<ServerJobs> {
    tables(Arc::new(Fake::with_flaw(Arc::clone(clock), Flaw::None)))
}

/// Job tables over the SQLite file of `dir`, on `clock`. Opening it again reopens the same
/// tables.
pub(super) fn over_sqlite(dir: &TestDir, clock: &Arc<ManualClock>) -> Arc<ServerJobs> {
    let clock: Arc<dyn js_state::Clock> = clock.clone();
    let path = dir.path().join("state.db");
    let store = SqliteStore::open(&path, clock, Options::default()).expect("the state file opens");
    tables(Arc::new(store))
}

/// The job service over a set of tables and its two transports.
pub(super) struct Rig {
    /// The store's clock, which the service reads too.
    pub(super) clock: Arc<ManualClock>,
    /// The job tables the service reads.
    pub(super) jobs: Arc<ServerJobs>,
    channel: Channel,
    router: Router,
}

impl Rig {
    /// The service over `jobs` with the read context `ctx`, reading `clock`.
    pub(super) async fn with(
        jobs: Arc<ServerJobs>,
        clock: Arc<ManualClock>,
        ctx: read::Ctx,
    ) -> Rig {
        let shared = SharedClock::new(ServiceClock(Arc::clone(&clock)));
        let api = Arc::new(JobApi::new(Arc::clone(&jobs), shared).with_read(ctx));
        let channel = connect(grpc(Arc::clone(&api), token())).await;
        let router = gateway(api, token());
        Rig {
            clock,
            jobs,
            channel,
            router,
        }
    }

    /// The service over `jobs` with the default read context.
    pub(super) async fn over(jobs: Arc<ServerJobs>, clock: Arc<ManualClock>) -> Rig {
        Rig::with(jobs, clock, read::Ctx::default()).await
    }

    /// Moves the clock of the store and the service forward by `by`.
    pub(super) fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// The answer of `method` to `request` over gRPC, having asserted that the gateway's
    /// answer to `GET uri` is the same: the response, decoded from its JSON, or the gateway's
    /// row of the gRPC status.
    pub(super) async fn read<Req, Resp>(
        &self,
        method: &str,
        request: Req,
        uri: &str,
    ) -> Result<Resp, Status>
    where
        Req: Message + Send + Sync + 'static,
        Resp: Message + Default + PartialEq + Debug + Send + Sync + 'static,
    {
        let answer = self.grpc::<Req, Resp>(method, request).await;
        let (status, body) = self.get(uri).await;
        match &answer {
            Ok(response) => {
                assert_eq!(status, StatusCode::OK, "{uri}");
                let name = format!("jobscheduler.v1.{method}Response");
                assert_eq!(&from_json::<Resp>(&name, &body), response, "{uri}");
            }
            Err(refused) => {
                let row = Answer::of_status(refused);
                assert_eq!(
                    (status, json_of(&body)),
                    (row.status(), row.body()),
                    "{uri}"
                );
            }
        }
        answer
    }

    /// The answer of `method` to `request` over gRPC alone, with the token.
    pub(super) async fn grpc<Req, Resp>(&self, method: &str, request: Req) -> Result<Resp, Status>
    where
        Req: Message + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
    {
        let mut grpc = Grpc::new(self.channel.clone());
        grpc.ready().await.expect("the channel is ready");
        let path = PathAndQuery::try_from(format!("/{SERVICE}/{method}")).expect("a path");
        let mut request = Request::new(request);
        let bearer = MetadataValue::try_from(format!("Bearer {TOKEN}")).expect("ASCII");
        request.metadata_mut().insert("authorization", bearer);
        let codec = ProstCodec::<Req, Resp>::default();
        let response = grpc.unary(request, path, codec).await;
        response.map(tonic::Response::into_inner)
    }

    /// The status and body of the gateway's answer to `GET uri`, with the token.
    pub(super) async fn get(&self, uri: &str) -> (StatusCode, Vec<u8>) {
        let request = HttpRequest::builder()
            .method("GET")
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .expect("the request builds");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("answered");
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read");
        (status, body.to_vec())
    }

    /// `GetJob` of `id`.
    pub(super) async fn get_job(&self, id: &str) -> Result<v1::Job, Status> {
        let request = GetJobRequest {
            job_id: id.to_owned(),
        };
        let uri = format!("/v1/jobs/{id}");
        let response: GetJobResponse = self.read("GetJob", request, &uri).await?;
        Ok(response.job.expect("an answer carries the job"))
    }

    /// `GetResult` of `id`.
    pub(super) async fn get_result(&self, id: &str) -> Result<v1::JobResult, Status> {
        let request = GetResultRequest {
            job_id: id.to_owned(),
        };
        let uri = format!("/v1/jobs/{id}/result");
        let response: GetResultResponse = self.read("GetResult", request, &uri).await?;
        Ok(response.result.expect("an answer carries the result"))
    }

    /// `GetLogs` of `id`.
    pub(super) async fn get_logs(&self, id: &str) -> Result<GetLogsResponse, Status> {
        let request = GetLogsRequest {
            job_id: id.to_owned(),
        };
        self.read("GetLogs", request, &format!("/v1/jobs/{id}/logs"))
            .await
    }

    /// `ListJobs` of `request`, whose query string the gateway is sent.
    pub(super) async fn list(&self, request: ListJobsRequest) -> Result<ListJobsResponse, Status> {
        let uri = format!("/v1/jobs?{}", query(&request));
        self.read("ListJobs", request, &uri).await
    }
}

/// The query string of a `ListJobs` request, by the gateway's rules: a repeated field once
/// per value (a state by its name; one this version does not know as unspecified), a map
/// entry as `name[key]=value`, a field at its default left out. Every value here is a name,
/// a number or a page token (a key of the job table), so only the token's `/` is escaped.
fn query(request: &ListJobsRequest) -> String {
    let mut pairs: Vec<String> = Vec::new();
    for state in &request.states {
        let name = v1::JobState::try_from(*state)
            .unwrap_or_default()
            .as_str_name();
        pairs.push(format!("states={name}"));
    }
    for (key, value) in &request.labels {
        pairs.push(format!("labels[{key}]={value}"));
    }
    if let Some(after) = request.queued_after_ms {
        pairs.push(format!("queuedAfterMs={after}"));
    }
    if let Some(before) = request.queued_before_ms {
        pairs.push(format!("queuedBeforeMs={before}"));
    }
    if request.page_size != 0 {
        pairs.push(format!("pageSize={}", request.page_size));
    }
    if !request.page_token.is_empty() {
        pairs.push(format!(
            "pageToken={}",
            request.page_token.replace('/', "%2F")
        ));
    }
    pairs.join("&")
}

/// `body` as JSON.
pub(super) fn json_of(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("the body is JSON")
}

/// `body`, JSON, decoded as the message `name` by the proto3 JSON mapping and converted to
/// `M`.
pub(super) fn from_json<M: Message + Default>(name: &str, body: &[u8]) -> M {
    let pool = DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET).expect("the set decodes");
    let descriptor = pool
        .get_message_by_name(name)
        .expect("the message is defined");
    DynamicMessage::deserialize(descriptor, json_of(body))
        .expect("the body decodes")
        .transcode_to::<M>()
        .expect("the message converts")
}

/// The id `text`.
pub(super) fn id(text: &str) -> JobId {
    JobId::parse(text).expect("a job id")
}

/// The image every job runs.
pub(super) fn image() -> String {
    format!("team/app:v1@sha256:{}", "a".repeat(64))
}

/// A job that runs `run --fast` from [`image`] with 1.5 cores and [`BIG`] bytes of memory,
/// for at most ten minutes, carrying `labels`.
pub(super) fn spec(labels: &[(&str, &str)]) -> JobSpec {
    let labels: BTreeMap<String, String> = labels
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    JobSpec {
        image: ImageDigestRef::parse(&image()).expect("an image"),
        command: vec!["run".to_owned()],
        args: vec!["--fast".to_owned()],
        env: BTreeMap::from([("MODE".to_owned(), "test".to_owned())]),
        needs: Needs::Shape(Resources::new(1500, BIG, 0, None).expect("resources")),
        constraints: Constraints::default(),
        wall_limit_secs: 600,
        network: NetworkPolicy::None,
        ports: Vec::new(),
        files: BTreeMap::new(),
        labels: Labels::from_caller(&labels).expect("labels"),
    }
}

/// What a placement books.
pub(super) const BOOKED: Booked = Booked::Shape {
    cpu_millis: 1500,
    memory_bytes: 1 << 30,
    disk_bytes: 0,
    accelerators: 0,
};

/// Records the job `text` with `spec`, queued now.
pub(super) fn queue(jobs: &ServerJobs, text: &str, spec: JobSpec) -> JobId {
    let new = NewJob {
        id: Some(id(text)),
        spec,
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new)
        .expect("the job is recorded")
        .record
        .id
}

/// Places the queued job `id` on the host `host-a`.
pub(super) fn place(jobs: &ServerJobs, id: &JobId) {
    let host = HostId::new("host-a").expect("a host id");
    jobs.place(id, host, BOOKED).expect("the job is placed");
}

/// Moves the placed job `id` to starting.
pub(super) fn start(jobs: &ServerJobs, id: &JobId) {
    jobs.transition(id, JobState::Placed, JobState::Starting)
        .expect("the job starts");
}

/// Moves the starting job `id` to running.
pub(super) fn run(jobs: &ServerJobs, id: &JobId) {
    jobs.transition(id, JobState::Starting, JobState::Running)
        .expect("the job runs");
}

/// Records the job `text` and takes it to running.
pub(super) fn running(jobs: &ServerJobs, text: &str) -> JobId {
    let id = queue(jobs, text, spec(&[]));
    place(jobs, &id);
    start(jobs, &id);
    run(jobs, &id);
    id
}

/// The standard output of the ended job: `é` and 8191 bytes of `a`, 8193 bytes, so the cut
/// to 8192 falls inside the `é`.
fn stdout() -> String {
    format!("é{}", "a".repeat(8191))
}

/// The standard error of the ended job: 2731 three-byte `日`, 8193 bytes, so the cut to
/// 8192 falls inside the first.
fn stderr() -> String {
    "日".repeat(2731)
}

/// What the ended job used: a CPU time of `BIG` microseconds and a peak of `BIG` bytes.
fn used() -> ResourceUsage {
    ResourceUsage {
        cpu_time: Duration::from_micros(BIG),
        peak_memory_bytes: BIG,
    }
}

/// The end of the job: exit 3 with [`stdout`], [`stderr`] and [`used`].
fn exit_3() -> JobEnd {
    JobEnd::Exited {
        exit_code: 3,
        output: Output {
            stdout: stdout(),
            stderr: stderr(),
            truncated: false,
            usage: Some(used()),
            logs: None,
        },
    }
}

/// The result of a job with `times` that ended with [`exit_3`]: both tails cut at the next
/// character boundary.
fn result_of_exit_3(times: v1::JobTimes) -> v1::JobResult {
    v1::JobResult {
        state: v1::JobState::Failed.into(),
        exit_code: Some(3),
        reason: v1::ResultReason::Exit.into(),
        stdout_tail: "a".repeat(8191).into_bytes(),
        stderr_tail: "日".repeat(2730).into_bytes(),
        truncated: true,
        times: Some(times),
        usage: Some(v1::ResourceUsage {
            cpu_usec: BIG,
            peak_memory_bytes: BIG,
            disk_bytes: None,
            gpus: 0,
        }),
    }
}

/// The times the row of the job `text` holds, in milliseconds since the Unix epoch. The fake
/// store moves its clock on each commit (the cost of a sync), so a test reads the instants
/// the store recorded rather than assume them.
pub(super) fn times_of(jobs: &ServerJobs, text: &str) -> v1::JobTimes {
    let job = jobs.get_job(&id(text)).expect("read").expect("a row");
    let ms = |time: StoreTime| u64::try_from(time.as_duration().as_millis()).expect("fits");
    v1::JobTimes {
        queued_at_ms: ms(job.submitted_at),
        placed_at_ms: job.placed_at.map(ms),
        started_at_ms: job.started_at.map(ms),
        ended_at_ms: job.ended_at.map(ms),
    }
}

/// Whether `ms` falls in the second that begins `s` seconds after [`START_MS`].
fn in_second(ms: Option<u64>, s: u64) -> bool {
    let start = START_MS + s * 1000;
    ms.is_some_and(|ms| (start..start + 1000).contains(&ms))
}

/// The `GetLogs` answer of a job in `state` that has no result: empty tails, not cut.
fn no_tails(state: v1::JobState) -> GetLogsResponse {
    GetLogsResponse {
        state: state.into(),
        ..GetLogsResponse::default()
    }
}

/// A step that moves a job on: placing, starting or running it.
pub(super) type Step = fn(&ServerJobs, &JobId);

/// The (code, message) of a refusal.
fn refusal<T>(answer: Result<T, Status>) -> (Code, String) {
    let status = answer.err().expect("the call is refused");
    (status.code(), status.message().to_owned())
}

/// `unknown-job`, as gRPC answers it.
fn unknown() -> (Code, String) {
    (Code::NotFound, "unknown-job".to_owned())
}

/// Asserts that over the fake store and over SQLite, `GetJob`, `ListJobs`, `GetResult` and
/// `GetLogs` answer the same over gRPC and the gateway (the JSON decoding to the very
/// message), with the job queued, running and ended: the job's state, host and times; the
/// listing that holds it; `not-finished` and then the result with both tails, timing and
/// usage; and the tails. The usage and the memory need carry [`BIG`], which the JSON holds
/// as an exact decimal string. Catches a transport that answers from anything but the one
/// service object, and a mapping that loses or rounds a field.
#[tokio::test]
async fn the_job_is_visible_over_grpc_and_json_identically() {
    let dir = scratch("read_visible");
    for name in ["fake", "sqlite"] {
        let clock = clock();
        let jobs = match name {
            "fake" => over_fake(&clock),
            _ => over_sqlite(&dir, &clock),
        };
        let rig = Rig::over(jobs, clock).await;
        let jobs = &rig.jobs;
        let id = queue(jobs, "visible-1", spec(&[("team", "ml")]));
        let job = rig.get_job("visible-1").await.expect("the job is read");
        assert_eq!(job.state, i32::from(v1::JobState::Queued), "{name}");
        assert_eq!(job.host_id, "", "{name}");
        assert_eq!(
            job.spec
                .as_ref()
                .and_then(|spec| spec.needs.clone())
                .map(|needs| needs.memory_bytes),
            Some(BIG)
        );
        let recorded = times_of(jobs, "visible-1");
        assert_eq!(job.times, Some(recorded), "{name}");
        assert!(in_second(Some(recorded.queued_at_ms), 0), "{recorded:?}");
        let later = (
            recorded.placed_at_ms,
            recorded.started_at_ms,
            recorded.ended_at_ms,
        );
        assert_eq!(later, (None, None, None), "{name}");
        let listed = rig.list(ListJobsRequest::default()).await.expect("a list");
        assert_eq!(
            (listed.jobs, listed.next_page_token),
            (vec![job], String::new())
        );
        let not_finished = (Code::FailedPrecondition, "not-finished".to_owned());
        assert_eq!(refusal(rig.get_result("visible-1").await), not_finished);
        let logs = rig.get_logs("visible-1").await.expect("logs");
        assert_eq!(logs, no_tails(v1::JobState::Queued));

        rig.advance(Duration::from_secs(1));
        place(jobs, &id);
        start(jobs, &id);
        run(jobs, &id);
        let job = rig.get_job("visible-1").await.expect("the job is read");
        assert_eq!(job.state, i32::from(v1::JobState::Running), "{name}");
        assert_eq!(job.host_id, "host-a", "{name}");
        let running = times_of(jobs, "visible-1");
        assert_eq!(job.times, Some(running), "{name}");
        assert_eq!(running.queued_at_ms, recorded.queued_at_ms, "{name}");
        assert!(in_second(running.placed_at_ms, 1), "{running:?}");
        assert!(in_second(running.started_at_ms, 1), "{running:?}");
        assert_eq!(running.ended_at_ms, None, "{name}");
        let listed = rig.list(ListJobsRequest::default()).await.expect("a list");
        assert_eq!(listed.jobs, vec![job]);
        assert_eq!(refusal(rig.get_result("visible-1").await), not_finished);
        let logs = rig.get_logs("visible-1").await.expect("logs");
        assert_eq!(logs, no_tails(v1::JobState::Running));

        rig.advance(Duration::from_secs(1));
        jobs.end_job(&id, exit_3()).expect("the job ends");
        let ended = times_of(jobs, "visible-1");
        let job = rig.get_job("visible-1").await.expect("the job is read");
        assert_eq!(job.state, i32::from(v1::JobState::Failed), "{name}");
        assert_eq!(job.times, Some(ended), "{name}");
        assert!(in_second(ended.ended_at_ms, 2), "{ended:?}");
        let result = rig.get_result("visible-1").await.expect("the result");
        assert_eq!(result, result_of_exit_3(ended), "{name}");
        let logs = rig.get_logs("visible-1").await.expect("logs");
        let tails = GetLogsResponse {
            state: v1::JobState::Failed.into(),
            stdout_tail: result.stdout_tail.clone(),
            stderr_tail: result.stderr_tail.clone(),
            truncated: true,
        };
        assert_eq!(logs, tails, "{name}");
        let (status, body) = rig.get("/v1/jobs/visible-1/result").await;
        assert_eq!(status, StatusCode::OK);
        let usage = &json_of(&body)["result"]["usage"];
        assert_eq!(usage["peakMemoryBytes"], json!(BIG.to_string()), "{name}");
        assert_eq!(usage["cpuUsec"], json!(BIG.to_string()), "{name}");
        let (_, body) = rig.get("/v1/jobs/visible-1").await;
        let needs = &json_of(&body)["job"]["spec"]["needs"];
        assert_eq!(needs["memoryBytes"], json!(BIG.to_string()), "{name}");
    }
}

/// Asserts that a job that exited 3, with a multi-byte character at the cut of each tail,
/// keeps its result in the store, over SQLite: the job is run, ended and its allocation
/// freed through one opening of the file, which is then closed; a service over the file
/// opened again answers `GetResult` with the exit code, the reason, both tails cut at the
/// next character boundary, the timing and the usage at the end instant and at one second
/// before the retention end; at the retention end, before the sweep, it answers `expired`
/// (410), and so does `GetLogs`, while `GetJob` still shows the job; after the sweep,
/// `unknown-job` (404) for all three. Catches a result kept in server memory, one dropped
/// when the allocation is freed, and an off-by-one at the retention end in either
/// direction.
#[tokio::test]
async fn a_result_is_readable_after_the_job_ends() {
    let dir = scratch("read_result");
    let clock = clock();
    let first = over_sqlite(&dir, &clock);
    let id = running(&first, "result-1");
    clock.advance(Duration::from_secs(5));
    first.end_job(&id, exit_3()).expect("the job ends");
    first
        .free(&id, EndReason::Exited)
        .expect("the allocation is freed");
    let freed = first
        .get_allocation(&id)
        .expect("read")
        .map(|allocation| allocation.step);
    assert_eq!(freed, Some(js_jobs::AllocationStep::Freed));
    drop(first);

    let rig = Rig::over(over_sqlite(&dir, &clock), Arc::clone(&clock)).await;
    let ended = START_MS + 5000;
    let times = v1::JobTimes {
        queued_at_ms: START_MS,
        placed_at_ms: Some(START_MS),
        started_at_ms: Some(START_MS),
        ended_at_ms: Some(ended),
    };
    assert_eq!(times_of(&rig.jobs, "result-1"), times);
    let expected = result_of_exit_3(times);
    assert_eq!(rig.get_result("result-1").await.expect("read"), expected);
    let retention = rig.jobs.retention().window();
    rig.advance(retention - Duration::from_secs(1));
    assert_eq!(rig.get_result("result-1").await.expect("read"), expected);

    rig.advance(Duration::from_secs(1));
    let expired = (Code::NotFound, "expired".to_owned());
    assert_eq!(refusal(rig.get_result("result-1").await), expired);
    assert_eq!(refusal(rig.get_logs("result-1").await), expired);
    let (status, _) = rig.get("/v1/jobs/result-1/result").await;
    assert_eq!(status, StatusCode::GONE);
    let job = rig
        .get_job("result-1")
        .await
        .expect("the job is still there");
    assert_eq!(job.state, i32::from(v1::JobState::Failed));

    assert_eq!(rig.jobs.tick().expect("the sweep runs"), vec![id]);
    assert_eq!(refusal(rig.get_result("result-1").await), unknown());
    assert_eq!(refusal(rig.get_logs("result-1").await), unknown());
    assert_eq!(refusal(rig.get_job("result-1").await), unknown());
}

/// Asserts that, over the fake store, for a queued, a placed, a starting and a running job,
/// `GetResult` answers `not-finished` (`FAILED_PRECONDITION`, 409) and `GetLogs` answers the
/// job's state with empty tails, not cut, and no error; and that once the job ends, `GetLogs`
/// answers its result's tails. Catches a result read of a live job that answers anything but
/// `not-finished`, a logs read of one that errors or carries a tail, and logs of an ended
/// job that are not its result's.
#[tokio::test]
async fn get_result_of_a_running_job_is_not_finished() {
    let clock = clock();
    let rig = Rig::over(over_fake(&clock), clock).await;
    let jobs = &rig.jobs;
    let id = queue(jobs, "live-1", spec(&[]));
    let not_finished = (Code::FailedPrecondition, "not-finished".to_owned());
    let steps: [(v1::JobState, Step); 4] = [
        (v1::JobState::Queued, |_, _| {}),
        (v1::JobState::Placed, place),
        (v1::JobState::Starting, start),
        (v1::JobState::Running, run),
    ];
    for (state, step) in steps {
        step(jobs, &id);
        assert_eq!(
            refusal(rig.get_result("live-1").await),
            not_finished,
            "{state:?}"
        );
        let (status, body) = rig.get("/v1/jobs/live-1/result").await;
        assert_eq!(
            (status, json_of(&body)),
            (StatusCode::CONFLICT, json!({"error": "not-finished"}))
        );
        let logs = rig.get_logs("live-1").await.expect("logs of a live job");
        assert_eq!(logs, no_tails(state));
    }
    jobs.end_job(&id, exit_3()).expect("the job ends");
    let result = rig.get_result("live-1").await.expect("the result");
    let logs = rig.get_logs("live-1").await.expect("logs");
    assert_eq!(
        (logs.stdout_tail, logs.stderr_tail, logs.truncated),
        (result.stdout_tail, result.stderr_tail, true)
    );
    assert_eq!(logs.state, i32::from(v1::JobState::Failed));
}

/// Asserts that `GetJob`, `GetResult` and `GetLogs` of an id no job has answer `unknown-job`
/// (`NOT_FOUND`, 404), and so do they for a job swept after its retention window, the same
/// answer as for one that never existed; and that a `job_id` that is not a job id is refused
/// as an argument naming `jobId`, over gRPC and over the gateway. Catches a swept job that
/// reads as something else, and an empty or malformed id read as a job.
#[tokio::test]
async fn get_job_of_an_unknown_id_is_not_found() {
    let clock = clock();
    let rig = Rig::over(over_fake(&clock), clock).await;
    assert_eq!(refusal(rig.get_job("never-1").await), unknown());
    assert_eq!(refusal(rig.get_result("never-1").await), unknown());
    assert_eq!(refusal(rig.get_logs("never-1").await), unknown());
    let (status, body) = rig.get("/v1/jobs/never-1").await;
    assert_eq!(
        (status, json_of(&body)),
        (StatusCode::NOT_FOUND, json!({"error": "unknown-job"}))
    );

    let id = running(&rig.jobs, "swept-1");
    rig.jobs.end_job(&id, exit_3()).expect("the job ends");
    rig.advance(rig.jobs.retention().window());
    assert_eq!(rig.jobs.tick().expect("the sweep runs"), vec![id]);
    assert_eq!(refusal(rig.get_job("swept-1").await), unknown());
    assert_eq!(refusal(rig.get_result("swept-1").await), unknown());
    assert_eq!(refusal(rig.get_logs("swept-1").await), unknown());

    let bad = (Code::InvalidArgument, "field: jobId".to_owned());
    let long = "x".repeat(64);
    for text in ["", "has space", long.as_str()] {
        let request = |job_id: &str| GetJobRequest {
            job_id: job_id.to_owned(),
        };
        let answer = rig.grpc::<_, GetJobResponse>("GetJob", request(text)).await;
        assert_eq!(refusal(answer), bad, "{text:?}");
        let request = GetResultRequest {
            job_id: text.to_owned(),
        };
        let answer = rig.grpc::<_, GetResultResponse>("GetResult", request).await;
        assert_eq!(refusal(answer), bad, "{text:?}");
        let request = GetLogsRequest {
            job_id: text.to_owned(),
        };
        let answer = rig.grpc::<_, GetLogsResponse>("GetLogs", request).await;
        assert_eq!(refusal(answer), bad, "{text:?}");
    }
    for uri in [
        "/v1/jobs/has%20space",
        "/v1/jobs/has%20space/result",
        "/v1/jobs//logs",
    ] {
        let (status, body) = rig.get(uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(json_of(&body)["field"], json!("jobId"), "{uri}");
    }
}

/// Asserts that a read of the store that fails answers `INTERNAL` with the message
/// `internal` over gRPC and 500 `{"error":"internal"}` over the gateway, for a job read
/// (`GetJob`, `GetResult`, `GetLogs`) and for a listing, and never the store's text. Catches
/// a failed read answered as an unknown job or an empty list, and a cause that leaks into
/// the answer.
#[tokio::test]
async fn a_failed_read_answers_internal() {
    let clock = clock();
    let store = Arc::new(Interfere::new(Fake::with_flaw(
        Arc::clone(&clock),
        Flaw::None,
    )));
    let rig = Rig::over(tables(store.clone()), clock).await;
    queue(&rig.jobs, "fails-1", spec(&[]));
    let internal = (Code::Internal, "internal".to_owned());
    store.fail_read_after(0);
    let answer = rig
        .grpc::<_, GetJobResponse>(
            "GetJob",
            GetJobRequest {
                job_id: "fails-1".to_owned(),
            },
        )
        .await;
    assert_eq!(refusal(answer), internal);
    store.fail_read_after(0);
    let answer = rig
        .grpc::<_, GetResultResponse>(
            "GetResult",
            GetResultRequest {
                job_id: "fails-1".to_owned(),
            },
        )
        .await;
    assert_eq!(refusal(answer), internal);
    store.fail_read_after(0);
    let answer = rig
        .grpc::<_, GetLogsResponse>(
            "GetLogs",
            GetLogsRequest {
                job_id: "fails-1".to_owned(),
            },
        )
        .await;
    assert_eq!(refusal(answer), internal);
    store.fail_read_after(0);
    let answer = rig
        .grpc::<_, ListJobsResponse>("ListJobs", ListJobsRequest::default())
        .await;
    assert_eq!(refusal(answer), internal);
    for uri in [
        "/v1/jobs/fails-1",
        "/v1/jobs/fails-1/result",
        "/v1/jobs/fails-1/logs",
        "/v1/jobs",
    ] {
        store.fail_read_after(0);
        let (status, body) = rig.get(uri).await;
        assert_eq!(
            (status, json_of(&body)),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "internal"})
            ),
            "{uri}"
        );
    }
    let job = rig.get_job("fails-1").await.expect("the next read passes");
    assert_eq!(job.id, "fails-1");
}

/// Every read call over both transports, for the jobs of
/// [`the_read_calls_change_nothing`] and an unknown id: each job read, a first page of one
/// job, and the next page filtered by a state and a label.
async fn read_everything(rig: &Rig) {
    for text in ["still-1", "still-2", "still-3", "never-1"] {
        let _ = rig.get_job(text).await;
        let _ = rig.get_result(text).await;
        let _ = rig.get_logs(text).await;
    }
    let first = ListJobsRequest {
        page_size: 1,
        ..ListJobsRequest::default()
    };
    let first = rig.list(first).await.expect("a page");
    let filtered = ListJobsRequest {
        states: vec![v1::JobState::Running.into()],
        labels: BTreeMap::from([("team".to_owned(), "ml".to_owned())]),
        page_token: first.next_page_token,
        ..ListJobsRequest::default()
    };
    rig.list(filtered).await.expect("a page");
}

/// Asserts that, over a store that records every write it is asked for, no read call asks
/// for one: `GetJob`, `ListJobs` (filtered and paged), `GetResult` and `GetLogs` of a queued,
/// a running and an ended job, of an expired one and of an unknown id, over both transports,
/// leave the store's writes as the setup left them. Catches a read that writes back (a
/// refreshed row, a counter, a cleanup of an expired job).
#[tokio::test]
async fn the_read_calls_change_nothing() {
    let clock = clock();
    let fake: Arc<dyn Store> = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let recording = Arc::new(RecordingStore::new(fake));
    let rig = Rig::over(tables(recording.clone()), clock).await;
    queue(&rig.jobs, "still-1", spec(&[("team", "ml")]));
    running(&rig.jobs, "still-2");
    let ended = running(&rig.jobs, "still-3");
    rig.jobs.end_job(&ended, exit_3()).expect("the job ends");
    let setup = recording.writes();
    assert!(!setup.is_empty());
    read_everything(&rig).await;
    rig.advance(rig.jobs.retention().window());
    read_everything(&rig).await;
    assert_eq!(recording.writes(), setup);
}
