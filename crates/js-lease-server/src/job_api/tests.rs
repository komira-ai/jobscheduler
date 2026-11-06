//! The job service skeleton over both transports, in one process: gRPC through a tonic client
//! over an in-memory duplex stream, the gateway through its router's `oneshot`. No TCP or UDP
//! socket and no port.

use std::future::{Ready, ready};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{HeaderValue, Request as HttpRequest, StatusCode, Uri, header};
use hyper_util::rt::TokioIo;
use js_jobs::JobStore;
use js_lease::{Clock, ManualClock, Timestamp};
use js_proto::FILE_DESCRIPTOR_SET;
use js_proto::v1::job_service_server::{JobService, JobServiceServer};
use js_proto::v1::{
    CancelJobRequest, CancelJobResponse, CordonHostRequest, CordonHostResponse, GetJobRequest,
    GetJobResponse, GetLogsRequest, GetLogsResponse, GetResultRequest, GetResultResponse,
    GetUsageRequest, GetUsageResponse, ListHostsRequest, ListHostsResponse, ListJobsRequest,
    ListJobsResponse, ListPoolsRequest, ListPoolsResponse, ListProvidersRequest,
    ListProvidersResponse, SubmitJobRequest, SubmitJobResponse, UncordonHostRequest,
    UncordonHostResponse,
};
use js_state::Store;
use js_state::fake::{Fake, Flaw};
use js_state::sqlite::{Options, SqliteStore};
use js_testkit::dir::TestDir;
use js_types::HostId;
use prost_types::FileDescriptorSet;
use tokio::io::DuplexStream;
use tonic::client::Grpc;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::codegen::tokio_stream;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Code, Request, Response, Status};
use tonic_prost::ProstCodec;
use tonic_prost::prost::Message;
use tower::{Service, ServiceExt};

use super::{
    Bearer, JobApi, UNIMPLEMENTED, cancel, fleet, gateway, gateway_over, grpc, grpc_over, read,
    submit,
};
use crate::adapt::SharedClock;
use crate::config::{Clients, LiveClients, parse_token};
use crate::hosts::{HostList, LeasesOn};
use crate::job_store::ServerJobs;
use crate::status::Status as PoolStatus;

/// The token the services under test are built with.
pub(super) const TOKEN: &str = "job-api-test-token-0123456789abcdef";

/// The `Authorization` values that present no token or another one, with the reason each
/// is refused: no header at all is the `None` case of the tests. Together they cover each
/// way `Bearer <token>` can be wrong: the scheme, the separator, the token's length and its
/// bytes at the same length.
const REFUSED: [(&str, &str); 6] = [
    (
        "Bearer job-api-test-token-0123456789abcdeX",
        "same length, last byte differs",
    ),
    (
        "Bearer job-api-test-token-0123456789abcde",
        "a prefix of the token",
    ),
    (
        "Bearer job-api-test-token-0123456789abcdef0",
        "the token and one byte more",
    ),
    (
        "Basic job-api-test-token-0123456789abcdef",
        "the token under another scheme",
    ),
    (
        "Bearerjob-api-test-token-0123456789abcdef",
        "no space after the scheme",
    ),
    ("Bearer ", "the scheme with no token"),
];

/// The gRPC service the methods are paths of.
pub(super) const SERVICE: &str = "jobscheduler.v1.JobService";

/// The methods a group serves over a service built with every context at its default
/// (`read.rs` and `cancel.rs`): they answer what the call answers, not `UNIMPLEMENTED`.
const SERVED: [&str; 5] = ["GetJob", "ListJobs", "CancelJob", "GetResult", "GetLogs"];

/// The methods the fleet group (`fleet.rs`) serves once its context is given the server's
/// handles ([`fleet::Ctx::served`]), as the server's service is.
const FLEET: [&str; 6] = [
    "ListHosts",
    "CordonHost",
    "UncordonHost",
    "ListPools",
    "ListProviders",
    "GetUsage",
];

/// The time the services' clock reads, in milliseconds since the Unix epoch.
const NOW_MS: u64 = 1_800_000_000_000;

/// The body the gateway answers a call the service does not implement with.
const UNIMPLEMENTED_BODY: &str = r#"{"error":"unimplemented"}"#;

