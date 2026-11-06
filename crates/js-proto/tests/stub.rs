//! The generated client and server traits, run against each other in one process over an
//! in-memory duplex stream: no TCP or UDP socket and no port.

mod common;

use std::future::{Ready, ready};
use std::io;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use hyper_util::rt::TokioIo;
use js_proto::refusal::{refusal_of, refusal_status};
use js_proto::v1::job_service_client::JobServiceClient;
use js_proto::v1::job_service_server::{JobService, JobServiceServer};
use js_proto::v1::{
    Availability, CancelJobRequest, CancelJobResponse, CordonHostRequest, CordonHostResponse,
    GetJobRequest, GetJobResponse, GetLogsRequest, GetLogsResponse, GetResultRequest,
    GetResultResponse, GetUsageRequest, GetUsageResponse, Host, HostBooking, HostMark, HostSource,
    HostState, Job, JobResult, JobState, JobTimes, ListHostsRequest, ListHostsResponse,
    ListJobsRequest, ListJobsResponse, ListPoolsRequest, ListPoolsResponse, ListProvidersRequest,
    ListProvidersResponse, Machine, Memory, Pool, Provider, ProviderCheck, Refusal, RefusalCode,
    ResourceUsage, ResultReason, Shape, SubmitJobRequest, SubmitJobResponse, UncordonHostRequest,
    UncordonHostResponse, UsageRecord, WholeHostOutcome,
};
use tokio::io::DuplexStream;
use tonic::codegen::Service;
use tonic::codegen::http::Uri;
use tonic::codegen::tokio_stream;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Code, Request, Response, Status};

use common::{T0, job, map, spec};

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
        ready(
            self.0
                .take()
                .map(TokioIo::new)
                .ok_or_else(|| io::Error::other("the in-memory stream connects once")),
        )
    }
}

/// A client of `service`, served on a task of this runtime over an in-memory stream. The
/// endpoint's URI names no host that exists and is never resolved: the connector ignores
/// it.
async fn connect(service: impl JobService) -> JobServiceClient<Channel> {
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let server = Server::builder().serve_with_incoming(
        JobServiceServer::new(service),
        tokio_stream::once(Ok::<_, io::Error>(server_end)),
    );
    tokio::spawn(server);
    let channel = Endpoint::from_static("http://in-process.invalid")
        .connect_with_connector(InMemory(Some(client_end)))
        .await
        .expect("the in-memory channel connects");
    JobServiceClient::new(channel)
}

/// One call as the stub received it.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    SubmitJob(Box<SubmitJobRequest>),
    GetJob(GetJobRequest),
    ListJobs(ListJobsRequest),
    CancelJob(CancelJobRequest),
    GetResult(GetResultRequest),
    GetLogs(GetLogsRequest),
    ListHosts(ListHostsRequest),
    CordonHost(CordonHostRequest),
    UncordonHost(UncordonHostRequest),
    ListPools(ListPoolsRequest),
    ListProviders(ListProvidersRequest),
    GetUsage(GetUsageRequest),
}

/// A server that records every request and answers each method with its own sample.
#[derive(Clone, Default)]
struct Recording {
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Recording {
    fn record<T>(&self, call: Call, answer: T) -> Result<Response<T>, Status> {
        self.calls
            .lock()
            .expect("the record is not poisoned")
            .push(call);
        Ok(Response::new(answer))
    }

    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .expect("the record is not poisoned")
            .clone()
    }
}

#[tonic::async_trait]
impl JobService for Recording {
    async fn submit_job(
        &self,
        request: Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        self.record(
            Call::SubmitJob(Box::new(request.into_inner())),
            answers::submit_job(),
        )
    }

    async fn get_job(
        &self,
        request: Request<GetJobRequest>,
    ) -> Result<Response<GetJobResponse>, Status> {
        self.record(Call::GetJob(request.into_inner()), answers::get_job())
    }

    async fn list_jobs(
        &self,
        request: Request<ListJobsRequest>,
    ) -> Result<Response<ListJobsResponse>, Status> {
        self.record(Call::ListJobs(request.into_inner()), answers::list_jobs())
    }

