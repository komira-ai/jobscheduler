//! Runs the job-service suite on the reference service and on each broken one, and holds
//! the reference service itself to the answers of the real one.

use std::future::{Ready, ready};
use std::task::{Context, Poll};
use std::time::Duration;

use js_conformance::Verdict;
use js_conformance::jobs::{
    self, Defect, JobHarness, JobsReport, ReferenceHarness, ReferenceJobs, Refused, Scenario, Step,
    channel_to,
};
use js_proto::v1::job_service_client::JobServiceClient;
use js_proto::v1::job_service_server::{JobService, JobServiceServer};
use js_proto::v1::{self, JobState};
use tonic::body::Body;
use tonic::codegen::{Service, http};
use tonic::transport::Channel;
use tonic::{Code, Request, Response, Status};

fn failed_by(defect: Defect) -> Vec<Scenario> {
    let report = block_on(jobs::run(|| ReferenceJobs::broken_harness(defect)));
    report.failed()
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .map(|runtime| runtime.block_on(future))
        .unwrap_or_else(|error| panic!("the runtime builds: {error}"))
}

#[tokio::test]
async fn the_reference_service_passes_every_scenario() {
    let report = jobs::run(ReferenceJobs::harness).await;
    assert_eq!(report.results.len(), Scenario::ALL.len());
    let verdicts: Vec<_> = report.results.iter().map(|(_, verdict)| verdict).collect();
    assert!(
        verdicts.iter().all(|verdict| **verdict == Verdict::Passed),
        "{verdicts:?}"
    );
    assert!(report.failed().is_empty());
    let order: Vec<_> = report
        .results
        .iter()
        .map(|(scenario, _)| *scenario)
        .collect();
    assert_eq!(order, Scenario::ALL);
}

#[test]
fn a_resubmit_refused_as_a_conflict_fails_submit_is_idempotent() {
    assert_eq!(
        failed_by(Defect::ResubmitConflicts),
        [Scenario::SubmitIsIdempotent]
    );
}

#[test]
fn a_conflicting_submit_that_overwrites_fails_same_id_other_spec_is_refused() {
    assert_eq!(
        failed_by(Defect::ConflictOverwrites),
        [Scenario::SameIdOtherSpecIsRefused]
    );
}

#[test]
fn a_nonzero_exit_that_succeeds_fails_the_state_and_result_scenarios() {
    assert_eq!(
        failed_by(Defect::ExitCodeIgnored),
        [
            Scenario::StatesMoveInOrder,
            Scenario::ResultIsTheRecordedOne
        ]
    );
}

#[test]
fn a_state_machine_that_takes_any_step_fails_illegal_moves_are_refused() {
    assert_eq!(
        failed_by(Defect::AnyStepIsTaken),
        [Scenario::IllegalMovesAreRefused]
    );
}

#[test]
fn a_cancel_that_ends_a_running_job_fails_cancel_follows_the_state() {
    assert_eq!(
        failed_by(Defect::CancelEndsRunning),
        [Scenario::CancelFollowsTheState]
    );
}

#[test]
fn an_empty_result_before_the_end_fails_result_waits_for_the_end() {
    assert_eq!(
        failed_by(Defect::ResultBeforeEnd),
        [Scenario::ResultWaitsForTheEnd]
    );
}

#[test]
fn a_result_without_its_exit_code_fails_result_is_the_recorded_one() {
    assert_eq!(
        failed_by(Defect::ResultLosesExitCode),
        [Scenario::ResultIsTheRecordedOne]
    );
}

#[test]
fn a_result_that_expires_early_fails_retention_holds_until_its_bound() {
    assert_eq!(
        failed_by(Defect::ResultExpiresEarly),
        [Scenario::RetentionHoldsUntilItsBound]
    );
}

#[test]
fn a_sweep_that_deletes_nothing_fails_retention_holds_until_its_bound() {
    assert_eq!(
        failed_by(Defect::SweepKeepsJobs),
        [Scenario::RetentionHoldsUntilItsBound]
    );
}

#[test]
fn a_zero_wall_limit_that_is_accepted_fails_errors_have_stable_codes() {
    assert_eq!(
        failed_by(Defect::WallZeroAccepted),
        [Scenario::ErrorsHaveStableCodes]
    );
}