/// The clients the services under test admit: the client `default`, whose token is [`TOKEN`].
pub(super) fn token() -> LiveClients {
    let token = parse_token(TOKEN.as_bytes()).expect("the test token is a valid token");
    LiveClients::new(Clients::only(&token, "--token-file"))
}

fn clock() -> SharedClock {
    SharedClock::new(ManualClock::new(Timestamp::from_millis(NOW_MS)))
}

/// Job tables over `store`, as the server holds them.
fn tables(store: impl Store + 'static) -> Arc<ServerJobs> {
    let store: Arc<dyn Store> = Arc::new(store);
    let jobs = JobStore::new(Arc::new(store), crate::job_store::ServerIds::random())
        .expect("the job tables open");
    Arc::new(jobs)
}

/// Job tables over the in-memory fake store.
fn over_fake() -> Arc<ServerJobs> {
    tables(Fake::with_flaw(Arc::default(), Flaw::None))
}

/// Job tables over a SQLite file in `dir`.
fn over_sqlite(dir: &TestDir) -> Arc<ServerJobs> {
    let path = dir.path().join("state.db");
    let clock: Arc<dyn js_state::Clock> = Arc::new(js_state::ManualClock::default());
    tables(SqliteStore::open(&path, clock, Options::default()).expect("the state file opens"))
}

/// An empty directory of the test's own under the target directory, removed when the
/// guard is dropped.
pub(super) fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/job_api");
    TestDir::under(root, name)
}

/// The method names of the job service in the service definition, in its order: the
/// generated service serves exactly these paths.
pub(super) fn methods() -> Vec<String> {
    let set = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("the descriptor set decodes");
    set.file
        .iter()
        .filter(|file| file.package() == "jobscheduler.v1")
        .flat_map(|file| file.service.iter())
        .filter(|service| service.name() == "JobService")
        .flat_map(|service| service.method.iter())
        .map(|method| method.name().to_owned())
        .collect()
}

/// The method names of the gateway's routes, in the order of `ROUTES`.
fn routed() -> Vec<&'static str> {
    js_api_gateway::ROUTES
        .iter()
        .map(|route| route.rpc.name())
        .collect()
}

/// A connector that hands the channel one end of an in-memory stream, once.
struct InMemory(Option<DuplexStream>);

impl Service<Uri> for InMemory {
    type Response = TokioIo<DuplexStream>;
    type Error = io::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let stream = self.0.take().map(TokioIo::new);
        ready(stream.ok_or(io::Error::from(io::ErrorKind::NotConnected)))
    }
}

/// A channel to `service`, served on a task of this runtime over an in-memory stream. The
/// endpoint's URI names no host that exists and is never resolved: the connector ignores
/// it.
pub(super) async fn connect<T: JobService>(service: Bearer<JobServiceServer<T>>) -> Channel {
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let server = Server::builder()
        .serve_with_incoming(service, tokio_stream::once(Ok::<_, io::Error>(server_end)));
    tokio::spawn(server);
    Endpoint::from_static("http://in-process.invalid")
        .connect_with_connector(InMemory(Some(client_end)))
        .await
        .expect("the in-memory channel connects")
}

/// The answer of the gRPC call of `method` with an empty request, sent with `authorization`
/// as its metadata of that name (none when `None`): `Ok` when the call succeeded (whatever
/// its response), else its status.
pub(super) async fn call(
    channel: &Channel,
    method: &str,
    authorization: Option<&str>,
) -> Result<(), Status> {
    let mut grpc = Grpc::new(channel.clone());
    grpc.ready().await.expect("the channel is ready");
    let path = PathAndQuery::try_from(format!("/{SERVICE}/{method}")).expect("the path parses");
    let mut request = Request::new(());
    if let Some(value) = authorization {
        let value = MetadataValue::try_from(value).expect("the value is ASCII");
        request.metadata_mut().insert("authorization", value);
    }
    grpc.unary::<(), (), _>(request, path, ProstCodec::default())
        .await
        .map(|_| ())
}

