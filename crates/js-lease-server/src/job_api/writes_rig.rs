//! The rig of the write calls' tests (`submit_tests.rs`, `cancel_tests.rs`): one job service
//! over a set of tables, with a served submit, reached over both transports in one process
//! (gRPC through a tonic client over an in-memory duplex stream, the gateway through its
//! router's `oneshot`), and admission sources a test controls. Nothing here sleeps or binds
//! a socket.
//!
//! An answer is read as the gateway would say it ([`Said`]): a refusal over gRPC is turned
//! into the gateway's row for its status, so a test asserts one shape whichever transport it
//! used, and a test that needs the gRPC code reads it from [`Writes::grpc`].

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request as HttpRequest, StatusCode, header};
use js_api_gateway::Answer;
use js_jobs::interfere::Interfere;
use js_jobs::{Isolation, JobId, MAX_PAGE, Retention};
use js_proto::v1::{
    self, CancelJobRequest, CancelJobResponse, SubmitJobRequest, SubmitJobResponse,
};
use js_state::ManualClock;
use js_state::fake::Fake;
use js_types::Requirements;
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::Value;
use tonic::client::Grpc;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Request, Status};
use tonic_prost::ProstCodec;
use tonic_prost::prost::Message;
use tower::ServiceExt;

use super::read_tests::{from_json, json_of};
use super::submit::{self, Admission};
use super::tests::{SERVICE, TOKEN, connect, token};
use super::{JobApi, gateway, grpc};
use crate::adapt::SharedClock;
use crate::admit::{Admit, JobAdmission};
use crate::agent_api::job_rig::{OneClock, Rig, START_MS, tables_on};
use crate::config::LiveClients;
use crate::fence::Fence;
use crate::job_expiry::JobLimits;
use crate::job_store::ServerJobs;

/// The ceiling the tests' service holds a wall limit to: one hour.
pub(super) const CEILING: Duration = Duration::from_secs(3600);

/// The two transports of the job service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Via {
    /// gRPC.
    Grpc,
    /// The HTTP/JSON gateway.
    Json,
}

impl Via {
    /// Both transports, gRPC first.
    pub(super) const BOTH: [Via; 2] = [Via::Grpc, Via::Json];
}

/// What a call answered, as the gateway says it: the job, or the status and JSON body of a
/// refusal.
pub(super) type Said = Result<v1::Job, (StatusCode, Value)>;

/// An admission source that answers `true` (something could serve every job) or `false`
/// (nothing could), and counts how often it was asked.
pub(super) struct Fixed {
    answer: bool,
    asked: AtomicUsize,
}

impl Fixed {
    /// A source that always answers `answer`.
    pub(super) fn new(answer: bool) -> Arc<Fixed> {
        Arc::new(Fixed {
            answer,
            asked: AtomicUsize::new(0),
        })
    }

    /// How often it was asked.
    pub(super) fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

#[tonic::async_trait]
impl Admission for Fixed {
    async fn can_ever_serve(&self, _: &Requirements, _: Isolation) -> bool {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.answer
    }
}

/// The job service over `jobs`, reading `clock`, its submit served with [`CEILING`] and
/// `admission`, and its two transports.
pub(super) struct Writes {
    /// The tables the service writes.
    pub(super) jobs: Arc<ServerJobs>,
    /// The service's fence, which a fenced store trips.
    pub(super) fence: Fence,
    channel: Channel,
    router: Router,
}

impl Writes {
    /// The service over `jobs` and `clock`, admitting through `admission`.
    pub(super) async fn new(
        jobs: Arc<ServerJobs>,
        clock: SharedClock,
        admission: Arc<dyn Admission>,
    ) -> Writes {
        Writes::with_clients(jobs, clock, admission, token()).await
    }

