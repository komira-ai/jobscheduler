//! The retry family: what a job that asks for more than one attempt and a completion
//! callback goes through in the cell, over the real server and its job service, each
//! scenario over [`SEEDS`] seeds under every invariant of `Invariant::ALL` (the three
//! of retry and the callback among them): a crash between an attempt's end and its retry, a
//! lost whole-host attempt, the callback of a retried job at each final state, a restart
//! with callbacks due, a cancel racing a retry, a late result of an earlier attempt, and a
//! callback whose claim lapsed.
//!
//! Each states what the design promises and the server as built does, with a witness or a
//! statement that shows the situation happened, and the outcome with [`Expect`]s. Two
//! negative cases follow the scenarios: agents that forget a result the server has not
//! acknowledged fail the first, and the run's own observations, changed in one place, are
//! flagged by each of the three new invariants.
//!
//! The scenarios and their sizes are in `cell/retry.rs`. Each runs in the pull request's
//! `test` and `coverage` jobs, so none is large: a handful of hosts, and about 3,000 virtual
//! seconds for the longest (the late result, whose start timeout is 15 minutes).

#[path = "cell/retry.rs"]
mod retry;

use std::path::Path;
use std::time::Duration;

use js_conformance::agent::Defect;
use js_jobs::JobState;
use js_sim::Faults;
use js_sim_cell::Action;
use js_sim_cell::actions::kill_agents_mid_job;
use js_sim_cell::{
    AgentTruth, Cell, Checker, Invariant, JobFront, Observations, ScenarioFailure, ServerSpec,
    Setup, Shape, Touched, Work,
};
use js_testkit::dir::TestDir;
use js_types::Resources;
use js_types::sched::FarmTime;

/// Every scenario of the family runs over this many seeds.
const SEEDS: u64 = 8;

/// Covers the retry design's row for a crash between an attempt's end and its retry: the
/// server is stopped between the moment an agent makes the result of a refused start and the
/// moment the server acknowledges it, and one retry is made after the restart.
#[test]
fn restart_between_attempts() {
    retry::restart_between_attempts().run_seeds(SEEDS);
}

/// A whole-host attempt lost while it runs is retried, its failed prepares counted again
/// from zero.
#[test]
fn whole_host_retry() {
    retry::whole_host_retry().run_seeds(SEEDS);
}

/// A retried job gets one callback at each final state, after its last attempt.
#[test]
fn one_callback_after_retries_are_exhausted() {
    retry::one_callback_after_retries_are_exhausted().run_seeds(SEEDS);
}

/// Callbacks due while the server is down are delivered after it restarts.
#[test]
fn restart_with_due_callbacks() {
    retry::restart_with_due_callbacks().run_seeds(SEEDS);
}

/// A cancel racing a retry never lets a job be tried again.
#[test]
fn a_cancel_racing_a_retry_ends_cancelled() {
    retry::a_cancel_racing_a_retry_ends_cancelled().run_seeds(SEEDS);
}

/// A late result of an earlier attempt is answered stale.
#[test]
fn a_late_result_of_an_earlier_attempt_is_stale() {
    retry::a_late_result_of_an_earlier_attempt_is_stale().run_seeds(SEEDS);
}

/// A callback whose claim lapsed is delivered by another instance.
#[test]
fn a_lapsed_callback_claim_is_delivered_by_another_instance() {
    retry::a_lapsed_callback_claim_is_delivered_by_another_instance().run_seeds(SEEDS);
}

/// Asserts that `restart_between_attempts` with agents that forget a result the server has
/// not acknowledged (`Defect::ForgetsUnackedResult`) fails `result-kept-until-acknowledged`
/// at the first seed, and that the report names that seed. The server is down for longer
/// than an agent keeps its connection, so such an agent drops the refusal's result before the
/// server is back to acknowledge it. Catches a scenario whose restart never leaves a result
/// waiting for an acknowledgement.
#[test]
fn agents_that_forget_an_unacknowledged_result_fail_the_restart_between_attempts() {
    let broken = retry::restart_between_attempts_of(Some(Defect::ForgetsUnackedResult));
    let failure = broken
        .try_run_seeds(SEEDS)
        .expect_err("agents that forget a result fail");
    let ScenarioFailure::Violated(report) = failure else {
        panic!("not a violation: {failure}");
    };
    assert_eq!(
        (report.seed, report.violation.invariant),
        (0, Invariant::ResultKeptUntilAcknowledged)
    );
}