/// The status and body of the gateway's answer to each of its routes, in the order of
/// `ROUTES`, with `authorization` as the `Authorization` header (none when `None`). A
/// route's path variables are each `x1`; a POST carries the body `{}` as JSON.
pub(super) async fn send_all(
    router: &Router,
    authorization: Option<HeaderValue>,
) -> Vec<(StatusCode, Bytes)> {
    let mut answers = Vec::new();
    for route in js_api_gateway::ROUTES {
        let mut request = HttpRequest::builder()
            .method(route.verb.as_str())
            .uri(fill(route.template));
        if let Some(value) = &authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        let body = if route.body {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from("{}")
        } else {
            Body::empty()
        };
        let request = request.body(body).expect("the request builds");
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("the router answers");
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body reads");
        answers.push((status, body));
    }
    answers
}

/// `template` with each path variable (`{name}`) replaced by `x1`.
fn fill(template: &str) -> String {
    let mut path = String::new();
    let mut in_variable = false;
    for c in template.chars() {
        match c {
            '{' => {
                in_variable = true;
                path.push_str("x1");
            }
            '}' => in_variable = false,
            _ if in_variable => {}
            _ => path.push(c),
        }
    }
    path
}

/// The header value of `text`.
pub(super) fn value(text: &str) -> HeaderValue {
    HeaderValue::from_str(text).expect("the text is a header value")
}

/// The job service that counts the calls that reach it and answers each as [`JobApi`] does:
/// a call it counts is a call that reached the group function of [`JobApi`].
pub(super) struct Recording {
    api: JobApi,
    calls: AtomicUsize,
}

impl Recording {
    pub(super) fn new() -> Arc<Recording> {
        Arc::new(Recording {
            api: JobApi::new(over_fake(), clock()),
            calls: AtomicUsize::new(0),
        })
    }

    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// Writes the [`JobService`] methods of [`Recording`]: each counts the call and hands it to
/// [`JobApi`]'s method of the same name.
macro_rules! recorded {
    ($($method:ident: $request:ty => $response:ty;)*) => {
        #[tonic::async_trait]
        impl JobService for Recording {
            $(
                async fn $method(
                    &self,
                    request: Request<$request>,
                ) -> Result<Response<$response>, Status> {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    JobService::$method(&self.api, request).await
                }
            )*
        }
    };
}

recorded! {
    submit_job: SubmitJobRequest => SubmitJobResponse;
    get_job: GetJobRequest => GetJobResponse;
    list_jobs: ListJobsRequest => ListJobsResponse;
    cancel_job: CancelJobRequest => CancelJobResponse;
    get_result: GetResultRequest => GetResultResponse;
    get_logs: GetLogsRequest => GetLogsResponse;
    list_hosts: ListHostsRequest => ListHostsResponse;
    cordon_host: CordonHostRequest => CordonHostResponse;
    uncordon_host: UncordonHostRequest => UncordonHostResponse;
    list_pools: ListPoolsRequest => ListPoolsResponse;
    list_providers: ListProvidersRequest => ListProvidersResponse;
    get_usage: GetUsageRequest => GetUsageResponse;
}

/// Asserts that a request with no `Authorization`, with a non-UTF-8 one (gateway only: gRPC
/// metadata cannot carry it), or with each value of [`REFUSED`] is answered, for every method
/// over gRPC, `UNAUTHENTICATED` with an empty message, and for every route of the gateway,
/// 401 with an empty body, and that none of those requests reaches a handler of the
/// recording service. Catches a check that runs after the handler, a transport left
/// unguarded, and a refusal that differs between the transports.
#[tokio::test]
async fn an_unauthenticated_request_runs_no_handler() {
    let recording = Recording::new();
    let channel = connect(grpc_over(recording.clone(), token())).await;
    let router = gateway_over(recording.clone(), token());
    let mut cases: Vec<Option<&str>> = vec![None];
    cases.extend(REFUSED.iter().map(|(text, _)| Some(*text)));
    for authorization in cases {
        for method in methods() {
            let status = call(&channel, &method, authorization)
                .await
                .expect_err("a refused request fails");
            assert_eq!(
                (status.code(), status.message()),
                (Code::Unauthenticated, ""),
                "{method} with {authorization:?}"
            );
        }
        let answers = send_all(&router, authorization.map(value)).await;
        for (route, answer) in js_api_gateway::ROUTES.iter().zip(answers) {
            assert_eq!(
                answer,
                (StatusCode::UNAUTHORIZED, Bytes::new()),
                "{} with {authorization:?}",
                route.template
            );
        }
    }
    let not_utf8 = HeaderValue::from_bytes(b"Bearer \xff").expect("obs-text is a header value");
    for answer in send_all(&router, Some(not_utf8)).await {
        assert_eq!(answer, (StatusCode::UNAUTHORIZED, Bytes::new()));
    }
    assert_eq!(recording.calls(), 0);
}

/// Asserts that the token both services were built with, presented as `Bearer <token>` and as
/// `bearer <token>` (the scheme is matched without regard to case), takes every method over
/// gRPC and every route of the gateway to the recording service, which counts each call once:
/// a call not served yet answers `UNIMPLEMENTED` (501), and a served one answers neither that
/// nor `UNAUTHENTICATED` (401). Catches a transport that reads a token of its own or refuses
/// the right one.
#[tokio::test]
async fn the_same_token_opens_both_transports() {
    let recording = Recording::new();
    let channel = connect(grpc_over(recording.clone(), token())).await;
    let router = gateway_over(recording.clone(), token());
    let methods = methods();
    let mut expected = 0;
    for scheme in ["Bearer", "bearer"] {
        let authorization = format!("{scheme} {TOKEN}");
        for method in &methods {
            let code = call(&channel, method, Some(&authorization))
                .await
                .map_or_else(|status| status.code(), |()| Code::Ok);
            match SERVED.contains(&method.as_str()) {
                true => assert!(
                    ![Code::Unimplemented, Code::Unauthenticated].contains(&code),
                    "{method}: {code:?}"
                ),
                false => assert_eq!(code, Code::Unimplemented, "{method}"),
            }
        }
        expected += methods.len();
        assert_eq!(recording.calls(), expected);
        let answers = send_all(&router, Some(value(&authorization))).await;
        for (route, (status, _)) in js_api_gateway::ROUTES.iter().zip(answers) {
            match SERVED.contains(&route.rpc.name()) {
                true => assert!(
                    ![StatusCode::NOT_IMPLEMENTED, StatusCode::UNAUTHORIZED].contains(&status),
                    "{}: {status}",
                    route.template
                ),
                false => assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{}", route.template),
            }
        }
        expected += js_api_gateway::ROUTES.len();
        assert_eq!(recording.calls(), expected);
    }
}

/// The hosts of a server that holds none: an empty listing, and every cordon and uncordon
/// refused as of a host the registry does not hold.
struct NoHosts;

#[tonic::async_trait]
impl fleet::Hosts for NoHosts {
    async fn hosts(&self) -> (HostList, LeasesOn) {
        (HostList::of(Vec::new(), Vec::new(), None), LeasesOn::new())
    }

