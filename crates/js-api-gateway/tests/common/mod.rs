//! What the gateway's tests share: a stub [`JobService`] that records each call, a request
//! helper over the router (tower `oneshot`, no socket), fully populated sample messages, and
//! the test descriptors under `tests/protos`.

#![allow(dead_code, reason = "each test binary uses a part of the module")]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request as HttpRequest, StatusCode};
use js_api_gateway::router;
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{
    Availability, Host, HostBooking, HostMark, HostSource, HostState, Machine, Memory, Pool,
    Provider, ProviderCheck, Shape, UsageRecord, WholeHostOutcome,
};
use js_proto::v1::{
    CancelJobRequest, CancelJobResponse, Constraints, CordonHostRequest, CordonHostResponse,
    ExecMode, GetJobRequest, GetJobResponse, GetLogsRequest, GetLogsResponse, GetResultRequest,
    GetResultResponse, GetUsageRequest, GetUsageResponse, Job, JobFile, JobResult, JobSpec,
    JobState, JobTimes, ListHostsRequest, ListHostsResponse, ListJobsRequest, ListJobsResponse,
    ListPoolsRequest, ListPoolsResponse, ListProvidersRequest, ListProvidersResponse, LogGap,
    LogStream, Needs, NetworkLevel, PortForward, ResourceUsage, ResultReason, StartFailureCode,
    SubmitJobRequest, SubmitJobResponse, UncordonHostRequest, UncordonHostResponse,
};
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage};
use tonic::{Request, Response, Status};
use tower::ServiceExt;

/// A time in milliseconds since the Unix epoch above 2^53: a JSON number would round it.
pub const BIG: u64 = (1 << 53) + 1;

/// One call as the stub received it: the method's name, the encoded request, and the
/// [`Marker`] its extensions carried, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// The method's name in the service, such as `GetJob`.
    pub rpc: &'static str,
    /// The request message, encoded.
    pub request: Vec<u8>,
    /// The marker a layer in front of the router set on the HTTP request.
    pub marker: Option<Marker>,
}

/// A value a layer in front of the router sets in the extensions of an HTTP request, as a
/// server's bearer layer sets who the caller is.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker(pub String);

/// A [`JobService`] that records every call and answers each with a fixed sample, or every
/// call with one status.
#[derive(Default)]
pub struct Stub {
    calls: Mutex<Vec<Call>>,
    failure: Option<Status>,
}

impl Stub {
    /// A stub that answers every call with `status`.
    pub fn failing(status: Status) -> Self {
        Self {
            failure: Some(status),
            ..Self::default()
        }
    }

    /// The calls received, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("the lock is not poisoned").clone()
    }

    /// The names of the methods called, in order.
    pub fn rpcs(&self) -> Vec<&'static str> {
        self.calls().into_iter().map(|call| call.rpc).collect()
    }

    /// The request of the only call, decoded as `M`.
    pub fn only_request<M: Message + Default>(&self) -> M {
        let calls = self.calls();
        assert_eq!(calls.len(), 1, "{:?}", self.rpcs());
        M::decode(calls[0].request.as_slice()).expect("the request decodes")
    }

    fn answer<Req: Message, Resp>(
        &self,
        rpc: &'static str,
        request: Request<Req>,
        response: Resp,
    ) -> Result<Response<Resp>, Status> {
        self.calls
            .lock()
            .expect("the lock is not poisoned")
            .push(Call {
                rpc,
                request: request.get_ref().encode_to_vec(),
                marker: request.extensions().get::<Marker>().cloned(),
            });
        match &self.failure {
            Some(status) => Err(status.clone()),
            None => Ok(Response::new(response)),
        }
    }
}

#[tonic::async_trait]
impl JobService for Stub {
    async fn submit_job(
        &self,
        request: Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        self.answer("SubmitJob", request, SubmitJobResponse { job: Some(job()) })
    }

    async fn get_job(
        &self,
        request: Request<GetJobRequest>,
    ) -> Result<Response<GetJobResponse>, Status> {
        let job = Job {
            id: request.get_ref().job_id.clone(),
            ..job()
        };
        self.answer("GetJob", request, GetJobResponse { job: Some(job) })
    }