    /// The service over `jobs` and `clock`, admitting through `admission`, behind the bearer
    /// layer of `clients`.
    pub(super) async fn with_clients(
        jobs: Arc<ServerJobs>,
        clock: SharedClock,
        admission: Arc<dyn Admission>,
        clients: LiveClients,
    ) -> Writes {
        let submit = submit::Ctx::served(CEILING, admission);
        Writes::over(JobApi::new(jobs, clock).with_submit(submit), clients).await
    }

    /// The service `api`, whatever its groups' contexts, behind the bearer layer of
    /// `clients`. The rig's fence is `api`'s.
    pub(super) async fn over(api: JobApi, clients: LiveClients) -> Writes {
        let jobs = Arc::clone(&api.jobs);
        let fence = api.fence.clone();
        let api = Arc::new(api);
        let channel = connect(grpc(Arc::clone(&api), clients.clone())).await;
        let router = gateway(api, clients);
        Writes {
            jobs,
            fence,
            channel,
            router,
        }
    }

    /// The answer of `method` to `request` over gRPC, with the token.
    pub(super) async fn grpc<Req, Resp>(&self, method: &str, request: Req) -> Result<Resp, Status>
    where
        Req: Message + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
    {
        self.grpc_as(TOKEN, method, request).await
    }

    /// The answer of `method` to `request` over gRPC, with the token `token`.
    pub(super) async fn grpc_as<Req, Resp>(
        &self,
        token: &str,
        method: &str,
        request: Req,
    ) -> Result<Resp, Status>
    where
        Req: Message + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
    {
        let mut grpc = Grpc::new(self.channel.clone());
        grpc.ready().await.expect("the channel is ready");
        let path = PathAndQuery::try_from(format!("/{SERVICE}/{method}")).expect("a path");
        let mut request = Request::new(request);
        let bearer = MetadataValue::try_from(format!("Bearer {token}")).expect("ASCII");
        request.metadata_mut().insert("authorization", bearer);
        let codec = ProstCodec::<Req, Resp>::default();
        let response = grpc.unary(request, path, codec).await;
        response.map(tonic::Response::into_inner)
    }

    /// The status and body of the gateway's answer to `POST uri` with the JSON `body`, with
    /// the token.
    pub(super) async fn post(&self, uri: &str, body: &Value) -> (StatusCode, Vec<u8>) {
        self.http_as(TOKEN, "POST", uri, Some(body)).await
    }

