//! The negative cases of the three invariants of retry and the completion callback
//! (`attempts-bounded`, `one-attempt-at-a-time` and `callback-after-the-final-end`, in
//! [`NEGATIVE_CASES`], which the parent's table check reads with its own), their correct
//! siblings, and the boundaries of what each reads.

use js_jobs::{AllocationId, JobId, JobState};
use js_sim_cell::{CallbackCall, JobReply, JobStartSeen};
use js_types::CallbackAnswer;

use super::*;

/// Every negative case of these invariants and the one invariant it must trip, as the
/// parent's `NEGATIVE_CASES`.
pub(crate) const NEGATIVE_CASES: &[NegativeCase] = &[
    (Invariant::AttemptsBounded, launched_past_the_limit),
    (Invariant::AttemptsBounded, read_past_the_limit),
    (Invariant::OneAttemptAtATime, start_beside_another_host),
    (Invariant::OneAttemptAtATime, start_beside_the_same_host),
    (Invariant::CallbackAfterTheFinalEnd, called_while_queued),
    (Invariant::CallbackAfterTheFinalEnd, called_with_two_ids),
];

fn job(n: u64) -> JobId {
    JobId::parse(&format!("sim-job-{n}")).expect("a job id")
}

fn allocation(n: u8) -> AllocationId {
    AllocationId::from_bits([n; 16])
}

/// The run of job `j` under the text `a` of an allocation.
fn job_run(j: u64, a: &str) -> RunId {
    RunId::new(format!("sim-job-{j}/{a}"))
}

/// The lines of the step violations of `observations`.
fn found(observations: &Observations) -> Vec<String> {
    lines(&step_all(observations))
}

/// Asserts that nothing is flagged.
fn clear(observations: &Observations) {
    assert_eq!(found(observations), Vec::<String>::new());
}

/// Job 0 asked for `max` attempts, read at 10.
fn asked(max: u32) -> Observations {
    Observations::quiet()
        .at(ms(1_000))
        .with_attempt_read(job(0), 1, max, ms(10))
}

/// `base` with job 0 launched under each of `allocations`, 100 ms apart from 100.
fn launched(base: Observations, allocations: &[&str]) -> Observations {
    let mut at = 0;
    allocations.iter().fold(base, |o, a| {
        at += 100;
        o.with_launch(job_run(0, a), host(0), ms(at))
    })
}

/// Host `h` read a start of job 0 under allocation `a` at `at`.
fn start(h: u64, a: u8, at: u64) -> JobReply {
    JobReply {
        host: host(h),
        at: ms(at),
        beat_at: ms(at - 10),
        listed: BTreeSet::new(),
        starts: vec![JobStartSeen {
            job: job(0),
            allocation: allocation(a),
            refused: false,
        }],
        stops: BTreeSet::new(),
        acks: Vec::new(),
    }
}

/// A truth of an agent holding job 0 under allocation `a`.
fn holding(a: u8) -> AgentTruth {
    AgentTruth::new(capacity()).with_job(job(0), allocation(a), res(1_000, GIB))
}

/// A call the receiver got for job `j` at `at`, with `webhook`, the job's row in `state`.
fn call(j: u64, at: u64, webhook: &str, state: Option<JobState>) -> CallbackCall {
    CallbackCall {
        job: job(j),
        webhook_id: webhook.to_owned(),
        at: ms(at),
        answer: CallbackAnswer::Delivered,
        ended: state.is_some_and(JobState::is_ended),
        state,
    }
}

// ---- The negative cases. ----

/// Job 0 asked for 2 attempts and is launched under 3 allocations.
pub fn launched_past_the_limit() -> Observations {
    launched(asked(2), &["a1", "a2", "a3"])
}

/// Job 0 asked for 2 attempts and its row is read at attempt 3.
pub fn read_past_the_limit() -> Observations {
    asked(2).with_attempt_read(job(0), 3, 2, ms(500))
}

/// Host 0 holds job 0 under allocation 1, and host 1 reads a start of it under 2.
pub fn start_beside_another_host() -> Observations {
    Observations::quiet()
        .at(ms(1_000))
        .with_agent(host(0), holding(1))
        .with_agent(host(1), AgentTruth::new(capacity()))
        .with_job_reply(start(1, 2, 500))
}

/// Host 0 holds job 0 under allocation 1 and reads a start of it under 2.
pub fn start_beside_the_same_host() -> Observations {
    Observations::quiet()
        .at(ms(1_000))
        .with_agent(host(0), holding(1))
        .with_job_reply(start(0, 2, 500))
}

/// The receiver is called for job 0 at 500 while its row reads queued, between attempts.
pub fn called_while_queued() -> Observations {
    let queued = call(0, 500, "w-1", Some(JobState::Queued));
    Observations::quiet()
        .at(ms(1_000))
        .with_callback_call(queued)
}

/// Two calls for job 0, after its end, with different webhook ids.
pub fn called_with_two_ids() -> Observations {
    let done = Some(JobState::Succeeded);
    Observations::quiet()
        .at(ms(1_000))
        .with_callback_call(call(0, 500, "w-1", done))
        .with_callback_call(call(0, 600, "w-2", done))
}