    async fn list_jobs(
        &self,
        request: Request<ListJobsRequest>,
    ) -> Result<Response<ListJobsResponse>, Status> {
        let response = ListJobsResponse {
            jobs: vec![job(), cancelled_job(), job_of_a_newer_state()],
            next_page_token: "page-2".to_owned(),
        };
        self.answer("ListJobs", request, response)
    }

    async fn cancel_job(
        &self,
        request: Request<CancelJobRequest>,
    ) -> Result<Response<CancelJobResponse>, Status> {
        let response = CancelJobResponse {
            job: Some(cancelled_job()),
        };
        self.answer("CancelJob", request, response)
    }

    async fn get_result(
        &self,
        request: Request<GetResultRequest>,
    ) -> Result<Response<GetResultResponse>, Status> {
        let response = GetResultResponse {
            result: Some(result()),
        };
        self.answer("GetResult", request, response)
    }

    async fn get_logs(
        &self,
        request: Request<GetLogsRequest>,
    ) -> Result<Response<GetLogsResponse>, Status> {
        let response = if request.get_ref().stream == LogStream::Stderr as i32 {
            GetLogsResponse {
                state: JobState::Running.into(),
                data: b"\xff\x00a\x80".to_vec(),
                offset: BIG + 2,
                next_offset: BIG + 6,
                gaps: vec![LogGap {
                    offset: BIG,
                    length: 2,
                }],
                eof: true,
                first_byte_at_ms: Some(BIG + 10),
                ..GetLogsResponse::default()
            }
        } else {
            GetLogsResponse {
                state: JobState::Running.into(),
                stdout_tail: b"out\n".to_vec(),
                stderr_tail: b"err\n".to_vec(),
                truncated: true,
                ..GetLogsResponse::default()
            }
        };
        self.answer("GetLogs", request, response)
    }

    async fn list_hosts(
        &self,
        request: Request<ListHostsRequest>,
    ) -> Result<Response<ListHostsResponse>, Status> {
        let response = ListHostsResponse {
            hosts: vec![host("host-a"), Host::default(), cordoned_now()],
        };
        self.answer("ListHosts", request, response)
    }

    async fn cordon_host(
        &self,
        request: Request<CordonHostRequest>,
    ) -> Result<Response<CordonHostResponse>, Status> {
        let response = CordonHostResponse {
            host: Some(host("host-a")),
            already_cordoned: true,
        };
        self.answer("CordonHost", request, response)
    }

    async fn uncordon_host(
        &self,
        request: Request<UncordonHostRequest>,
    ) -> Result<Response<UncordonHostResponse>, Status> {
        let response = UncordonHostResponse {
            host: Some(cordoned_now()),
            lifted: false,
        };
        self.answer("UncordonHost", request, response)
    }

    async fn list_pools(
        &self,
        request: Request<ListPoolsRequest>,
    ) -> Result<Response<ListPoolsResponse>, Status> {
        let response = ListPoolsResponse {
            pools: vec![
                Pool {
                    name: "gpu".to_owned(),
                    kind: "lambdalabs".to_owned(),
                    state: Availability::Unavailable.into(),
                    reasons: vec!["no-offer".to_owned()],
                    memory: Some(memory()),
                    instances: 3,
                    last_tick_ms: Some(BIG),
                },
                Pool {
                    instances: 0,
                    last_tick_ms: Some(0),
                    ..Pool::default()
                },
            ],
        };
        self.answer("ListPools", request, response)
    }

    async fn list_providers(
        &self,
        request: Request<ListProvidersRequest>,
    ) -> Result<Response<ListProvidersResponse>, Status> {
        let response = ListProvidersResponse {
            providers: vec![Provider {
                name: "gpu".to_owned(),
                kind: "lambdalabs".to_owned(),
                state: Availability::Available.into(),
                checks: vec![ProviderCheck {
                    name: "credentials".to_owned(),
                    ok: true,
                    reason: String::new(),
                    note: "key read".to_owned(),
                    checked_at_ms: BIG,
                }],
                machines: vec![Machine {
                    host: "machine-host".to_owned(),
                    state: "joined".to_owned(),
                    offer: "gpu_1x_a100".to_owned(),
                    zone: "zone-1".to_owned(),
                    price_cents_per_hour: Some(0),
                    provider_id: "instance-1".to_owned(),
                }],
            }],
        };
        self.answer("ListProviders", request, response)
    }