    async fn cancel_job(
        &self,
        request: Request<CancelJobRequest>,
    ) -> Result<Response<CancelJobResponse>, Status> {
        self.record(Call::CancelJob(request.into_inner()), answers::cancel_job())
    }

    async fn get_result(
        &self,
        request: Request<GetResultRequest>,
    ) -> Result<Response<GetResultResponse>, Status> {
        self.record(Call::GetResult(request.into_inner()), answers::get_result())
    }

    async fn get_logs(
        &self,
        request: Request<GetLogsRequest>,
    ) -> Result<Response<GetLogsResponse>, Status> {
        self.record(Call::GetLogs(request.into_inner()), answers::get_logs())
    }

    async fn list_hosts(
        &self,
        request: Request<ListHostsRequest>,
    ) -> Result<Response<ListHostsResponse>, Status> {
        self.record(Call::ListHosts(request.into_inner()), answers::list_hosts())
    }

    async fn cordon_host(
        &self,
        request: Request<CordonHostRequest>,
    ) -> Result<Response<CordonHostResponse>, Status> {
        self.record(
            Call::CordonHost(request.into_inner()),
            answers::cordon_host(),
        )
    }

    async fn uncordon_host(
        &self,
        request: Request<UncordonHostRequest>,
    ) -> Result<Response<UncordonHostResponse>, Status> {
        self.record(
            Call::UncordonHost(request.into_inner()),
            answers::uncordon_host(),
        )
    }

    async fn list_pools(
        &self,
        request: Request<ListPoolsRequest>,
    ) -> Result<Response<ListPoolsResponse>, Status> {
        self.record(Call::ListPools(request.into_inner()), answers::list_pools())
    }

    async fn list_providers(
        &self,
        request: Request<ListProvidersRequest>,
    ) -> Result<Response<ListProvidersResponse>, Status> {
        self.record(
            Call::ListProviders(request.into_inner()),
            answers::list_providers(),
        )
    }

    async fn get_usage(
        &self,
        request: Request<GetUsageRequest>,
    ) -> Result<Response<GetUsageResponse>, Status> {
        self.record(Call::GetUsage(request.into_inner()), answers::get_usage())
    }
}

/// The requests the client sends: every field holds a value that is not its default, and
/// no two requests share a value.
mod requests {
    use super::*;

    pub fn submit_job() -> SubmitJobRequest {
        SubmitJobRequest {
            spec: Some(spec("presigned-url-text")),
        }
    }

    pub fn get_job() -> GetJobRequest {
        GetJobRequest {
            job_id: "get-job".to_owned(),
        }
    }

    pub fn list_jobs() -> ListJobsRequest {
        ListJobsRequest {
            states: vec![JobState::Running.into(), JobState::Lost.into()],
            labels: map(&[("team", "list")]),
            queued_after_ms: Some(T0 + 10),
            queued_before_ms: Some(T0 + 20),
            page_size: 25,
            page_token: "list-page-2".to_owned(),
            client: Some("list-client".to_owned()),
        }
    }

    pub fn cancel_job() -> CancelJobRequest {
        CancelJobRequest {
            job_id: "cancel-job".to_owned(),
        }
    }

    pub fn get_result() -> GetResultRequest {
        GetResultRequest {
            job_id: "result-job".to_owned(),
        }
    }

    pub fn get_logs() -> GetLogsRequest {
        GetLogsRequest {
            job_id: "logs-job".to_owned(),
        }
    }

    pub fn cordon_host() -> CordonHostRequest {
        CordonHostRequest {
            host_id: "cordon-host".to_owned(),
            cordon_secs: Some(600),
        }
    }

    pub fn uncordon_host() -> UncordonHostRequest {
        UncordonHostRequest {
            host_id: "uncordon-host".to_owned(),
        }
    }

    pub fn get_usage() -> GetUsageRequest {
        GetUsageRequest {
            from_ms: Some(T0 + 30),
            to_ms: Some(T0 + 40),
            host_id: "usage-host".to_owned(),
            job_id: "usage-job".to_owned(),
            page_size: 50,
            page_token: "usage-page-2".to_owned(),
        }
    }
}