// ---- The tests. ----

/// Asserts the text of a launch past the limit, and that it is named once: a read of the
/// row afterwards, and a step with nothing new, flag nothing more, and a fourth launch is
/// named again. Catches a launch that goes unnamed and one named at every step.
#[test]
fn a_launch_past_the_limit_is_flagged_once_with_the_job_and_the_limit() {
    let first = "attempts-bounded at 1000: job sim-job-0 was launched under 3 allocations, the last at 300, its policy allows 2";
    assert_eq!(found(&launched_past_the_limit()), [first]);
    let mut checker = Checker::new();
    let everything = |o: &Observations| Touched::everything_in(o);
    let mut seen = launched_past_the_limit();
    assert_eq!(lines(&checker.step(&seen, &everything(&seen))), [first]);
    seen = seen.with_attempt_read(job(0), 2, 2, ms(600));
    none(&checker.step(&seen, &everything(&seen)));
    none(&checker.step(&seen, &everything(&seen)));
    seen = seen.with_launch(job_run(0, "a4"), host(1), ms(700));
    let fourth = "attempts-bounded at 1000: job sim-job-0 was launched under 4 allocations, the last at 700, its policy allows 2";
    assert_eq!(lines(&checker.step(&seen, &everything(&seen))), [fourth]);
}

/// Asserts the boundary: exactly `max_attempts` launches pass and one more fails, for a
/// policy of 3 and for a job that has none (one attempt). Catches an off-by-one on either
/// side.
#[test]
fn exactly_the_limit_of_launches_passes_and_one_more_fails() {
    clear(&launched(asked(3), &["a1", "a2", "a3"]));
    assert_eq!(
        found(&launched(asked(3), &["a1", "a2", "a3", "a4"])),
        [
            "attempts-bounded at 1000: job sim-job-0 was launched under 4 allocations, the last at 400, its policy allows 3"
        ]
    );
    clear(&launched(asked(1), &["a1"]));
    assert_eq!(
        found(&launched(asked(1), &["a1", "a2"])),
        [
            "attempts-bounded at 1000: job sim-job-0 was launched under 2 allocations, the last at 200, its policy allows 1"
        ]
    );
}

/// Asserts that a launch that comes before the first read of the job's row is judged when
/// the read arrives, and that a job never read has no known limit and is not judged.
/// Catches a limit applied before it is known and one that never catches up.
#[test]
fn a_launch_is_judged_by_the_limit_read_after_it() {
    let before = launched(Observations::quiet().at(ms(1_000)), &["a1", "a2"]);
    clear(&before);
    let mut checker = Checker::new();
    none(&checker.step(&before, &Touched::everything_in(&before)));
    let read = before.with_attempt_read(job(0), 1, 1, ms(500));
    let found = checker.step(&read, &Touched::nothing());
    assert_eq!(
        lines(&found),
        [
            "attempts-bounded at 1000: job sim-job-0 was launched under 2 allocations, the last at 200, its policy allows 1"
        ]
    );
}

/// Asserts the text of an attempt read past the limit and the boundary: attempt 2 of 2
/// passes, attempt 3 of 2 fails. Catches a read the check ignores.
#[test]
fn an_attempt_read_past_the_limit_is_flagged() {
    clear(&asked(2).with_attempt_read(job(0), 2, 2, ms(500)));
    assert_eq!(
        found(&read_past_the_limit()),
        ["attempts-bounded at 1000: job sim-job-0 read attempt 3 at 500, its policy allows 2"]
    );
}

/// Asserts that runs the check does not read are left alone: a run of a lease (no
/// allocation in its id), a run whose first part is no job id, the same allocation launched
/// twice (the ledger's own check names that), and launches of another job. Catches a run id
/// split wrongly.
#[test]
fn runs_that_are_not_a_jobs_launches_are_not_counted() {
    let base = asked(1)
        .with_launch(run(0), host(0), ms(100))
        .with_launch(RunId::new("not a job/a1".to_owned()), host(0), ms(110))
        .with_launch(job_run(0, "a1"), host(0), ms(120))
        .with_launch(job_run(0, "a1"), host(1), ms(130))
        .with_launch(job_run(1, "a1"), host(1), ms(140))
        .with_launch(job_run(1, "a2"), host(1), ms(150));
    assert_eq!(
        flagged(&step_all(&base)),
        [Invariant::AtMostOneLaunch],
        "only the repeated run is named, by its own invariant"
    );
}

/// Asserts the text of a start read beside another host's hold and beside the same
/// host's, and that nothing else is flagged. Catches a start beside a live earlier attempt
/// that goes unnamed.
#[test]
fn a_start_beside_a_live_attempt_is_flagged_with_both_hosts_and_allocations() {
    let one = allocation(1);
    let two = allocation(2);
    assert_eq!(
        found(&start_beside_another_host()),
        [format!(
            "one-attempt-at-a-time at 1000: host host-1 read a start of job sim-job-0 under allocation {two} at 500 while host host-0 still holds the job under allocation {one}"
        )]
    );
    assert_eq!(
        found(&start_beside_the_same_host()),
        [format!(
            "one-attempt-at-a-time at 1000: host host-0 read a start of job sim-job-0 under allocation {two} at 500 while host host-0 still holds the job under allocation {one}"
        )]
    );
}