    async fn get_usage(
        &self,
        request: Request<GetUsageRequest>,
    ) -> Result<Response<GetUsageResponse>, Status> {
        let page = request.get_ref().page_token.clone();
        let response = match page.as_str() {
            "empty" => GetUsageResponse::default(),
            _ => GetUsageResponse {
                records: vec![UsageRecord {
                    job_id: "usage-job".to_owned(),
                    host_id: "usage-host".to_owned(),
                    queue_wait_ms: 1500,
                    run_ms: BIG,
                    end_state: JobState::Succeeded.into(),
                    ended_reason: ResultReason::Exit.into(),
                    ended_at_ms: BIG,
                    booked: Some(booking("usage-job")),
                    used: Some(ResourceUsage {
                        cpu_usec: 9_000_000,
                        peak_memory_bytes: 1 << 30,
                        disk_bytes: Some(0),
                        gpus: 1,
                    }),
                    whole_host: WholeHostOutcome::Drained.into(),
                    mark_give_ups: 1,
                    estimated_run_ms: Some(0),
                }],
                next_page_token: "usage-page-2".to_owned(),
            },
        };
        self.answer("GetUsage", request, response)
    }
}

/// What the router answered.
#[derive(Debug)]
pub struct Reply {
    /// The status.
    pub status: StatusCode,
    /// The headers.
    pub headers: HeaderMap,
    /// The body.
    pub body: Vec<u8>,
}

impl Reply {
    /// The body as JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("the body is JSON")
    }
}

/// Sends one request through `router` and reads the whole answer.
pub async fn send(router: &Router, method: &str, uri: &str, body: impl Into<Body>) -> Reply {
    let request = HttpRequest::builder()
        .method(method)
        .uri(uri)
        .body(body.into())
        .expect("the request is well formed");
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("the router answers every request");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("the body is read");
    Reply {
        status,
        headers,
        body: body.to_vec(),
    }
}

/// The 400 body of `field` with `reason`.
pub fn invalid(field: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({ "error": "invalid-argument", "field": field, "reason": reason })
}

/// A map of the pairs.
pub fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// A job spec with every field set; its file's contents are empty, as a read returns them.
pub fn spec() -> JobSpec {
    JobSpec {
        job_id: "job-1".to_owned(),
        image: format!(
            "registry.example/app@sha256:{}",
            "0123456789abcdef".repeat(4)
        ),
        command: "/bin/run".to_owned(),
        args: vec!["--fast".to_owned(), "input".to_owned()],
        env: map(&[("B_VAR", "2"), ("A_VAR", "1")]),
        needs: Some(Needs {
            cpu_millis: 1500,
            memory_bytes: BIG,
            disk_bytes: Some(1 << 30),
            gpus: 1,
            gpu_class: "nvidia-a100-sxm4-80gb".to_owned(),
            whole_host: true,
        }),
        constraints: Some(Constraints {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container.into(),
            isa_level: "x86-64-v3".to_owned(),
            accelerator: Some(js_proto::v1::Accelerator {
                vendor: Some("nvidia".to_owned()),
                model: Some("nvidia-a100".to_owned()),
                count: Some(1),
                memory_per_device_gib: Some(80),
                interconnect: Some("nvlink-domain>=8".to_owned()),
                form_factor: Some("sxm".to_owned()),
            }),
        }),
        wall_limit_seconds: Some(3600),
        network: NetworkLevel::AllowList.into(),
        ports: vec![PortForward {
            host_port: 8080,
            container_port: 80,
        }],
        files: vec![JobFile {
            name: "upload-url".to_owned(),
            contents: String::new(),
        }],
        priority: -7,
        labels: map(&[("team", "build"), ("kind", "test")]),
    }
}

/// The times of a job, the first above 2^53.
fn times() -> JobTimes {
    JobTimes {
        queued_at_ms: BIG,
        placed_at_ms: Some(BIG + 1000),
        started_at_ms: Some(BIG + 2000),
        ended_at_ms: Some(BIG + 3000),
    }
}

/// A job with every field set.
pub fn job() -> Job {
    Job {
        id: "job-1".to_owned(),
        spec: Some(spec()),
        state: JobState::FailedToStart.into(),
        start_failure: StartFailureCode::AgentRefused.into(),
        start_failure_detail: "image-fetch-failed".to_owned(),
        host_id: "host-a".to_owned(),
        times: Some(times()),
        host_silent_since_ms: Some(BIG + 4000),
        cancel_requested: true,
        client: Some("client-a".to_owned()),
    }
}