/// The stub's answers: every field holds a value that is not its default, and each
/// method's answer differs from the others'.
mod answers {
    use super::*;

    fn with_id(id: &str) -> Job {
        Job {
            id: id.to_owned(),
            client: Some("stub-client".to_owned()),
            ..job()
        }
    }

    pub fn result() -> JobResult {
        JobResult {
            state: JobState::Failed.into(),
            exit_code: Some(3),
            reason: ResultReason::Exit.into(),
            stdout_tail: b"stdout tail".to_vec(),
            stderr_tail: b"stderr tail".to_vec(),
            truncated: true,
            times: Some(JobTimes {
                queued_at_ms: T0 + 100,
                placed_at_ms: Some(T0 + 101),
                started_at_ms: Some(T0 + 102),
                ended_at_ms: Some(T0 + 103),
            }),
            usage: Some(ResourceUsage {
                cpu_usec: 7_000_000,
                peak_memory_bytes: 3 << 20,
                disk_bytes: Some(5 << 20),
                gpus: 2,
            }),
        }
    }

    fn booking(job_id: &str) -> HostBooking {
        HostBooking {
            job_id: job_id.to_owned(),
            cpu_millis: 2000,
            memory_bytes: 4 << 30,
            disk_bytes: 8 << 30,
            gpus: 1,
            host_ports: vec![8080, 9090],
        }
    }

    fn memory() -> Memory {
        Memory {
            configured_bytes: 64 << 30,
            schedulable_percent: 90,
            schedulable_bytes: 57_982_058_496,
        }
    }

    pub fn host(id: &str) -> Host {
        Host {
            id: id.to_owned(),
            source: HostSource::Registered.into(),
            state: HostState::Live.into(),
            shape: Some(Shape {
                cpu_millis: 16_000,
                memory_bytes: 64 << 30,
                gpus: 4,
                gpu_class: "nvidia-a100-sxm4-80gb".to_owned(),
                disk_bytes: Some(500 << 30),
            }),
            memory: Some(memory()),
            capabilities: map(&[("os", "linux"), ("arch", "x86_64")]),
            labels: map(&[("rack", "r1")]),
            booked: vec![booking("booked-job")],
            mark: HostMark::Draining.into(),
            cordon_expires_at_ms: Some(T0 + 300_000),
            taint_reason: "post-job-check-failed".to_owned(),
            agent_version: "0.1.0".to_owned(),
        }
    }

    pub fn submit_job() -> SubmitJobResponse {
        SubmitJobResponse {
            job: Some(with_id("submitted-job")),
        }
    }

    pub fn get_job() -> GetJobResponse {
        GetJobResponse {
            job: Some(with_id("got-job")),
        }
    }

    pub fn list_jobs() -> ListJobsResponse {
        ListJobsResponse {
            jobs: vec![with_id("listed-job-1"), with_id("listed-job-2")],
            next_page_token: "list-page-3".to_owned(),
        }
    }

    pub fn cancel_job() -> CancelJobResponse {
        CancelJobResponse {
            job: Some(with_id("cancelled-job")),
        }
    }

    pub fn get_result() -> GetResultResponse {
        GetResultResponse {
            result: Some(result()),
        }
    }

    pub fn get_logs() -> GetLogsResponse {
        GetLogsResponse {
            state: JobState::Running.into(),
            stdout_tail: b"stdout so far".to_vec(),
            stderr_tail: b"stderr so far".to_vec(),
            truncated: true,
        }
    }

    pub fn list_hosts() -> ListHostsResponse {
        ListHostsResponse {
            hosts: vec![host("listed-host")],
        }
    }

    pub fn cordon_host() -> CordonHostResponse {
        CordonHostResponse {
            host: Some(host("cordoned-host")),
            already_cordoned: true,
        }
    }

    pub fn uncordon_host() -> UncordonHostResponse {
        UncordonHostResponse {
            host: Some(host("uncordoned-host")),
            lifted: true,
        }
    }

    pub fn list_pools() -> ListPoolsResponse {
        ListPoolsResponse {
            pools: vec![Pool {
                name: "gpu".to_owned(),
                kind: "lambdalabs".to_owned(),
                state: Availability::Unavailable.into(),
                reasons: vec!["no-offer".to_owned()],
                memory: Some(memory()),
                instances: 3,
                last_tick_ms: Some(T0 + 5000),
            }],
        }
    }

