//! What the gateway's tests share: a stub [`JobService`] that records each call, a request
//! helper over the router (tower `oneshot`, no socket), fully populated sample messages, and
//! the test descriptors under `tests/protos`.

#![allow(dead_code, reason = "each test binary uses a part of the module")]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request as HttpRequest, StatusCode};
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{
    CancelJobRequest, CancelJobResponse, Constraints, CordonHostRequest, CordonHostResponse,
    ExecMode, GetJobRequest, GetJobResponse, GetLogsRequest, GetLogsResponse, GetResultRequest,
    GetResultResponse, GetUsageRequest, GetUsageResponse, Job, JobFile, JobResult, JobSpec,
    JobState, JobTimes, ListHostsRequest, ListHostsResponse, ListJobsRequest, ListJobsResponse,
    ListPoolsRequest, ListPoolsResponse, ListProvidersRequest, ListProvidersResponse, Needs,
    NetworkLevel, PortForward, ResourceUsage, ResultReason, StartFailureCode, SubmitJobRequest,
    SubmitJobResponse, UncordonHostRequest, UncordonHostResponse,
};
use prost::Message;
use prost_reflect::DescriptorPool;
use tonic::{Request, Response, Status};
use tower::ServiceExt;

/// A time in milliseconds since the Unix epoch above 2^53: a JSON number would round it.
pub const BIG: u64 = (1 << 53) + 1;

/// One call as the stub received it: the method's name and the encoded request.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// The method's name in the service, such as `GetJob`.
    pub rpc: &'static str,
    /// The request message, encoded.
    pub request: Vec<u8>,
}

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
        self.answer("GetJob", request, GetJobResponse { job: Some(job()) })
    }

    async fn list_jobs(
        &self,
        request: Request<ListJobsRequest>,
    ) -> Result<Response<ListJobsResponse>, Status> {
        let response = ListJobsResponse {
            jobs: vec![job(), cancelled_job()],
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
        let response = GetLogsResponse {
            state: JobState::Running.into(),
            stdout_tail: b"out\n".to_vec(),
            stderr_tail: b"err\n".to_vec(),
            truncated: true,
        };
        self.answer("GetLogs", request, response)
    }

    async fn list_hosts(
        &self,
        request: Request<ListHostsRequest>,
    ) -> Result<Response<ListHostsResponse>, Status> {
        self.answer("ListHosts", request, ListHostsResponse::default())
    }

    async fn cordon_host(
        &self,
        request: Request<CordonHostRequest>,
    ) -> Result<Response<CordonHostResponse>, Status> {
        self.answer("CordonHost", request, CordonHostResponse::default())
    }

    async fn uncordon_host(
        &self,
        request: Request<UncordonHostRequest>,
    ) -> Result<Response<UncordonHostResponse>, Status> {
        self.answer("UncordonHost", request, UncordonHostResponse::default())
    }

    async fn list_pools(
        &self,
        request: Request<ListPoolsRequest>,
    ) -> Result<Response<ListPoolsResponse>, Status> {
        self.answer("ListPools", request, ListPoolsResponse::default())
    }

    async fn list_providers(
        &self,
        request: Request<ListProvidersRequest>,
    ) -> Result<Response<ListProvidersResponse>, Status> {
        self.answer("ListProviders", request, ListProvidersResponse::default())
    }

    async fn get_usage(
        &self,
        request: Request<GetUsageRequest>,
    ) -> Result<Response<GetUsageResponse>, Status> {
        self.answer("GetUsage", request, GetUsageResponse::default())
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