/// A job in a state this version does not know (a number a newer server could send).
pub fn job_of_a_newer_state() -> Job {
    Job {
        id: "job-newer".to_owned(),
        state: 42,
        ..Job::default()
    }
}

/// The resources booked for `job_id` on a host.
pub fn booking(job_id: &str) -> HostBooking {
    HostBooking {
        job_id: job_id.to_owned(),
        cpu_millis: 2000,
        memory_bytes: BIG,
        disk_bytes: 8 << 30,
        gpus: 1,
        host_ports: vec![8080, 9090],
    }
}

/// The memory of a host or a pool.
pub fn memory() -> Memory {
    Memory {
        configured_bytes: 64 << 30,
        schedulable_percent: 90,
        schedulable_bytes: 57_982_058_496,
    }
}

/// A host with every field set.
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
        cordon_expires_at_ms: Some(BIG),
        taint_reason: "post-job-check-failed".to_owned(),
        agent_version: "0.1.0".to_owned(),
    }
}

/// A host whose optional cordon time is present and zero, beside fields at their defaults.
pub fn cordoned_now() -> Host {
    Host {
        id: "host-b".to_owned(),
        cordon_expires_at_ms: Some(0),
        ..Host::default()
    }
}

/// The job as a cancel leaves it: cancelled.
pub fn cancelled_job() -> Job {
    Job {
        state: JobState::Cancelled.into(),
        start_failure: StartFailureCode::Unspecified.into(),
        start_failure_detail: String::new(),
        ..job()
    }
}

/// A result with every field set.
pub fn result() -> JobResult {
    JobResult {
        state: JobState::Failed.into(),
        exit_code: Some(-1),
        reason: ResultReason::Memory.into(),
        stdout_tail: b"\x00\xffout".to_vec(),
        stderr_tail: b"err".to_vec(),
        truncated: true,
        times: Some(times()),
        usage: Some(ResourceUsage {
            cpu_usec: BIG,
            peak_memory_bytes: 1 << 33,
            disk_bytes: Some(4096),
            gpus: 2,
        }),
    }
}

/// The descriptor pool of the test descriptor `file` (under `tests/protos`), with the
/// `google/api` files js-proto carries.
pub fn fixture(file: &str) -> DescriptorPool {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut compiler =
        protox::Compiler::new([root.join("tests/protos"), root.join("../js-proto/proto")])
            .expect("the include directories exist");
    compiler
        .open_file(file)
        .expect("the test descriptor compiles");
    compiler.descriptor_pool()
}

/// The pool of the job service, with the options.
pub fn pool() -> DescriptorPool {
    DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET).expect("the descriptor set decodes")
}

/// A stub and the router over it.
pub fn serve() -> (Arc<Stub>, axum::Router) {
    let stub = Arc::new(Stub::default());
    let router = router(stub.clone());
    (stub, router)
}

/// `template` with every `{variable}` replaced by `x-1`, a valid id of every grammar.
pub fn fill(template: &str) -> String {
    let mut path = String::new();
    let mut rest = template;
    while let Some((before, after)) = rest.split_once('{') {
        path.push_str(before);
        let (_, tail) = after.split_once('}').expect("a variable closes");
        path.push_str("x-1");
        rest = tail;
    }
    path.push_str(rest);
    path
}

/// Decodes a JSON body as the message `name` by the proto3 JSON mapping and converts it to
/// `M`.
pub fn from_json<M: Message + Default>(name: &str, body: &[u8]) -> M {
    let descriptor = pool()
        .get_message_by_name(name)
        .expect("the message is defined");
    let json: serde_json::Value = serde_json::from_slice(body).expect("the body is JSON");
    DynamicMessage::deserialize(descriptor, json)
        .expect("the body decodes")
        .transcode_to::<M>()
        .expect("the message converts")
}

/// The text of a submit body before its command.
pub const HEAD: &str = r#"{"spec":{"jobId":"job-1","command":""#;

/// The text of a submit body after its command.
pub const TAIL: &str = r#""}}"#;

/// A submit body of exactly `size` bytes of valid JSON.
pub fn submit_body(size: usize) -> String {
    let body = format!("{HEAD}{}{TAIL}", "x".repeat(size - HEAD.len() - TAIL.len()));
    assert_eq!(body.len(), size);
    body
}