    async fn cordon(&self, _: &HostId, _: Duration) -> Result<bool, fleet::Refused> {
        Err(fleet::Refused::UnknownHost)
    }

    async fn uncordon(&self, _: &HostId) -> Result<bool, fleet::Refused> {
        Err(fleet::Refused::UnknownHost)
    }
}

/// Whether `method` is one no group serves yet: neither in [`SERVED`] nor in [`FLEET`].
fn unserved(method: &str) -> bool {
    !SERVED.contains(&method) && !FLEET.contains(&method)
}

/// Asserts that the methods of the service definition are the gateway's routes, one each in
/// the same order, and that, over the fake store and over SQLite, with the fleet group served
/// (over no hosts, the status of no pool and no usage history), every method that no group
/// serves yet (all but [`SERVED`] and [`FLEET`]) answers `UNIMPLEMENTED` over gRPC with the
/// stubs' own message (not the generated default's, so a method added to the proto that
/// reaches no stub of [`JobApi`] fails here) and 501 `unimplemented` over the gateway, both
/// transports calling one [`JobApi`], while every other method answers neither: so the list
/// of unserved methods is exact both ways, and a method taken off it that is not served, or
/// a served one left on it, fails here. The served calls have tests of their own
/// (`read_tests.rs`, `submit_tests.rs`, `cancel_tests.rs`, `fleet_tests.rs`). The production
/// builders `grpc` and `gateway` also refuse a request without the token
/// (`UNAUTHENTICATED`, 401), as the recording tests show for the generic ones.
#[tokio::test]
async fn every_unserved_call_answers_unimplemented() {
    let methods = methods();
    assert_eq!(methods, routed());
    let dir = scratch("every_call");
    for jobs in [over_fake(), over_sqlite(&dir)] {
        let pools = PoolStatus::new(None, None, None, Vec::new(), clock());
        let fleet = fleet::Ctx::served(Arc::new(NoHosts), Arc::new(pools), None);
        let api = Arc::new(JobApi::new(jobs, clock()).with_fleet(fleet));
        let channel = connect(grpc(api.clone(), token())).await;
        let authorization = format!("Bearer {TOKEN}");
        let not_served = || methods.iter().filter(|method| unserved(method));
        assert_eq!(
            not_served().count(),
            methods.len() - SERVED.len() - FLEET.len()
        );
        for method in methods.iter().filter(|method| !unserved(method)) {
            let code = call(&channel, method, Some(&authorization))
                .await
                .map_or_else(|status| status.code(), |()| Code::Ok);
            assert_ne!(code, Code::Unimplemented, "{method}");
        }
        for method in not_served() {
            let status = call(&channel, method, Some(&authorization))
                .await
                .expect_err("a call not served yet fails");
            assert_eq!(
                (status.code(), status.message()),
                (Code::Unimplemented, UNIMPLEMENTED),
                "{method}"
            );
        }
        let refused = call(&channel, &methods[0], None).await;
        assert_eq!(
            refused.expect_err("no token, no call").code(),
            Code::Unauthenticated
        );
        let router = gateway(api, token());
        for (status, body) in send_all(&router, None).await {
            assert_eq!((status, body), (StatusCode::UNAUTHORIZED, Bytes::new()));
        }
        let answers = send_all(&router, Some(value(&authorization))).await;
        for (method, answer) in methods.iter().zip(answers) {
            match unserved(method) {
                true => assert_eq!(
                    answer,
                    (
                        StatusCode::NOT_IMPLEMENTED,
                        Bytes::from_static(UNIMPLEMENTED_BODY.as_bytes())
                    ),
                    "{method}"
                ),
                false => assert_ne!(answer.0, StatusCode::NOT_IMPLEMENTED, "{method}"),
            }
        }
    }
}

/// Asserts that each group's builder sets its own context and keeps the other three, the
/// job store and the clock: the read context given first survives the other three builders,
/// and replacing it at the end leaves theirs. Catches a builder that resets or overwrites
/// another group's context or rebuilds the service around new handles.
#[test]
fn a_group_context_is_replaced_without_touching_the_others() {
    let jobs = over_fake();
    let api = JobApi::new(jobs.clone(), clock());
    assert_eq!(
        (&api.read, &api.submit, &api.cancel, &api.fleet),
        (
            &read::Ctx::default(),
            &submit::Ctx::default(),
            &cancel::Ctx::default(),
            &fleet::Ctx::default()
        )
    );
    let marked = |mark| read::Ctx {
        mark,
        ..read::Ctx::default()
    };
    let api = api.with_read(marked(1));
    assert_eq!(api.read, marked(1));
    assert_eq!(
        (&api.submit, &api.cancel, &api.fleet),
        (
            &submit::Ctx::default(),
            &cancel::Ctx::default(),
            &fleet::Ctx::default()
        )
    );
    let api = api
        .with_submit(submit::Ctx::marked(2))
        .with_cancel(cancel::Ctx { mark: 3 })
        .with_fleet(fleet::Ctx::marked(4))
        .with_read(marked(5));
    assert_eq!(
        (
            api.read.mark,
            api.submit.mark,
            api.cancel.mark,
            api.fleet.mark
        ),
        (5, 2, 3, 4)
    );
    assert!(Arc::ptr_eq(&api.jobs, &jobs));
    assert_eq!(api.clock.now(), Timestamp::from_millis(NOW_MS));
}

/// Asserts that the service holds the job tables and the clock it is built with, over the
/// fake store and over SQLite: the very tables (not a copy) and a clock that reads the given
/// clock's time.
#[test]
fn a_job_api_holds_the_tables_and_clock_it_is_given() {
    let dir = scratch("holds");
    for jobs in [over_fake(), over_sqlite(&dir)] {
        let api = JobApi::new(jobs.clone(), clock());
        assert!(Arc::ptr_eq(&api.jobs, &jobs));
        assert_eq!(api.clock.now(), Timestamp::from_millis(NOW_MS));
    }
}