    pub fn list_providers() -> ListProvidersResponse {
        ListProvidersResponse {
            providers: vec![Provider {
                name: "gpu".to_owned(),
                kind: "lambdalabs".to_owned(),
                state: Availability::Available.into(),
                checks: vec![ProviderCheck {
                    name: "credentials".to_owned(),
                    ok: true,
                    reason: "unused-when-ok".to_owned(),
                    note: "key read".to_owned(),
                    checked_at_ms: T0 + 6000,
                }],
                machines: vec![Machine {
                    host: "machine-host".to_owned(),
                    state: "joined".to_owned(),
                    offer: "gpu_1x_a100".to_owned(),
                    zone: "zone-1".to_owned(),
                    price_cents_per_hour: Some(129),
                    provider_id: "instance-1".to_owned(),
                }],
            }],
        }
    }

    pub fn get_usage() -> GetUsageResponse {
        GetUsageResponse {
            records: vec![UsageRecord {
                job_id: "usage-job".to_owned(),
                host_id: "usage-host".to_owned(),
                queue_wait_ms: 1500,
                run_ms: 60_000,
                end_state: JobState::Succeeded.into(),
                ended_reason: ResultReason::Exit.into(),
                ended_at_ms: T0 + 7000,
                booked: Some(booking("usage-job")),
                used: Some(ResourceUsage {
                    cpu_usec: 9_000_000,
                    peak_memory_bytes: 1 << 30,
                    disk_bytes: Some(2 << 20),
                    gpus: 1,
                }),
                whole_host: WholeHostOutcome::Drained.into(),
                mark_give_ups: 1,
                estimated_run_ms: Some(55_000),
            }],
            next_page_token: "usage-page-3".to_owned(),
        }
    }
}

/// The answer of a call that must succeed.
fn answer<T>(reply: Result<Response<T>, Status>) -> T {
    reply.expect("the stub answers").into_inner()
}

/// The status code of a call that must fail.
fn code<T>(reply: Result<Response<T>, Status>) -> Code {
    reply.map(|_| ()).expect_err("the call fails").code()
}

/// Asserts that each of the 12 methods carries its request to the stub's own handler with
/// every field intact, and carries that handler's answer back with every field intact.
/// Catches a field dropped on the way and a method wired to another method's handler.
#[tokio::test]
async fn every_method_round_trips_through_a_stub() {
    let stub = Recording::default();
    let mut client = connect(stub.clone()).await;
    assert_eq!(
        answer(client.submit_job(requests::submit_job()).await),
        answers::submit_job()
    );
    assert_eq!(
        answer(client.get_job(requests::get_job()).await),
        answers::get_job()
    );
    assert_eq!(
        answer(client.list_jobs(requests::list_jobs()).await),
        answers::list_jobs()
    );
    assert_eq!(
        answer(client.cancel_job(requests::cancel_job()).await),
        answers::cancel_job()
    );
    assert_eq!(
        answer(client.get_result(requests::get_result()).await),
        answers::get_result()
    );
    assert_eq!(
        answer(client.get_logs(requests::get_logs()).await),
        answers::get_logs()
    );
    assert_eq!(
        answer(client.list_hosts(ListHostsRequest {}).await),
        answers::list_hosts()
    );
    assert_eq!(
        answer(client.cordon_host(requests::cordon_host()).await),
        answers::cordon_host()
    );
    assert_eq!(
        answer(client.uncordon_host(requests::uncordon_host()).await),
        answers::uncordon_host()
    );
    assert_eq!(
        answer(client.list_pools(ListPoolsRequest {}).await),
        answers::list_pools()
    );
    assert_eq!(
        answer(client.list_providers(ListProvidersRequest {}).await),
        answers::list_providers()
    );
    assert_eq!(
        answer(client.get_usage(requests::get_usage()).await),
        answers::get_usage()
    );
    assert_eq!(
        stub.calls(),
        [
            Call::SubmitJob(Box::new(requests::submit_job())),
            Call::GetJob(requests::get_job()),
            Call::ListJobs(requests::list_jobs()),
            Call::CancelJob(requests::cancel_job()),
            Call::GetResult(requests::get_result()),
            Call::GetLogs(requests::get_logs()),
            Call::ListHosts(ListHostsRequest {}),
            Call::CordonHost(requests::cordon_host()),
            Call::UncordonHost(requests::uncordon_host()),
            Call::ListPools(ListPoolsRequest {}),
            Call::ListProviders(ListProvidersRequest {}),
            Call::GetUsage(requests::get_usage()),
        ]
    );
}