#[test]
fn a_job_that_records_no_client_fails_client_is_recorded() {
    assert_eq!(
        failed_by(Defect::ClientNotRecorded),
        [Scenario::ClientIsRecorded]
    );
}

#[test]
fn an_unknown_job_answered_as_invalid_fails_the_error_and_retention_scenarios() {
    assert_eq!(
        failed_by(Defect::UnknownJobIsInvalid),
        [
            Scenario::RetentionHoldsUntilItsBound,
            Scenario::ErrorsHaveStableCodes
        ]
    );
}

/// A harness whose client talks to `service` and whose hooks drive a reference service the
/// client never reaches.
struct Stubbed {
    jobs: ReferenceJobs,
    client: JobServiceClient<Channel>,
}

impl Stubbed {
    fn over<S>(service: S) -> Stubbed
    where
        S: Service<http::Request<Body>, Response = http::Response<Body>> + Clone + Send + 'static,
        S::Future: Send,
        S::Error: Into<tonic::codegen::StdError> + Send + 'static,
    {
        Stubbed {
            jobs: ReferenceJobs::new("stub"),
            client: JobServiceClient::new(channel_to(service)),
        }
    }
}

impl JobHarness for Stubbed {
    fn client(&mut self) -> JobServiceClient<Channel> {
        self.client.clone()
    }
    fn client_name(&self) -> String {
        self.jobs.client_name()
    }
    fn retention(&self) -> Duration {
        self.jobs.retention()
    }
    fn step(&mut self, job: &str, step: Step) -> Result<(), Refused> {
        self.jobs.step(job, step)
    }
    fn advance(&mut self, by: Duration) {
        self.jobs.advance(by);
    }
    fn sweep(&mut self) {
        self.jobs.sweep();
    }
}

/// A raw tower service that answers every call `UNIMPLEMENTED`, as a fake that is not given
/// the call does.
#[derive(Clone)]
struct Unserved;

impl Service<http::Request<Body>> for Unserved {
    type Response = http::Response<Body>;
    type Error = std::convert::Infallible;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: http::Request<Body>) -> Self::Future {
        ready(Ok(Status::unimplemented("no such call").into_http()))
    }
}

/// A service that answers every call of the suite with an empty message.
#[derive(Clone)]
struct Hollow;

macro_rules! hollow {
    ($($name:ident: $request:ident -> $response:ident),*) => {
        #[tonic::async_trait]
        impl JobService for Hollow {
            async fn submit_job(
                &self,
                _: Request<v1::SubmitJobRequest>,
            ) -> Result<Response<v1::SubmitJobResponse>, Status> {
                Ok(Response::new(v1::SubmitJobResponse::default()))
            }
            async fn get_job(
                &self,
                _: Request<v1::GetJobRequest>,
            ) -> Result<Response<v1::GetJobResponse>, Status> {
                Ok(Response::new(v1::GetJobResponse::default()))
            }
            async fn list_jobs(
                &self,
                _: Request<v1::ListJobsRequest>,
            ) -> Result<Response<v1::ListJobsResponse>, Status> {
                Ok(Response::new(v1::ListJobsResponse::default()))
            }
            async fn cancel_job(
                &self,
                _: Request<v1::CancelJobRequest>,
            ) -> Result<Response<v1::CancelJobResponse>, Status> {
                Ok(Response::new(v1::CancelJobResponse::default()))
            }
            async fn get_result(
                &self,
                _: Request<v1::GetResultRequest>,
            ) -> Result<Response<v1::GetResultResponse>, Status> {
                Ok(Response::new(v1::GetResultResponse::default()))
            }
            $(
                async fn $name(
                    &self,
                    _: Request<v1::$request>,
                ) -> Result<Response<v1::$response>, Status> {
                    Err(Status::unimplemented("unimplemented"))
                }
            )*
        }
    };
}

hollow!(
    get_logs: GetLogsRequest -> GetLogsResponse,
    list_hosts: ListHostsRequest -> ListHostsResponse,
    cordon_host: CordonHostRequest -> CordonHostResponse,
    uncordon_host: UncordonHostRequest -> UncordonHostResponse,
    list_pools: ListPoolsRequest -> ListPoolsResponse,
    list_providers: ListProvidersRequest -> ListProvidersResponse,
    get_usage: GetUsageRequest -> GetUsageResponse
);