/// A cell of two correct agents of a registered pool, its client the job front, in which a
/// job that asks for two attempts and a callback has its first attempt lost while it runs,
/// is tried again on the other host and succeeds: its observations after 400 s.
fn a_lost_attempt_run(name: &str) -> Cell {
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell"),
        name,
    );
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), 2);
    let mut cell = Cell::new(Setup::new(5), spec, dir, Faults::default()).expect("the cell starts");
    cell.set_front(Box::new(JobFront::new()));
    for _ in 0..2 {
        cell.add_agent().expect("an agent name is a host id");
    }
    cell.run_until(FarmTime::from_millis(1_000))
        .expect("no violation");
    cell.submit(&Work::run_for(Duration::from_secs(60)).retry(2).callback());
    cell.run_until(FarmTime::from_millis(30_000))
        .expect("no violation");
    kill_agents_mid_job(1, Duration::from_secs(10)).run(&mut cell);
    cell.run_until(FarmTime::from_millis(400_000))
        .expect("no violation");
    cell
}

/// The invariants that flag `seen`, each once, after one step that touched everything.
fn flagged(seen: &Observations) -> Vec<Invariant> {
    let found = Checker::new().step(seen, &Touched::everything_in(seen));
    let mut invariants: Vec<Invariant> = found.iter().map(|found| found.invariant).collect();
    invariants.dedup();
    invariants
}

/// Asserts that the run of a job whose first attempt was lost and whose second succeeded
/// flags nothing as it is, and that each of the three invariants of retry and the callback
/// flags the run changed in one place: the policy read as one attempt after the job was
/// launched under two allocations (`attempts-bounded`), a start of the second attempt read
/// while the host of the first still holds it (`one-attempt-at-a-time`), and the receiver's
/// call made while the job's row had not ended (`callback-after-the-final-end`). Catches an
/// invariant that the scenarios' runs never give anything to judge.
#[test]
fn the_run_of_a_retried_job_gives_each_new_invariant_something_to_flag() {
    let cell = a_lost_attempt_run("retry_flagged_runs");
    let seen = cell.observations().clone();
    assert_eq!(flagged(&seen), []);
    let job = seen.attempt_reads[0].job.clone();
    assert_eq!(cell.ledger().launches().len(), 2);

    let one_attempt = seen.clone().with_attempt_read(job.clone(), 1, 1, seen.at);
    assert_eq!(flagged(&one_attempt), [Invariant::AttemptsBounded]);

    let starts: Vec<_> = seen
        .job_replies
        .iter()
        .flat_map(|reply| reply.starts.iter().map(move |start| (reply, start)))
        .filter(|(_, start)| !start.refused)
        .collect();
    let [(first, first_start), (second, _)] = starts.as_slice() else {
        panic!("two starts were read: {starts:?}");
    };
    assert_ne!(first.host, second.host);
    let capacity = seen.agents[&first.host].capacity.clone();
    let resources = Resources::new(1_000, 1 << 30, 0, None).expect("resources");
    let holding = AgentTruth::new(capacity).with_job(job, first_start.allocation, resources);
    let beside = seen.clone().with_agent(first.host.clone(), holding);
    assert_eq!(flagged(&beside), [Invariant::OneAttemptAtATime]);

    let mut early = seen.clone();
    early.callback_calls[0].ended = false;
    early.callback_calls[0].state = Some(JobState::Queued);
    assert_eq!(flagged(&early), [Invariant::CallbackAfterTheFinalEnd]);
}
