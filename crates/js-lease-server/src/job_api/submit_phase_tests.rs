//! The phases a `SubmitJob` call times (the module docs' "Phases timed"): each phase the call
//! reaches is counted once in `js_submit_phase_seconds`, whatever its answer, and no other. The
//! meter's clock moves a millisecond at each reading, so each timed phase took exactly 1 ms and
//! nothing here sleeps; the fake store has no pool, so the insert's checkout is 0.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_proto::v1::{self, SubmitJobRequest};
use js_types::Monotonic;
use tonic::{Code, Request, Status};

use super::{Ctx, submit_job};
use crate::agent_api::job_rig::{Rig, clocked_rig};
use crate::config::Role;
use crate::job_api::JobApi;
use crate::job_api::roles::Caller;
use crate::job_api::writes_rig::{CEILING, Fixed, NEVER, wire};
use crate::metrics::{Meter, Recorder, rendered};

/// The meter's clock: each reading is a millisecond after the one before it.
#[derive(Debug, Default)]
struct Stepping(Mutex<Duration>);

impl Monotonic for Stepping {
    fn reading(&self) -> Duration {
        let mut at = self.0.lock().unwrap();
        *at += Duration::from_millis(1);
        *at
    }
}

/// A placing rig whose agent side counts on a recorder timed by [`Stepping`]; the service over
/// its tables admitting as `admits` says, its submit poking the rig's poller when `poking`.
fn metered(name: &str, admits: bool, poking: bool) -> (Rig, JobApi, Arc<Recorder>) {
    let rig = clocked_rig(name, true, NEVER);
    let recorder = Arc::new(Recorder::default());
    let meter = Meter::new(recorder.clone(), Arc::new(Stepping::default()));
    rig.agents.set_meter(meter);
    let ctx = Ctx::served(CEILING, Fixed::new(admits));
    let ctx = match poking {
        true => ctx.poking(Arc::clone(&rig.agents)),
        false => ctx,
    };
    let api = JobApi::new(Arc::clone(&rig.jobs), rig.shared.clone()).with_submit(ctx);
    (rig, api, recorder)
}

/// The answer to submitting `spec` to `api` as an operator.
async fn submit(api: &JobApi, spec: v1::JobSpec) -> Result<v1::Job, Status> {
    let caller = Caller {
        name: js_jobs::ClientName::default_client(),
        role: Role::Operator,
    };
    let request = Request::new(SubmitJobRequest { spec: Some(spec) });
    let answer = submit_job(api, &caller, request).await?;
    Ok(answer.into_inner().job.unwrap())
}

/// The phases in the order they are written.
const PHASES: [&str; 6] = [
    "validation",
    "get",
    "admission",
    "insert-checkout",
    "insert-transaction",
    "note-inserted",
];

/// Of each phase, in [`PHASES`] order, its `_count` and its `_sum` as the scrape of `recorder`
/// writes them; a phase with no series reads `None`.
fn phases(recorder: &Recorder) -> Vec<Option<(u64, String)>> {
    let text = rendered(recorder);
    let value = |series: String| {
        let prefix = format!("{series} ");
        let mut lines = text.lines().filter_map(|line| line.strip_prefix(&prefix));
        let found = lines.next().map(str::to_owned);
        assert_eq!(lines.next(), None, "{series} is written once");
        found
    };
    PHASES
        .iter()
        .map(|phase| {
            let label = format!("{{phase=\"{phase}\"}}");
            let count = value(format!("js_submit_phase_seconds_count{label}"));
            let sum = value(format!("js_submit_phase_seconds_sum{label}"));
            Some((count?.parse().unwrap(), sum?))
        })
        .collect()
}

/// Each phase's count and sum, from `counts` and `sums` in [`PHASES`] order.
fn expected(counts: [u64; 6], sums: [&str; 6]) -> Vec<Option<(u64, String)>> {
    let pairs = counts.into_iter().zip(sums);
    pairs.map(|(n, sum)| Some((n, sum.to_owned()))).collect()
}