async fn every_scenario_fails(mut harness: Stubbed) -> JobsReport {
    let mut results = Vec::new();
    for scenario in Scenario::ALL {
        results.push((scenario, scenario.run(&mut harness).await));
    }
    JobsReport { results }
}

#[tokio::test]
async fn a_service_that_answers_every_call_unimplemented_fails_every_scenario() {
    let report = every_scenario_fails(Stubbed::over(Unserved)).await;
    assert_eq!(report.failed(), Scenario::ALL);
    let (_, verdict) = &report.results[0];
    let Verdict::Failed(text) = verdict else {
        panic!("the first scenario failed");
    };
    assert!(text.contains("Unimplemented"), "{text}");
}

#[tokio::test]
async fn a_service_that_answers_every_call_with_nothing_fails_every_scenario() {
    let report = every_scenario_fails(Stubbed::over(JobServiceServer::new(Hollow))).await;
    assert_eq!(report.failed(), Scenario::ALL);
    let Verdict::Failed(text) = &report.results[0].1 else {
        panic!("the first scenario failed");
    };
    assert!(text.contains("the answer holds nothing"), "{text}");
}

#[tokio::test]
async fn a_fake_that_serves_the_reference_service_passes_the_suite() {
    let report = jobs::run(|| async {
        let jobs = ReferenceJobs::new("other-client");
        let client = JobServiceClient::new(channel_to(JobServiceServer::new(jobs.clone())));
        Shared { jobs, client }
    })
    .await;
    assert!(report.failed().is_empty(), "{report:?}");
}

/// A harness assembled as another crate's fake would: one clone of the service served to
/// the client, another used for the hooks.
struct Shared {
    jobs: ReferenceJobs,
    client: JobServiceClient<Channel>,
}

impl JobHarness for Shared {
    fn client(&mut self) -> JobServiceClient<Channel> {
        self.client.clone()
    }
    fn client_name(&self) -> String {
        self.jobs.client_name()
    }
    fn retention(&self) -> Duration {
        self.jobs.retention()
    }
    fn step(&mut self, job: &str, step: Step) -> Result<(), Refused> {
        self.jobs.step(job, step)
    }
    fn advance(&mut self, by: Duration) {
        self.jobs.advance(by);
    }
    fn sweep(&mut self) {
        self.jobs.sweep();
    }
}

async fn harness() -> ReferenceHarness {
    ReferenceJobs::harness().await
}

#[tokio::test]
async fn the_hooks_report_the_retention_and_the_client_the_service_was_given() {
    let harness = harness().await;
    assert_eq!(harness.retention(), Duration::from_secs(60));
    assert_eq!(harness.client_name(), "conformance-client");
    harness.jobs().set_retention(Duration::from_secs(120));
    assert_eq!(harness.retention(), Duration::from_secs(120));
    assert_eq!(ReferenceJobs::new("alice").client_name(), "alice");
}

#[tokio::test]
async fn a_longer_retention_set_before_the_end_holds_the_result_for_it() {
    let mut harness = harness().await;
    harness.jobs().set_retention(Duration::from_secs(600));
    let mut client = harness.client();
    client
        .submit_job(jobs::submit(jobs::spec("long")))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    for step in [Step::Place, Step::Start, Step::Run, Step::Exit(0)] {
        assert_eq!(harness.step("long", step), Ok(()));
    }
    harness.advance(Duration::from_secs(599));
    let request = v1::GetResultRequest {
        job_id: "long".to_string(),
    };
    assert!(client.get_result(request.clone()).await.is_ok());
    harness.advance(Duration::from_secs(1));
    let status = client.get_result(request).await.err();
    assert_eq!(
        status.map(|s| (s.code(), s.message().to_string())),
        Some((Code::NotFound, "expired".to_string()))
    );
}

