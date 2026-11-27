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
//!   later one (`cancel_requested`). The mark means asked and not yet ended: a job that has
//!   ended shows it false, a queued job cancelled at once and a marked job that then exits
//!   alike. A move the state machine has no edge for is refused and leaves the job as it
//!   was.
//! - A job with a retry policy (`spec.retry`) is retried when an attempt ends for a cause of
//!   its host, and only then: a host that is lost while the job is placed, starting or
//!   running, and a start that fails ([`Step::HostLost`], [`Step::StartFailed`]). The job
//!   is queued again, not ended: it shows the attempt that ended in `attempt_history` as
//!   retried (its number, host, times, outcome `HOST_LOST` or `START_FAILED` and start
//!   failure), no host, the queue status `WAITING_FOR_RETRY`, and `retry_not_before_ms`,
//!   after the end of that attempt and at most the policy's longest wait away (the wait is
//!   [`js_types::decide`]'s), and its result is not ready. It is not placed before that
//!   time; the next placement counts the next attempt, and the time is no longer shown. The
//!   attempt count and the history of every ended attempt, the last included (not retried),
//!   show on the job and on its result (`attempt`). When the policy leaves no attempt, or
//!   the job has none, the job ends `LOST` or `FAILED_TO_START` (`START_TIMEOUT`).
//! - Nothing else retries: a process that exits with a nonzero code ends `FAILED` on its
//!   first attempt, and a job whose cancel was asked for while an attempt held a host ends
//!   `LOST` when that host is lost, still showing `cancel_requested`. A cancel of a job that
//!   waits for its retry ends it `CANCELLED` at once, with no attempt after the one that
//!   ended.
//! - A job with a callback (`spec.callback`) shows `callback_status` `ARMED` with no tries
//!   from its submit, through every retry, and `PENDING` with its deadline once the job has
//!   ended for good; one delivery try answered 2xx ([`Step::DeliverCallback`]) makes it
//!   `DELIVERED` with one try, and no second try is due. No job shows its callback's key.
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
//!   and reports whether the state machine took it. The same hook loses a job's host, fails
//!   its start and delivers its due callback ([`Step::HostLost`], [`Step::StartFailed`],
//!   [`Step::DeliverCallback`]), through the path the real service takes for each: the
//!   job pass's own loss, the start timeout, a callback claimed and answered. The scenarios
//!   read the result only through the client.
//! - **A fresh service per scenario.** [`run`] builds one harness per scenario, so no
//!   scenario sees another's jobs.
//!
//! [`ReferenceJobs`] is the in-memory service this crate's tests and the fakes of other
//! crates' tests serve; each [`Defect`] breaks one rule of it, which shows each scenario
//! can fail.

mod model;
mod retry;
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
    /// The job's host is lost while the job is placed, starting or running: the provider
    /// reads the host absent, or the host comes back and does not list the job. The real
    /// service retries the attempt when the job's retry policy leaves another attempt and no
    /// cancel was asked for (the job is queued again, its attempt in the history, and it may
    /// be placed once its wait has passed); otherwise the job ends lost. A job that is in
    /// none of those states is refused.
    HostLost,
    /// The job's start fails for a cause of its host, as a start that times out does: the
    /// job is placed or starting. The real service retries the attempt as it does for
    /// [`Step::HostLost`]; otherwise the job ends failed to start (`START_TIMEOUT`). A job
    /// that is in neither state is refused.
    StartFailed,
    /// One try of the job's completion callback, answered 2xx by the receiver: the callback
    /// of a job that has ended and has one due is delivered, with one try. Refused when no
    /// callback of the job is due: the job has none, has not ended (a job that waits for a
    /// retry included), or its callback was delivered already.
    DeliverCallback,
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