/// A server that refuses every submit as a job id conflict, with its detail text.
struct Refusing(&'static str);

#[tonic::async_trait]
impl JobService for Refusing {
    async fn submit_job(
        &self,
        _: Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        Err(refusal_status(RefusalCode::JobIdConflict, self.0))
    }
}

/// Asserts that a refusal a server answers reaches the client as a status with the
/// refusal's gRPC code, the detail as its message, and details that still decode to the same
/// code and detail. Catches the details being lost between the server and the client.
#[tokio::test]
async fn stub_refusal_reaches_the_client_as_a_refusal() {
    let mut client = connect(Refusing("job-1 names another job")).await;
    let status = client
        .submit_job(requests::submit_job())
        .await
        .expect_err("the stub refuses");
    assert_eq!(status.code(), Code::AlreadyExists);
    assert_eq!(status.message(), "job-1 names another job");
    assert_eq!(
        refusal_of(&status),
        Some(Refusal {
            code: RefusalCode::JobIdConflict.into(),
            detail: "job-1 names another job".to_owned(),
        })
    );
}

/// Asserts that an empty detail, and one holding characters that the transport has to
/// escape in a status message (a percent sign, a literal percent escape, non-ASCII letters
/// and symbols, a line break and a carriage return), reach the client unchanged, both as the
/// status message and in the decoded refusal.
#[tokio::test]
async fn refusal_details_with_any_text_reach_the_client_unchanged() {
    for detail in ["", "100% sûr: ½ ✓ %41\nline two\r\nend"] {
        let mut client = connect(Refusing(detail)).await;
        let status = client
            .submit_job(requests::submit_job())
            .await
            .expect_err("the stub refuses");
        assert_eq!(status.code(), Code::AlreadyExists, "{detail:?}");
        assert_eq!(status.message(), detail);
        assert_eq!(
            refusal_of(&status),
            Some(Refusal {
                code: RefusalCode::JobIdConflict.into(),
                detail: detail.to_owned(),
            })
        );
    }
}

/// A server that implements `GetJob` alone.
struct OnlyGetJob;

#[tonic::async_trait]
impl JobService for OnlyGetJob {
    async fn get_job(&self, _: Request<GetJobRequest>) -> Result<Response<GetJobResponse>, Status> {
        Ok(Response::new(answers::get_job()))
    }
}

/// Asserts that a server implementing only `GetJob` answers it and answers each of the
/// other 11 methods `UNIMPLEMENTED`, so a partial server fails closed.
#[tokio::test]
async fn unimplemented_default_methods_answer_unimplemented() {
    let mut client = connect(OnlyGetJob).await;
    assert_eq!(
        client
            .get_job(requests::get_job())
            .await
            .expect("GetJob is implemented")
            .into_inner(),
        answers::get_job()
    );
    let codes = [
        code(client.submit_job(requests::submit_job()).await),
        code(client.list_jobs(requests::list_jobs()).await),
        code(client.cancel_job(requests::cancel_job()).await),
        code(client.get_result(requests::get_result()).await),
        code(client.get_logs(requests::get_logs()).await),
        code(client.list_hosts(ListHostsRequest {}).await),
        code(client.cordon_host(requests::cordon_host()).await),
        code(client.uncordon_host(requests::uncordon_host()).await),
        code(client.list_pools(ListPoolsRequest {}).await),
        code(client.list_providers(ListProvidersRequest {}).await),
        code(client.get_usage(requests::get_usage()).await),
    ];
    assert_eq!(codes, [Code::Unimplemented; 11]);
}