/// Asserts that one submit of a new job with an id, admitted and committed on the fake store,
/// is timed once in each of the six phases: each series exists with a count of 1, each phase
/// took the one step of the meter's clock, and the insert spent nothing getting connections.
/// It catches a phase that is not timed, one timed twice, one counted under another's label,
/// and an insert whose checkout share is its whole time.
#[tokio::test]
async fn one_submit_is_timed_once_in_each_phase() {
    let (_rig, api, recorder) = metered("phase_each", true, true);
    let job = submit(&api, wire("job-each")).await.unwrap();
    assert_eq!(job.state(), v1::JobState::Queued);
    let ms = "0.001";
    let want = expected([1; 6], [ms, ms, ms, "0", ms, ms]);
    assert_eq!(phases(&recorder), want);
}

/// Asserts that before any submit every phase is written at zero, and that a context that does
/// not poke (no agent side) times nothing, though the agent side's meter counts. It catches a
/// family written only once observed, and a submit timed by a meter it was not given.
#[tokio::test]
async fn every_phase_is_written_at_zero_and_a_submit_that_does_not_poke_is_not_timed() {
    let (_rig, api, recorder) = metered("phase_unpoked", true, false);
    let zero = expected([0; 6], ["0"; 6]);
    assert_eq!(phases(&recorder), zero);
    submit(&api, wire("job-unpoked")).await.unwrap();
    assert_eq!(phases(&recorder), zero);
}

/// Asserts that a job the checks refuse is timed in validation alone: the refusal is the
/// answer and no later phase is reached. It catches a refused call left untimed, and phases
/// counted that the call never reached.
#[tokio::test]
async fn a_job_the_checks_refuse_is_timed_in_validation_alone() {
    let (_rig, api, recorder) = metered("phase_refused", true, true);
    let spec = v1::JobSpec {
        wall_limit_seconds: Some(0),
        ..wire("job-refused")
    };
    let refused = submit(&api, spec).await.unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    let want = expected([1, 0, 0, 0, 0, 0], ["0.001", "0", "0", "0", "0", "0"]);
    assert_eq!(phases(&recorder), want);
}

/// Asserts that a job with no id reads no id, so `get` is not timed, and every other phase is.
/// It catches a `get` timed for a read that did not happen.
#[tokio::test]
async fn a_job_with_no_id_is_not_timed_in_get() {
    let (_rig, api, recorder) = metered("phase_no_id", true, true);
    submit(&api, wire("")).await.unwrap();
    let ms = "0.001";
    let want = expected([1, 0, 1, 1, 1, 1], [ms, "0", ms, "0", ms, ms]);
    assert_eq!(phases(&recorder), want);
}

/// Asserts that a job nothing could serve is timed up to its admission, which refused it, and
/// no further. It catches an admission timed only when it admits.
#[tokio::test]
async fn a_job_admission_refuses_is_timed_up_to_its_admission() {
    let (_rig, api, recorder) = metered("phase_unservable", false, true);
    let refused = submit(&api, wire("job-unservable")).await.unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition);
    let ms = "0.001";
    let want = expected([1, 1, 1, 0, 0, 0], [ms, ms, ms, "0", "0", "0"]);
    assert_eq!(phases(&recorder), want);
}

/// Asserts that an insert whose commit fails is still timed, in both of its parts, and that
/// the placer is then not told: the answer is `state-write-failed`. It catches an insert timed
/// only when it commits.
#[tokio::test]
async fn an_insert_whose_commit_fails_is_timed_and_the_placer_is_not() {
    let (rig, api, recorder) = metered("phase_failed", true, true);
    rig.store.fail_next();
    let failed = submit(&api, wire("job-failed")).await.unwrap_err();
    assert_eq!(failed.code(), Code::Internal);
    let ms = "0.001";
    let want = expected([1, 1, 1, 1, 1, 0], [ms, ms, ms, "0", ms, "0"]);
    assert_eq!(phases(&recorder), want);
}

/// Asserts that a retry of a job already held is timed in every phase but admission, which it
/// skips, its insert answering the held job: two calls, one admission. It catches a retry
/// timed in an admission it did not ask.
#[tokio::test]
async fn a_retry_of_a_held_job_is_timed_in_every_phase_but_admission() {
    let (_rig, api, recorder) = metered("phase_retry", true, true);
    let first = submit(&api, wire("job-retry")).await.unwrap();
    let again = submit(&api, wire("job-retry")).await.unwrap();
    assert_eq!(again.job_id, first.job_id);
    let (one, two) = ("0.001", "0.002");
    let want = expected([2, 2, 1, 2, 2, 2], [two, two, one, "0", two, two]);
    assert_eq!(phases(&recorder), want);
}