/// Asserts the correct siblings: a start after the earlier attempt ended (no agent holds
/// the job), a start of the allocation the agent already holds (a repeat), a start an agent
/// refused as coming after its stop, a start of another job, and two hosts holding the job with no
/// start read (that is `one-allocation-per-job`'s). Catches a check that flags a retry, a resend or a refusal.
#[test]
fn a_start_with_no_other_attempt_held_is_not_flagged() {
    let idle = Observations::quiet()
        .at(ms(1_000))
        .with_agent(host(0), AgentTruth::new(capacity()));
    clear(&idle.clone().with_job_reply(start(1, 2, 500)));
    clear(
        &Observations::quiet()
            .at(ms(1_000))
            .with_agent(host(0), holding(1))
            .with_job_reply(start(0, 1, 500)),
    );
    let mut refused = start(1, 2, 500);
    refused.starts[0].refused = true;
    let held = Observations::quiet()
        .at(ms(1_000))
        .with_agent(host(0), holding(1));
    clear(&held.clone().with_job_reply(refused));
    let mut other = start(1, 2, 500);
    other.starts[0].job = job(1);
    clear(&held.with_job_reply(other));
    let both = Observations::quiet()
        .at(ms(1_000))
        .with_agent(host(0), holding(1))
        .with_agent(host(1), holding(2));
    assert_eq!(flagged(&step_all(&both)), [Invariant::OneAllocationPerJob]);
}

/// Asserts that the hold is read from the agents' truth as the steps touch them: a host
/// that held the job and then does not (the agent's process was replaced, or the agent went
/// away) no longer counts, a start read by the other host is then not flagged; and while it
/// holds the job the same start is flagged. Catches a hold that outlives its agent.
#[test]
fn an_attempt_that_ended_no_longer_blocks_a_start() {
    let mut checker = Checker::new();
    let held = Observations::quiet()
        .at(ms(1_000))
        .with_agent(host(0), holding(1));
    none(&checker.step(&held, &Touched::nothing().agent(host(0))));
    let ended = Observations::quiet()
        .at(ms(1_000))
        .with_agent(host(0), AgentTruth::new(capacity()))
        .with_job_reply(start(1, 2, 500));
    none(&checker.step(&ended, &Touched::nothing().agent(host(0))));
    let mut checker = Checker::new();
    none(&checker.step(&held, &Touched::nothing().agent(host(0))));
    let gone = Observations::quiet()
        .at(ms(1_000))
        .with_job_reply(start(1, 2, 500));
    none(&checker.step(&gone, &Touched::nothing().agent(host(0))));
    let mut checker = Checker::new();
    let both = held.clone().with_job_reply(start(1, 2, 500));
    assert_eq!(
        flagged(&checker.step(&both, &Touched::nothing().agent(host(0)))),
        [Invariant::OneAttemptAtATime]
    );
}

/// Asserts the text of a call made while the job's row waited for another attempt, and
/// of one made when the store held no row, and that a call after the end is not flagged,
/// whatever final state. Catches a callback sent between attempts.
#[test]
fn a_call_before_the_final_end_is_flagged_with_the_state_of_the_row() {
    assert_eq!(
        found(&called_while_queued()),
        [
            "callback-after-the-final-end at 1000: the callback of job sim-job-0 was called at 500 while its row read queued, not ended"
        ]
    );
    let absent = Observations::quiet()
        .at(ms(1_000))
        .with_callback_call(call(0, 500, "w-1", None));
    assert_eq!(
        found(&absent),
        [
            "callback-after-the-final-end at 1000: the callback of job sim-job-0 was called at 500 while its row read absent, not ended"
        ]
    );
    for state in [
        JobState::Succeeded,
        JobState::Lost,
        JobState::Cancelled,
        JobState::Failed,
    ] {
        let seen = Observations::quiet().at(ms(1_000)).with_callback_call(call(
            0,
            500,
            "w-1",
            Some(state),
        ));
        clear(&seen);
    }
}

/// Asserts the text of a second webhook id for one job, and the siblings: a repeated call
/// with the same id (a retry of a failed try), and two jobs with ids of their own. Catches
/// a redelivery under a new id.
#[test]
fn one_webhook_id_to_a_job_and_a_new_one_is_flagged() {
    assert_eq!(
        found(&called_with_two_ids()),
        [
            "callback-after-the-final-end at 1000: the callback of job sim-job-0 was called at 600 with webhook id w-2, after webhook id w-1 at 500"
        ]
    );
    let done = Some(JobState::Succeeded);
    clear(
        &Observations::quiet()
            .at(ms(1_000))
            .with_callback_call(call(0, 500, "w-1", done))
            .with_callback_call(call(0, 600, "w-1", done))
            .with_callback_call(call(1, 700, "w-2", done)),
    );
}