#[tokio::test]
async fn a_submit_without_an_id_mints_a_distinct_id_each_time() {
    let mut harness = harness().await;
    let mut client = harness.client();
    let blank = jobs::spec("");
    let first = client
        .submit_job(jobs::submit(blank.clone()))
        .await
        .map(|r| r.into_inner().job);
    let second = client
        .submit_job(jobs::submit(blank))
        .await
        .map(|r| r.into_inner().job);
    let ids: Vec<_> = [first, second]
        .into_iter()
        .map(|answer| answer.ok().flatten().map(|job| job.id))
        .collect();
    assert_eq!(
        ids,
        [Some(format!("{:032x}", 1)), Some(format!("{:032x}", 2))]
    );
}

#[tokio::test]
async fn an_id_over_63_bytes_or_with_a_byte_outside_the_alphabet_is_malformed() {
    let mut harness = harness().await;
    let mut client = harness.client();
    for id in ["x".repeat(64), "é".to_string(), "a/b".to_string()] {
        let request = v1::GetJobRequest { job_id: id };
        let status = client.get_job(request).await.err();
        let seen = status.map(|s| (s.code(), s.message().to_string()));
        assert_eq!(
            seen,
            Some((Code::InvalidArgument, "field: jobId".to_string()))
        );
    }
    let longest = "x".repeat(63);
    let request = v1::GetJobRequest { job_id: longest };
    let status = client.get_job(request).await.err();
    assert_eq!(status.map(|s| s.code()), Some(Code::NotFound));
}

#[tokio::test]
async fn a_hook_step_on_an_unknown_job_is_refused_and_a_placement_stamps_the_host() {
    let mut harness = harness().await;
    assert_eq!(harness.step("nobody", Step::Place), Err(Refused));
    let mut client = harness.client();
    client
        .submit_job(jobs::submit(jobs::spec("j")))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(harness.step("j", Step::Place), Ok(()));
    let job = client
        .get_job(v1::GetJobRequest {
            job_id: "j".to_string(),
        })
        .await;
    let job = job.ok().and_then(|r| r.into_inner().job);
    assert_eq!(
        job.as_ref().map(|j| j.host_id.as_str()),
        Some("conformance-host")
    );
    assert_eq!(job.map(|j| j.state), Some(i32::from(JobState::Placed)));
}

#[tokio::test]
async fn the_calls_the_model_does_not_serve_answer_unimplemented() {
    let mut harness = harness().await;
    let mut client = harness.client();
    let codes = [
        client
            .get_logs(v1::GetLogsRequest::default())
            .await
            .err()
            .map(|s| s.code()),
        client
            .list_hosts(v1::ListHostsRequest::default())
            .await
            .err()
            .map(|s| s.code()),
        client
            .cordon_host(v1::CordonHostRequest::default())
            .await
            .err()
            .map(|s| s.code()),
        client
            .uncordon_host(v1::UncordonHostRequest::default())
            .await
            .err()
            .map(|s| s.code()),
        client
            .list_pools(v1::ListPoolsRequest::default())
            .await
            .err()
            .map(|s| s.code()),
        client
            .list_providers(v1::ListProvidersRequest::default())
            .await
            .err()
            .map(|s| s.code()),
        client
            .get_usage(v1::GetUsageRequest::default())
            .await
            .err()
            .map(|s| s.code()),
    ];
    assert_eq!(codes, [Some(Code::Unimplemented); 7]);
    let status = client.get_usage(v1::GetUsageRequest::default()).await.err();
    assert_eq!(
        status.map(|s| s.message().to_string()),
        Some("unimplemented".to_string())
    );
}

#[tokio::test]
async fn the_clones_of_a_service_are_one_service() {
    let jobs = ReferenceJobs::new("c");
    let other = jobs.clone();
    let mut harness = ReferenceHarness::of(jobs);
    let mut client = harness.client();
    client
        .submit_job(jobs::submit(jobs::spec("shared")))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(other.step("shared", Step::Place), Ok(()));
    other.advance(Duration::from_millis(5));
    other.sweep();
    let job = client
        .get_job(v1::GetJobRequest {
            job_id: "shared".to_string(),
        })
        .await;
    assert_eq!(
        job.ok().and_then(|r| r.into_inner().job).map(|j| j.state),
        Some(i32::from(JobState::Placed))
    );
}
