//! The job-service conformance suite (`js_conformance::jobs`) run against the real service:
//! the `JobApi` over the job tables, behind the real bearer layer, reached by a tonic client
//! over an in-memory stream. The hooks of the harness make the moves only the server itself
//! makes (the placing pass, the agent's heartbeats) on the job store, and move the one
//! manual clock the store stamps ends and retention with and the service reads when it
//! decides a result has expired. Nothing here sleeps or binds a socket.
//!
//! The service is built as a test builds it until the release that accepts a retry policy and
//! a completion callback: its submit takes both. A host that is lost goes through the job
//! pass's own loss path ([`job_loss::lose_absent`], an observer that reads every host
//! absent), a start that times out through [`job_retry::end_or_retry`] as the start timeout
//! of [`crate::job_expiry`] does, and a callback is claimed from the store and answered
//! delivered as a delivery thread does; the allocation of an attempt that retried is freed as
//! the host's confirmation frees it, so the next attempt can be placed.

use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::http::{HeaderValue, Request, header};
use js_callback::AddressRule;
use js_conformance::jobs::{self, JobHarness, Refused, Step};
use js_jobs::{InstanceId, JobEnd, JobId, JobRecord, JobState, Output, Retention, StartFailure};
use js_proto::v1::job_service_client::JobServiceClient;
use js_proto::v1::job_service_server::JobServiceServer;
use js_state::ManualClock;
use js_types::{CallbackAnswer, HostId};
use tonic::body::Body;
use tonic::transport::Channel;
use tower::Service;

use super::submit::{self, Ctx};
use super::writes_rig::{CEILING, Fixed};
use super::{Bearer, JobApi, grpc};
use crate::agent_api::job_rig::{BOOKED, OneClock, START_MS, tables_on};
use crate::clock::SharedClock;
use crate::config::{Clients, Entry, LiveClients, Role, parse_token};
use crate::job_alloc::{self, Freed};
use crate::job_loss::{self, Observe, Seen};
use crate::job_retry::{self, Settled};
use crate::job_store::ServerJobs;
use crate::metrics::NoMetrics;

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

/// The provider that reads every host absent: what a host that is lost is to the job pass.
struct Gone;

impl Observe for Gone {
    fn observe(&self, _: &HostId) -> Result<Seen, String> {
        Ok(Seen::Absent)
    }
}

/// The name the harness claims callbacks as.
const DELIVERER: &str = "conformance-deliverer";

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
        let entry = Entry {
            name,
            role: Role::Client,
            place: format!("--client-token {CLIENT}"),
            file: std::path::PathBuf::new(),
        };
        assert_eq!(clients.add(entry, &token), Ok(()));
        let ctx: Ctx = submit::Ctx::served(CEILING, Fixed::new(true))
            .accepting_retry()
            .accepting_callbacks(AddressRule::new([]));
        let api = Arc::new(JobApi::new(Arc::clone(&jobs), shared).with_submit(ctx));
        let channel = jobs::channel_to(Presenting(grpc(api, LiveClients::new(clients))));
        Real {
            jobs,
            clock,
            channel,
        }
    }
}

impl Real {
    /// Loses the host of `job`, which is placed, starting or running, as the job pass does when
    /// the provider reads the host absent: the job is retried or ends lost.
    fn host_lost(&self, job: &JobRecord) -> Result<(), Refused> {
        let live = matches!(
            job.state,
            JobState::Placed | JobState::Starting | JobState::Running
        );
        if !live {
            return Err(Refused);
        }
        job_loss::lose_absent(&self.jobs, &Gone, &NoMetrics).expect("the job pass runs");
        Ok(())
    }

    /// Times out the start of `job`, which is placed or starting, as the expiry does: the job
    /// is retried, and then its allocation freed as its host's confirmation frees it, or ends
    /// failed to start.
    fn start_failed(&self, job: &JobRecord) -> Result<(), Refused> {
        if !matches!(job.state, JobState::Placed | JobState::Starting) {
            return Err(Refused);
        }
        let end = JobEnd::FailedToStart(StartFailure::StartTimeout);
        let settled = job_retry::end_or_retry(&self.jobs, job, end, &NoMetrics);
        if let Settled::Retried(_) = settled.expect("the attempt is ended or retried") {
            job_alloc::free_allocation(&self.jobs, &job.id, Freed::HostConfirmed)
                .expect("the retried attempt's allocation is freed");
        }
        Ok(())
    }

    /// One delivery try of the callback of `job`, answered 2xx: the due callback is claimed
    /// and answered delivered. Refused when the callback due is another job's, or none is.
    fn deliver(&self, job: &JobId) -> Result<(), Refused> {
        let now = self.jobs.now().expect("the store reads its clock");
        let by = InstanceId::new(DELIVERER);
        let claims = self.jobs.claim_callbacks(&by, now, 1);
        let claims = claims.expect("the due callbacks are claimed");
        let claim = claims.into_iter().find(|claim| claim.job == *job);
        let claim = claim.ok_or(Refused)?;
        let answered = self
            .jobs
            .answer_callback(claim, CallbackAnswer::Delivered, now);
        answered.expect("the claim is answered");
        Ok(())
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
        let record = self.jobs.get_job(&id).ok().flatten().ok_or(Refused)?;
        let state = record.state;
        let moved = match step {
            Step::Place => {
                let host = HostId::new(HOST).expect("a host id");
                let now = self.jobs.now().expect("the store reads its clock");
                let waits = record.attempt.retry_not_before();
                if waits.is_some_and(|until| now < until) {
                    return Err(Refused);
                }
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
            Step::HostLost => return self.host_lost(&record),
            Step::StartFailed => return self.start_failed(&record),
            Step::DeliverCallback => return self.deliver(&id),
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
