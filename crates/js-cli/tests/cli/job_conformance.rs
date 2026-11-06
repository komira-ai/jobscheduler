//! The job-service fake of these tests held to the job-service conformance suite: the
//! fake backed by the in-memory reference service passes it, a fake that answers only from
//! its script does not, and a scripted answer still wins over the backing.

use std::time::Duration;

use js_conformance::jobs::{self, JobHarness, ReferenceJobs, Refused, Scenario, Step};
use js_proto::v1::GetJobRequest;
use js_proto::v1::job_service_client::JobServiceClient;
use tonic::Code;
use tonic::transport::Channel;

use crate::fake_jobs::{Answer, Fake};

/// The client name the reference service records.
const CLIENT: &str = "cli-client";

/// A harness whose client calls go through `fake`, and whose hooks act on `jobs`.
struct FakeHarness {
    fake: Fake,
    jobs: ReferenceJobs,
}

impl FakeHarness {
    /// A fake served by a reference service, hooked to the same service.
    fn backed() -> Self {
        let jobs = ReferenceJobs::new(CLIENT);
        Self {
            fake: Fake::serving(jobs.clone()),
            jobs,
        }
    }

    /// A fake with no backing, hooked to a reference service it does not serve.
    fn scripted_only() -> Self {
        Self {
            fake: Fake::new(),
            jobs: ReferenceJobs::new(CLIENT),
        }
    }
}

impl JobHarness for FakeHarness {
    fn client(&mut self) -> JobServiceClient<Channel> {
        JobServiceClient::new(jobs::channel_to(self.fake.clone()))
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

#[tokio::test(flavor = "current_thread")]
async fn the_fake_passes_the_job_service_suite() {
    let report = jobs::run(|| std::future::ready(FakeHarness::backed())).await;
    assert_eq!(report.failed(), Vec::<Scenario>::new());
}

#[tokio::test(flavor = "current_thread")]
async fn a_fake_answering_only_from_its_script_fails_the_suite() {
    let report = jobs::run(|| std::future::ready(FakeHarness::scripted_only())).await;
    assert_eq!(report.failed(), Scenario::ALL.to_vec());
}

#[tokio::test(flavor = "current_thread")]
async fn a_scripted_answer_overrides_the_backing_for_its_method() {
    let mut harness = FakeHarness::backed();
    harness.fake.answer(
        "GetJob",
        vec![Answer::Status(Code::Unavailable, "scripted")],
    );
    let mut client = harness.client();
    let submitted = client
        .submit_job(jobs::submit(jobs::spec("job-a")))
        .await
        .unwrap();
    assert!(submitted.into_inner().job.is_some());
    let got = client
        .get_job(GetJobRequest {
            job_id: "job-a".to_string(),
        })
        .await
        .unwrap_err();
    assert_eq!(got.code(), Code::Unavailable);
    assert_eq!(got.message(), "scripted");
    // The backing holds the job the scripted call did not answer, and the call was recorded.
    let seen: Vec<String> = harness.fake.seen().into_iter().map(|s| s.path).collect();
    assert_eq!(
        seen,
        [
            "/jobscheduler.v1.JobService/SubmitJob",
            "/jobscheduler.v1.JobService/GetJob"
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_forwarded_call_is_recorded_with_its_message() {
    let mut harness = FakeHarness::backed();
    let mut client = harness.client();
    client
        .get_job(GetJobRequest {
            job_id: "nope".to_string(),
        })
        .await
        .unwrap_err();
    let seen = harness.fake.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/jobscheduler.v1.JobService/GetJob");
    assert!(!seen[0].message.is_empty());
}
