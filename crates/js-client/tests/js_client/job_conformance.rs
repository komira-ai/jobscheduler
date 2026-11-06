//! The job-service test fake held to the job-service conformance suite: the fake, serving
//! the in-memory reference service, answers every scenario as the real service does; a fake
//! that answers only from its script does not; and a scripted answer still wins over the
//! service behind the fake.

use std::future::{Ready, ready};
use std::time::Duration;

use js_conformance::jobs::{self, JobHarness, ReferenceJobs, Refused, Scenario, Step};
use js_proto::v1::GetJobRequest;
use js_proto::v1::job_service_client::JobServiceClient;
use tonic::Code;
use tonic::transport::Channel;

use super::fake_jobs::{Answer, Fake};

/// A harness whose client calls go through the fake's own routing, and whose hooks act on
/// the service behind it.
struct FakeHarness {
    jobs: ReferenceJobs,
    client: JobServiceClient<Channel>,
}

impl FakeHarness {
    /// A harness over `fake`, with `jobs` for the hooks. Needs a tokio runtime.
    fn over(fake: &Fake, jobs: ReferenceJobs) -> Self {
        Self {
            jobs,
            client: JobServiceClient::new(jobs::channel_to(fake.clone())),
        }
    }

    /// A harness over a fake serving a fresh reference service.
    fn serving() -> Ready<Self> {
        let jobs = ReferenceJobs::new("conformance-client");
        ready(Self::over(&Fake::serving(jobs.clone()), jobs))
    }
}

impl JobHarness for FakeHarness {
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

#[tokio::test]
async fn the_fake_passes_the_job_service_suite() {
    let report = jobs::run(FakeHarness::serving).await;
    assert_eq!(report.failed(), Vec::<Scenario>::new());
}

#[tokio::test]
async fn a_fake_that_answers_only_from_its_script_fails_the_suite() {
    let report = jobs::run(|| {
        let jobs = ReferenceJobs::new("conformance-client");
        ready(FakeHarness::over(&Fake::new(), jobs))
    })
    .await;
    assert_eq!(report.failed(), Scenario::ALL.to_vec());
}

#[tokio::test]
async fn a_scripted_answer_overrides_the_service_behind_the_fake() {
    let jobs = ReferenceJobs::new("conformance-client");
    let fake = Fake::serving(jobs.clone());
    fake.answer(
        "GetJob",
        vec![Answer::Status(Code::Unavailable, "scripted")],
    );
    let mut client = JobServiceClient::new(jobs::channel_to(fake.clone()));

    let submitted = client.submit_job(jobs::submit(jobs::spec("a"))).await;
    assert!(submitted.is_ok(), "an unscripted call reaches the service");

    let status = client
        .get_job(GetJobRequest {
            job_id: "a".to_owned(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        (status.code(), status.message()),
        (Code::Unavailable, "scripted")
    );
    let paths: Vec<String> = fake.seen().into_iter().map(|seen| seen.path).collect();
    assert_eq!(
        paths,
        [
            "/jobscheduler.v1.JobService/SubmitJob",
            "/jobscheduler.v1.JobService/GetJob"
        ]
    );
}
