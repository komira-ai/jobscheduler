//! The job-service conformance suite: what a server of `jobscheduler.v1.JobService` must
//! answer, whoever implements it. The real service (`js-lease-server`'s `JobApi`) is the
//! spec; a fake that a client's tests talk to runs the same suite, so a fake that drifts
//! from the server fails here instead of in production.
//!
//! The scenarios talk to the service through a tonic client ([`JobHarness::client`]), the
//! surface every client uses, and drive what no client can (the scheduler placing a job,
//! the agent starting and ending it, time passing) through a few hooks of
//! [`JobHarness`]. They name no transport beyond gRPC and never sleep.
//!
//! ```
//! # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
//! use js_conformance::jobs::{self, ReferenceJobs};
//!
//! let report = jobs::run(ReferenceJobs::harness).await;
//! assert!(report.failed().is_empty());
//! # });
//! ```
//!
//! # What the suite holds a service to
//!
//! Every answer below is the real service's (`crates/js-lease-server/src/job_api/`):
//!
//! - A submit names its job id. The same id with the same spec answers the job already
//!   recorded and records no second one; the same id with another spec is refused with
//!   `ALREADY_EXISTS` carrying the refusal `JOB_ID_CONFLICT`.
//! - A job moves queued, placed, starting, running, then succeeded (exit code 0) or failed
//!   (any other code). A cancel ends a queued job as cancelled at once and only marks a
//!   later one (`cancel_requested`). A move the state machine has no edge for is refused
//!   and leaves the job as it was.
//! - `GetResult` of a job that has not ended answers `FAILED_PRECONDITION` `not-finished`;
//!   after the end it answers the recorded result.
//! - An ended job's result is readable until its retention ends; from then `GetResult`
//!   answers `NOT_FOUND` `expired`, and once the retention sweep has run every read answers
//!   `NOT_FOUND` `unknown-job`.
//! - Every refusal has its stable code and message: `NOT_FOUND` `unknown-job` for an id no
//!   job has, `INVALID_ARGUMENT` `field: jobId` for a malformed id in a read or cancel,
//!   `INVALID_ARGUMENT` `field: spec.jobId` for one in a submit, `INVALID_ARGUMENT`
//!   `field: spec` for a submit without a spec, and `INVALID_ARGUMENT` with the refusal
//!   `WALL_TIME_NOT_POSITIVE` for a wall limit that is absent or 0.
//! - The job records the name of the client that submitted it ([`JobHarness::client_name`],
//!   the name the service gives the client's token) and returns it on every read.
//!
//! # What a harness adapter maps
//!
//! - **Time is injected.** [`JobHarness::advance`] moves the service's clock (the one its
//!   job store stamps ends and retention with), and [`JobHarness::sweep`] runs the
//!   retention sweep once. Nothing waits for real time.
//! - **The scheduler and the agent are hooks.** [`JobHarness::step`] makes one move of a
//!   job's state machine the way the server's own placing pass and agent heartbeats would,
//!   and reports whether the state machine took it. The scenarios read the result only
//!   through the client.
//! - **A fresh service per scenario.** [`run`] builds one harness per scenario, so no
//!   scenario sees another's jobs.
//!
//! [`ReferenceJobs`] is the in-memory service this crate's tests and the fakes of other
//! crates' tests serve; each [`Defect`] breaks one rule of it, which shows each scenario
//! can fail.

mod model;
mod scenarios;
mod serve;

use std::future::Future;
use std::time::Duration;

use js_proto::v1::job_service_client::JobServiceClient;
use js_proto::v1::{self, JobSpec};
use tonic::transport::Channel;

pub use model::{Defect, ReferenceHarness, ReferenceJobs};
pub use scenarios::Scenario;
pub use serve::channel_to;

use crate::Verdict;

/// One move of a job's state machine that only the server itself makes: the placing pass
/// puts a queued job on a host, the agent reports it starting, running and ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The placing pass puts the job on a host: queued to placed.
    Place,
    /// The agent reports the job starting: placed to starting.
    Start,
    /// The agent reports the job running: starting to running.
    Run,
    /// The agent reports the job's process ended with this exit code: running to succeeded
    /// (code 0) or failed (any other code).
    Exit(i32),
}

/// The service's state machine refused a [`Step`]: the job is in no state the move starts
/// from. The job is left as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused;

/// A job service under test, with the hooks that do what no client can.
pub trait JobHarness {
    /// A client of the service. Every call it makes is the client [`Self::client_name`]
    /// names.
    fn client(&mut self) -> JobServiceClient<Channel>;

    /// The name the service gives the client of [`Self::client`]: the name a submitted job
    /// records.
    fn client_name(&self) -> String;

    /// How long the service keeps an ended job's result: from the job's end until its
    /// result reads as expired.
    fn retention(&self) -> Duration;

    /// Makes `step` on job `job` as the server's placing pass or an agent heartbeat would.
    /// Returns [`Refused`] when the job is in no state that `step` starts from (or no job
    /// has that id).
    fn step(&mut self, job: &str, step: Step) -> Result<(), Refused>;

    /// Moves the service's clock forward by `by`.
    fn advance(&mut self, by: Duration);

    /// Runs the service's retention sweep once: every ended job whose retention has ended
    /// is deleted.
    fn sweep(&mut self);
}

/// The verdicts of one run, in [`Scenario::ALL`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobsReport {
    /// Each scenario with its verdict.
    pub results: Vec<(Scenario, Verdict)>,
}

impl JobsReport {
    /// The scenarios that failed.
    #[must_use]
    pub fn failed(&self) -> Vec<Scenario> {
        let failed = self
            .results
            .iter()
            .filter(|(_, verdict)| verdict.is_failed());
        failed.map(|(scenario, _)| *scenario).collect()
    }
}

impl Verdict {
    fn is_failed(&self) -> bool {
        matches!(self, Verdict::Failed(_))
    }
}

/// Runs every scenario, each on a fresh harness from `make`.
pub async fn run<H, F>(mut make: impl FnMut() -> F) -> JobsReport
where
    H: JobHarness,
    F: Future<Output = H>,
{
    let mut results = Vec::with_capacity(Scenario::ALL.len());
    for scenario in Scenario::ALL {
        let mut harness = make().await;
        results.push((scenario, scenario.run(&mut harness).await));
    }
    JobsReport { results }
}

/// The wall limit of [`spec`]: ten minutes. A service under test admits it (its ceiling is
/// at least this).
pub const WALL_SECONDS: u64 = 600;

/// The spec every scenario submits, with job id `id`: a pinned image, one command, the
/// [`WALL_SECONDS`] wall limit, no needs, no labels. Every field the service validates is
/// valid, and nothing in it is a capacity no host could have.
#[must_use]
pub fn spec(id: &str) -> JobSpec {
    JobSpec {
        job_id: id.to_string(),
        image: format!("registry.example/conformance@sha256:{}", "a".repeat(64)),
        command: "true".to_string(),
        wall_limit_seconds: Some(WALL_SECONDS),
        ..JobSpec::default()
    }
}

/// [`spec`] with `args` set: the same id, another job.
#[must_use]
pub fn other_spec(id: &str) -> JobSpec {
    JobSpec {
        args: vec!["other".to_string()],
        ..spec(id)
    }
}

/// The request that submits `spec`.
#[must_use]
pub fn submit(spec: JobSpec) -> v1::SubmitJobRequest {
    v1::SubmitJobRequest { spec: Some(spec) }
}
