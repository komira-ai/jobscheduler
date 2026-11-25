//! A claimed start whose job's own record breaks a rule of the wire, as a job admitted before
//! `SubmitJob` checked its environment does: the poll ends the job `failed-to-start` with the
//! wire's code, once, instead of leaving its start claimed and the job placed.

use std::sync::Mutex;

use js_jobs::StartFailure;
use js_sched_placer::engine::TablesFrom;

use super::*;

/// The job `id` recorded and placed on `host-a` with an environment that sets `HOME`, a name
/// the agent owns and the wire refuses (`invalid-env`), as a submit before the check let it in.
fn placed_with_home(rig: &Rig, id: &str) -> JobId {
    let mut admitted = spec();
    admitted
        .env
        .insert("HOME".to_owned(), "/scratch".to_owned());
    placed(&rig.jobs, id, "host-a", admitted, BOOKED).0.id
}

/// Asserts that a poll that claims the start of a job whose environment sets `HOME` hands out
/// the other job's start and not this one, and ends the job `failed-to-start` as
/// `agent-refused` with the detail `invalid-env` (what an agent that was sent the start would
/// have answered), its allocation releasing, its start deleted and no stop queued (the host was
/// never sent it). A later poll, after the claim's lease would have ended, hands out nothing
/// for it and leaves its end as it was. Catches the start left claimed and the job placed,
/// logged on every poll, until the start timeout or a cancel ends it.
#[tokio::test]
async fn a_start_that_breaks_a_wire_rule_ends_its_job_failed_to_start() {
    let rig = server("dispatch_unbuilt", "server-a");
    let bad = placed_with_home(&rig, "job-1");
    rig.place("job-2", "host-a");
    let first = polled(&rig, Vec::new()).await.unwrap();
    assert_eq!(names(&first), [start("job-2")]);
    let ended = rig.job(&bad);
    let failed = JobState::FailedToStart(StartFailure::AgentRefused);
    assert_eq!(ended.state, failed);
    assert_eq!(ended.start_failure_detail.as_deref(), Some("invalid-env"));
    assert_eq!(rig.allocation(&bad).step, AllocationStep::Releasing);
    assert!(
        queued(&rig).iter().all(|message| message.job != bad),
        "its start is deleted and no stop is queued"
    );
    rig.advance(rig.agents.dispatching.times.lease + Duration::from_millis(1));
    let later = polled(&rig, acks_of(&first)).await.unwrap();
    assert_eq!(later, DispatchReply::default());
    assert_eq!(rig.job(&bad), ended, "the first end stands");
}

/// A sink that records the start failures it is told of, and counts nothing else.
#[derive(Debug, Default)]
struct Failures(Mutex<Vec<StartFailure>>);

impl crate::metrics::MetricsSink for Failures {
    fn job_placed(&self, _: Duration) {}
    fn start_failed(&self, failure: StartFailure) {
        self.0.lock().unwrap().push(failure);
    }
    fn job_lost(&self) {}
    fn store_write_failed(&self) {}
    fn job_pass_took(&self, _: Duration) {}
    fn job_tables(&self, _: TablesFrom) {}
    fn acquire_answered(&self, _: &str, _: &js_elastic::Acquired) {}
}

/// Asserts that the same claimed start of a job whose environment sets `HOME`, built twice (a
/// message handed out again after its lease), ends the job once and counts one start failure,
/// `agent-refused`: the second build finds the job ended and leaves the start out without a
/// second end or a second count. Catches a start failure counted on every delivery of the
/// message.
#[test]
fn a_start_that_breaks_a_wire_rule_is_ended_and_counted_once() {
    let rig = server("dispatch_unbuilt_once", "server-a");
    let bad = placed_with_home(&rig, "job-1");
    let message = queued(&rig).pop().unwrap();
    let facts = HostFacts::of(&rig.agents, "host-a").unwrap();
    let everything = (Vec::new(), Vec::new(), limits::MAX_COMMANDS);
    let inputs = Inputs::new(&rig.jobs, &facts, None, everything);
    let failures = Failures::default();
    let context = inputs.context(&failures);
    let claimed = [message.clone(), message];
    assert_eq!(handed_out(&rig.jobs, &context, &claimed).unwrap(), []);
    assert_eq!(
        rig.job(&bad).state,
        JobState::FailedToStart(StartFailure::AgentRefused)
    );
    assert_eq!(*failures.0.lock().unwrap(), [StartFailure::AgentRefused]);
}

/// Asserts that when the end of a job whose environment sets `HOME` cannot be committed, the
/// build leaves the start out, counts nothing and keeps the job placed with its start queued,
/// and that the next build of the same message (its next delivery), with the store committing
/// again, ends the job `failed-to-start` and counts it. Catches a failed end that loses the
/// start for good, or one that fails the whole poll.
#[test]
fn an_end_that_does_not_commit_is_made_by_the_next_delivery() {
    let rig = server("dispatch_unbuilt_fails", "server-a");
    let bad = placed_with_home(&rig, "job-1");
    let message = queued(&rig).pop().unwrap();
    let facts = HostFacts::of(&rig.agents, "host-a").unwrap();
    let everything = (Vec::new(), Vec::new(), limits::MAX_COMMANDS);
    let inputs = Inputs::new(&rig.jobs, &facts, None, everything);
    let failures = Failures::default();
    let context = inputs.context(&failures);
    let claimed = [message];
    rig.store.fail_after(0);
    assert_eq!(handed_out(&rig.jobs, &context, &claimed).unwrap(), []);
    assert_eq!(rig.job(&bad).state, JobState::Placed);
    assert!(failures.0.lock().unwrap().is_empty());
    assert_eq!(queued(&rig).len(), 1, "its start is still queued");
    assert_eq!(handed_out(&rig.jobs, &context, &claimed).unwrap(), []);
    assert_eq!(
        rig.job(&bad).state,
        JobState::FailedToStart(StartFailure::AgentRefused)
    );
    assert_eq!(*failures.0.lock().unwrap(), [StartFailure::AgentRefused]);
    assert!(queued(&rig).is_empty());
}