    /// The status and body of the gateway's answer to `method uri`, with the token `token`
    /// and, when given, the JSON `body`.
    pub(super) async fn http_as(
        &self,
        token: &str,
        method: &str,
        uri: &str,
        body: Option<&Value>,
    ) -> (StatusCode, Vec<u8>) {
        let request = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
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

    /// `SubmitJob` of `spec` over gRPC.
    pub(super) async fn submit_grpc(&self, spec: &v1::JobSpec) -> Result<v1::Job, Status> {
        let request = SubmitJobRequest {
            spec: Some(spec.clone()),
        };
        let answer: SubmitJobResponse = self.grpc("SubmitJob", request).await?;
        Ok(answer.job.expect("an answer carries the job"))
    }

    /// `SubmitJob` of `spec` over `via`. Over the gateway a job is answered 201.
    pub(super) async fn submit(&self, via: Via, spec: &v1::JobSpec) -> Said {
        match via {
            Via::Grpc => self.submit_grpc(spec).await.map_err(|status| said(&status)),
            Via::Json => {
                let request = SubmitJobRequest {
                    spec: Some(spec.clone()),
                };
                let body = to_json("SubmitJobRequest", &request);
                let (status, body) = self.post("/v1/jobs", &body).await;
                match status {
                    StatusCode::CREATED => {
                        let name = "jobscheduler.v1.SubmitJobResponse";
                        let answer: SubmitJobResponse = from_json(name, &body);
                        Ok(answer.job.expect("an answer carries the job"))
                    }
                    status => Err((status, json_of(&body))),
                }
            }
        }
    }

    /// `CancelJob` of `id` over gRPC.
    pub(super) async fn cancel_grpc(&self, id: &str) -> Result<v1::Job, Status> {
        let request = CancelJobRequest {
            job_id: id.to_owned(),
        };
        let answer: CancelJobResponse = self.grpc("CancelJob", request).await?;
        Ok(answer.job.expect("an answer carries the job"))
    }

    /// `CancelJob` of `id` over `via`. Over the gateway a job is answered 200.
    pub(super) async fn cancel(&self, via: Via, id: &str) -> Said {
        match via {
            Via::Grpc => self.cancel_grpc(id).await.map_err(|status| said(&status)),
            Via::Json => {
                let uri = format!("/v1/jobs/{id}:cancel");
                let (status, body) = self.post(&uri, &Value::Object(Default::default())).await;
                match status {
                    StatusCode::OK => {
                        let name = "jobscheduler.v1.CancelJobResponse";
                        let answer: CancelJobResponse = from_json(name, &body);
                        Ok(answer.job.expect("an answer carries the job"))
                    }
                    status => Err((status, json_of(&body))),
                }
            }
        }
    }
}

/// `status` as the gateway answers it: its status code and JSON body.
pub(super) fn said(status: &Status) -> (StatusCode, Value) {
    let row = Answer::of_status(status);
    (row.status(), row.body())
}

/// `message`, the message `name` of the job service's package, by the proto3 JSON mapping,
/// as a client of the gateway sends it.
pub(super) fn to_json<M: Message>(name: &str, message: &M) -> Value {
    let pool = DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET).expect("the set decodes");
    let descriptor = pool
        .get_message_by_name(&format!("jobscheduler.v1.{name}"))
        .expect("the message is defined");
    let mut dynamic = DynamicMessage::new(descriptor);
    dynamic
        .transcode_from(message)
        .expect("the message converts");
    serde_json::to_value(&dynamic).expect("the message is JSON")
}

/// The image every submitted job runs, pinned by digest.
pub(super) fn image() -> String {
    format!("team/app@sha256:{}", "a".repeat(64))
}

/// A job `id` (empty: none) as a client submits it: `run --fast` from [`image`], with 1.5
/// cores and 1 GiB, for at most ten minutes, and nothing else.
pub(super) fn wire(id: &str) -> v1::JobSpec {
    v1::JobSpec {
        job_id: id.to_owned(),
        image: image(),
        command: "run".to_owned(),
        args: vec!["--fast".to_owned()],
        needs: Some(v1::Needs {
            cpu_millis: 1500,
            memory_bytes: 1 << 30,
            ..v1::Needs::default()
        }),
        wall_limit_seconds: Some(600),
        ..v1::JobSpec::default()
    }
}

/// Limits long enough that no job of a test runs out of time.
pub(super) const NEVER: JobLimits = JobLimits {
    max_queue_wait: Duration::from_secs(365 * 24 * 3600),
    start_timeout: Duration::from_secs(365 * 24 * 3600),
};

/// The service over the agent rig's tables and clock, admitting over its registry's hosts
/// and no launching provider.
pub(super) async fn served(rig: &Rig) -> Writes {
    let admission = JobAdmission::new(Admit::default(), rig.agents.registry.clone());
    Writes::new(
        Arc::clone(&rig.jobs),
        rig.shared.clone(),
        Arc::new(admission),
    )
    .await
}

/// The service over tables of a store that fails what it is told to, admitting every job
/// through `admission`; the store with it.
pub(super) async fn over_fake(admission: Arc<Fixed>) -> (Arc<Interfere<Fake>>, Writes) {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (store, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let shared = SharedClock::new(OneClock(clock));
    let writes = Writes::new(Arc::new(jobs), shared, admission).await;
    (store, writes)
}

/// How many jobs the tables hold.
pub(super) fn rows(jobs: &ServerJobs) -> usize {
    jobs.list_jobs(None, None, MAX_PAGE).unwrap().jobs.len()
}

/// The id `text`.
pub(super) fn id(text: &str) -> JobId {
    JobId::parse(text).unwrap()
}
