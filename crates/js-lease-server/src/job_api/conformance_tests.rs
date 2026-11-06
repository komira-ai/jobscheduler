//! The job-service conformance suite (`js_conformance::jobs`) run against the real service:
//! the `JobApi` over the job tables, behind the real bearer layer, reached by a tonic client
//! over an in-memory stream. The hooks of the harness make the moves only the server itself
//! makes (the placing pass, the agent's heartbeats) on the job store, and move the one
//! manual clock the store stamps ends and retention with and the service reads when it
//! decides a result has expired. Nothing here sleeps or binds a socket.

use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::http::{HeaderValue, Request, header};
use js_conformance::jobs::{self, JobHarness, Refused, Step};
use js_jobs::{JobEnd, JobId, JobState, Output, Retention};
use js_proto::v1::job_service_client::JobServiceClient;
use js_proto::v1::job_service_server::JobServiceServer;
use js_state::ManualClock;
use js_types::HostId;
use tonic::body::Body;
use tonic::transport::Channel;
use tower::Service;

use super::submit::{self, Ctx};
use super::writes_rig::{CEILING, Fixed};
use super::{Bearer, JobApi, grpc};
use crate::adapt::SharedClock;
use crate::agent_api::job_rig::{BOOKED, OneClock, START_MS, tables_on};
use crate::config::{Clients, parse_token};
use crate::job_store::ServerJobs;

/// The token of the client the harness calls as.
const TOKEN: &str = "conformance-token-0123456789abcdefgh";

/// The name the service gives that token.
const CLIENT: &str = "conformance";

/// The host every placed job is booked on.
const HOST: &str = "conformance-host";

/// How long the tables keep an ended job: the shortest window they take.
const RETENTION: Duration = Retention::MIN;

/// The service as the tonic server serves it, with the client's token attached to every
/// request before the bearer layer reads it. The harness hands out a bare client over a
/// channel, which has nowhere to put a header, so the token is set on the server side of
/// the stream.
#[derive(Clone)]
struct Presenting(Bearer<JobServiceServer<JobApi>>);

impl Service<Request<Body>> for Presenting {
    type Response = <Bearer<JobServiceServer<JobApi>> as Service<Request<Body>>>::Response;
    type Error = <Bearer<JobServiceServer<JobApi>> as Service<Request<Body>>>::Error;
    type Future = <Bearer<JobServiceServer<JobApi>> as Service<Request<Body>>>::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Service::<Request<Body>>::poll_ready(&mut self.0, cx)
    }

    fn call(&mut self, mut request: Request<Body>) -> Self::Future {
        let bearer = HeaderValue::from_str(&format!("Bearer {TOKEN}")).expect("ASCII");
        request.headers_mut().insert(header::AUTHORIZATION, bearer);
        self.0.call(request)
    }
}

/// The real job service with its hooks.
struct Real {
    jobs: Arc<ServerJobs>,
    clock: Arc<ManualClock>,
    channel: Channel,
}

impl Real {
    /// A service over fresh tables, one clock, and one client.
    fn new() -> Real {
        let clock = Arc::new(ManualClock::default());
        clock.advance(Duration::from_millis(START_MS));
        let retention = Retention::new(RETENTION).expect("the shortest window is a window");
        let (_, tables) = tables_on(Arc::clone(&clock), retention);
        let jobs = Arc::new(tables);
        let shared = SharedClock::new(OneClock(Arc::clone(&clock)));
        let token = parse_token(TOKEN.as_bytes()).expect("a valid token");
        let name = js_jobs::ClientName::parse(CLIENT).expect("a client name");
        let mut clients = Clients::only(
            &parse_token(b"unused-token-0123456789abcdefghijkl").unwrap(),
            "--token-file",
        );
        assert_eq!(
            clients.add(name, &token, "--client-token".to_owned()),
            Ok(())
        );
        let ctx: Ctx = submit::Ctx::served(CEILING, Fixed::new(true));
        let api = Arc::new(JobApi::new(Arc::clone(&jobs), shared).with_submit(ctx));
        let channel = jobs::channel_to(Presenting(grpc(api, clients)));
        Real {
            jobs,
            clock,
            channel,
        }
    }
}

impl JobHarness for Real {
    fn client(&mut self) -> JobServiceClient<Channel> {
        JobServiceClient::new(self.channel.clone())
    }

    fn client_name(&self) -> String {
        CLIENT.to_owned()
    }

    fn retention(&self) -> Duration {
        self.jobs.retention().window()
    }

    /// Every move is the store's to take or refuse: the harness reads the job's state only to
    /// name it as the move's start, so the store's own checks (`place` takes a queued job,
    /// `transition` and `end_job` the edges of its state machine) decide. An end of a job
    /// that has already ended is the one move the store answers without an error: it writes
    /// nothing and returns the stored record (an agent's repeated result is a duplicate), so
    /// the harness reports that as refused.
    fn step(&mut self, job: &str, step: Step) -> Result<(), Refused> {
        let id = JobId::parse(job).map_err(|_| Refused)?;
        let state = self.jobs.get_job(&id).ok().flatten().ok_or(Refused)?.state;
        let moved = match step {
            Step::Place => {
                let host = HostId::new(HOST).expect("a host id");
                self.jobs.place(&id, host, BOOKED).map(|_| ())
            }
            Step::Start => self
                .jobs
                .transition(&id, state, JobState::Starting)
                .map(|_| ()),
            Step::Run => self
                .jobs
                .transition(&id, state, JobState::Running)
                .map(|_| ()),
            Step::Exit(_) if state.is_ended() => return Err(Refused),
            Step::Exit(exit_code) => {
                let end = JobEnd::Exited {
                    exit_code,
                    output: Output::default(),
                };
                self.jobs.end_job(&id, end).map(|_| ())
            }
        };
        moved.map_err(|_| Refused)
    }

    fn advance(&mut self, by: Duration) {
        self.clock.advance(by);
    }

    fn sweep(&mut self) {
        self.jobs.tick().expect("the retention sweep runs");
    }
}

/// Asserts that every scenario of the suite passes against the real job service, each on
/// fresh tables. It proves the service answers what the suite holds every implementation
/// to: idempotent submits, the state machine's moves, cancel, results, retention and the
/// stable error codes.
#[tokio::test]
async fn the_real_job_service_passes_every_conformance_scenario() {
    let report = jobs::run(|| std::future::ready(Real::new())).await;
    let verdicts: Vec<_> = report.results.iter().collect();
    assert!(report.failed().is_empty(), "{verdicts:#?}");
}
