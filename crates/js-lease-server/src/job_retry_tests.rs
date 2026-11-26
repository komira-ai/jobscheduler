//! The retry decision of a job's attempt that ended: how each end is classed for
//! [`js_types::decide`] ([`attempt_end`]), what follows a job's end under its policy
//! ([`next`]: no policy, a cancel asked for, a job not placed, starting or running, a retrying
//! end with attempts left, the last attempt, an end that never retries, an attempt limit of 0
//! and of 1), the commit of a retry ([`retry`]) and the two settlements of [`end_or_retry`]
//! with what each counts, on the job tables alone.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use js_jobs::{AllocationStep, JobId, NewJob, Output};
use js_types::HostId;

use super::*;
use crate::agent_api::job_rig::{BOOKED, placed, spec, tables};

/// A sink that records the retries and the exhausted attempts it is told of, and counts
/// nothing else.
#[derive(Debug, Default)]
struct Counts {
    retried: Mutex<Vec<RetryOutcome>>,
    exhausted: AtomicU64,
}

impl MetricsSink for Counts {
    fn job_placed(&self, _: Duration) {}
    fn start_failed(&self, _: StartFailure) {}
    fn job_lost(&self) {}
    fn store_write_failed(&self) {}
    fn job_pass_took(&self, _: Duration) {}
    fn job_tables(&self, _: js_sched_placer::engine::TablesFrom) {}
    fn acquire_answered(&self, _: &str, _: &js_elastic::Acquired) {}
    fn job_retried(&self, outcome: RetryOutcome) {
        self.retried.lock().unwrap().push(outcome);
    }
    fn attempts_exhausted(&self) {
        self.exhausted.fetch_add(1, Ordering::SeqCst);
    }
}

impl Counts {
    fn retried(&self) -> Vec<RetryOutcome> {
        self.retried.lock().unwrap().clone()
    }

    fn exhausted(&self) -> u64 {
        self.exhausted.load(Ordering::SeqCst)
    }
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A policy of `max_attempts` attempts with the default waits.
fn policy(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        initial_backoff: None,
        max_backoff: None,
    }
}

fn spec_with(retry: Option<RetryPolicy>) -> js_jobs::JobSpec {
    let mut spec = spec();
    spec.retry = retry;
    spec
}

fn exited(exit_code: i32) -> JobEnd {
    JobEnd::Exited {
        exit_code,
        output: Output::default(),
    }
}

fn refused(failure: StartFailure, detail: &str) -> JobEnd {
    JobEnd::Refused {
        failure,
        detail: detail.to_owned(),
    }
}

fn refused_with(code: &str) -> AttemptEnd {
    AttemptEnd::AgentRefused {
        code: code.to_owned(),
    }
}

/// The job `id` placed on `h1` under `retry`, as a placed job.
fn on_h1(jobs: &ServerJobs, id: &str, retry: Option<RetryPolicy>) -> JobRecord {
    placed(jobs, id, "h1", spec_with(retry), BOOKED).0
}

/// The job `id` placed on `h1` under `retry` and running.
fn running(jobs: &ServerJobs, id: &str, retry: Option<RetryPolicy>) -> JobRecord {
    let job = on_h1(jobs, id, retry);
    jobs.transition(&job.id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&job.id, JobState::Starting, JobState::Running)
        .unwrap()
}

fn reread(jobs: &ServerJobs, id: &JobId) -> JobRecord {
    jobs.get_job(id).unwrap().unwrap()
}

/// Asserts that every end of a job is classed as the retry decision's table has it: an exit
/// by its code, each stop by its cause, a lost host, and each start failure by its kind.
/// Catches an end read as another, which retries what must not or the reverse.
#[test]
fn attempt_end_classes_every_end() {
    let out = Output::default;
    let cases = [
        (exited(0), AttemptEnd::Succeeded),
        (exited(1), AttemptEnd::Exited),
        (exited(-9), AttemptEnd::Exited),
        (
            JobEnd::WallLimit {
                exit_code: Some(0),
                output: out(),
            },
            AttemptEnd::WallLimit,
        ),
        (
            JobEnd::Memory {
                exit_code: None,
                output: out(),
            },
            AttemptEnd::Memory,
        ),
        (
            JobEnd::InputFailed {
                exit_code: Some(3),
                input_failure: "io".to_owned(),
                output: out(),
            },
            AttemptEnd::InputFailed,
        ),
        (
            JobEnd::Cancelled {
                exit_code: None,
                output: out(),
            },
            AttemptEnd::Cancelled,
        ),
        (JobEnd::Lost, AttemptEnd::HostLost),
        (
            JobEnd::FailedToStart(StartFailure::NoCapacity),
            AttemptEnd::NoCapacity,
        ),
        (
            JobEnd::FailedToStart(StartFailure::StartTimeout),
            AttemptEnd::StartTimeout,
        ),
        (
            JobEnd::FailedToStart(StartFailure::ExclusiveNotReady),
            AttemptEnd::ExclusiveNotReady,
        ),
        (
            JobEnd::FailedToStart(StartFailure::InputUnavailable),
            AttemptEnd::InputUnavailable,
        ),
        (
            JobEnd::FailedToStart(StartFailure::AgentRefused),
            refused_with(""),
        ),
    ];
    for (end, class) in cases {
        assert_eq!(attempt_end(&end), class, "{end:?}");
    }
}

/// Asserts that a refusal is classed by the agent's code, the part of its detail before the
/// first colon: a plain code as it is, a requeue-bound detail naming hosts by its code, a
/// detail with no code as the empty code, and the other failures of a refusal by their kind
/// whatever the detail. Catches a code read with its host list, which no table names.
#[test]
fn attempt_end_reads_a_refusal_by_its_code() {
    let cases = [
        (
            refused(StartFailure::AgentRefused, "invalid-job"),
            refused_with("invalid-job"),
        ),
        (
            refused(StartFailure::AgentRefused, "host-held: h1, h2"),
            refused_with("host-held"),
        ),
        (
            refused(StartFailure::AgentRefused, "no-capacity: h1"),
            refused_with("no-capacity"),
        ),
        (
            refused(StartFailure::AgentRefused, ": h1"),
            refused_with(""),
        ),
        (refused(StartFailure::AgentRefused, ""), refused_with("")),
        (
            refused(StartFailure::InputUnavailable, "input-unavailable"),
            AttemptEnd::InputUnavailable,
        ),
        (
            refused(StartFailure::NoCapacity, "no-capacity: h1"),
            AttemptEnd::NoCapacity,
        ),
        (
            refused(StartFailure::StartTimeout, "x"),
            AttemptEnd::StartTimeout,
        ),
        (
            refused(StartFailure::ExclusiveNotReady, "x"),
            AttemptEnd::ExclusiveNotReady,
        ),
    ];
    for (end, class) in cases {
        assert_eq!(attempt_end(&end), class, "{end:?}");
    }
}

/// Asserts that a placed job with a policy of three attempts and a lost host has the wait
/// [`decide`] gives for its first attempt. Catches a wait computed from the wrong attempt,
/// id or end.
#[test]
fn next_retries_with_the_wait_decide_gives() {
    let (_store, jobs) = tables();
    let job = on_h1(&jobs, "job-a", Some(policy(3)));
    let class = AttemptEnd::HostLost;
    let Decision::Retry { wait } = decide(&policy(3), &class, 1, &job.id) else {
        panic!("a lost host retries");
    };
    assert_eq!(next(&job, &JobEnd::Lost), Next::Retry { wait });
    assert!(wait >= Duration::from_secs(10), "{wait:?}");
}

/// Asserts that the three live states retry alike. Catches a state left out of the live set.
#[test]
fn next_retries_a_placed_starting_and_running_job() {
    let (_store, jobs) = tables();
    let job = on_h1(&jobs, "job-live", Some(policy(2)));
    let end = JobEnd::FailedToStart(StartFailure::StartTimeout);
    let want = |job: &JobRecord| {
        let Decision::Retry { wait } = decide(&policy(2), &AttemptEnd::StartTimeout, 1, &job.id)
        else {
            panic!("a start timeout retries");
        };
        Next::Retry { wait }
    };
    assert_eq!(job.state, JobState::Placed);
    assert_eq!(next(&job, &end), want(&job));
    let starting = jobs
        .transition(&job.id, JobState::Placed, JobState::Starting)
        .unwrap();
    assert_eq!(next(&starting, &end), want(&job));
    let running = jobs
        .transition(&job.id, JobState::Starting, JobState::Running)
        .unwrap();
    assert_eq!(next(&running, &end), want(&job));
}

/// Asserts that a job with no policy ends, with no attempts counted as run out. Catches a
/// job with no policy retried or counted as exhausted.
#[test]
fn next_ends_a_job_with_no_policy() {
    let (_store, jobs) = tables();
    let job = on_h1(&jobs, "job-none", None);
    assert_eq!(next(&job, &JobEnd::Lost), Next::End { exhausted: false });
}

/// Asserts that a cancel asked for beats a retry, and the end is no exhaustion. Catches a
/// cancelled job queued again.
#[test]
fn next_ends_a_job_whose_cancel_was_asked_for() {
    let (_store, jobs) = tables();
    let job = running(&jobs, "job-cancel", Some(policy(3)));
    let asked = jobs.request_cancel(&job.id).unwrap();
    assert!(asked.cancel_requested);
    assert_eq!(asked.state, JobState::Running);
    assert_eq!(next(&asked, &JobEnd::Lost), Next::End { exhausted: false });
}

/// Asserts that a job that is queued, or already ended (a late report of an attempt that was
/// retried or ended), ends with no exhaustion. Catches a late report queuing a second
/// attempt.
#[test]
fn next_ends_a_job_that_is_not_live() {
    let (_store, jobs) = tables();
    let id = JobId::parse("job-queued").unwrap();
    jobs.insert_queued(NewJob {
        id: Some(id.clone()),
        spec: spec_with(Some(policy(3))),
        priority: 0,
        client: None,
    })
    .unwrap();
    let queued = reread(&jobs, &id);
    assert_eq!(queued.state, JobState::Queued);
    assert_eq!(next(&queued, &JobEnd::Lost), Next::End { exhausted: false });

    let job = running(&jobs, "job-over", Some(policy(3)));
    let over = jobs.end_job(&job.id, exited(1)).unwrap();
    assert!(over.state.is_ended());
    assert_eq!(next(&over, &JobEnd::Lost), Next::End { exhausted: false });
}

/// Asserts that the last attempt of a retrying end ends the job and says its attempts ran
/// out: a policy of one attempt and of none.
/// Catches the end of a retrying end left uncounted, or an attempt over the limit retried.
#[test]
fn next_ends_the_last_attempt_as_exhausted() {
    let (_store, jobs) = tables();
    for (name, max) in [("job-one", 1), ("job-zero", 0)] {
        let job = on_h1(&jobs, name, Some(policy(max)));
        assert_eq!(next(&job, &JobEnd::Lost), Next::End { exhausted: true });
    }
}

/// Asserts that an end that never retries ends the job with attempts left and is not
/// exhaustion, and that a refusal naming the job's own fault does not retry either. Catches
/// an exit or a job's fault counted as attempts run out.
#[test]
fn next_ends_an_end_that_never_retries() {
    let (_store, jobs) = tables();
    let job = on_h1(&jobs, "job-never", Some(policy(5)));
    let ends = [
        exited(1),
        exited(0),
        JobEnd::FailedToStart(StartFailure::NoCapacity),
        JobEnd::FailedToStart(StartFailure::ExclusiveNotReady),
        refused(StartFailure::AgentRefused, "invalid-job"),
        refused(StartFailure::AgentRefused, "invalid-job: why"),
    ];
    for end in ends {
        assert_eq!(next(&job, &end), Next::End { exhausted: false }, "{end:?}");
    }
}

/// Asserts that refusals of the host's cause retry, with a code this server does not know, with
/// none and with hosts named after the code, and an input that could not be read does too.
/// Catches a code read whole so a requeue-bound refusal never retries.
#[test]
fn next_retries_a_refusal_of_the_hosts_cause() {
    let (_store, jobs) = tables();
    let job = on_h1(&jobs, "job-host", Some(policy(5)));
    let ends = [
        refused(StartFailure::AgentRefused, "image-pull-failed"),
        refused(StartFailure::AgentRefused, "host-held: h1, h2"),
        refused(StartFailure::AgentRefused, ""),
        refused(StartFailure::InputUnavailable, "input-unavailable"),
        JobEnd::FailedToStart(StartFailure::AgentRefused),
        JobEnd::FailedToStart(StartFailure::InputUnavailable),
    ];
    for end in ends {
        assert!(matches!(next(&job, &end), Next::Retry { .. }), "{end:?}");
    }
}

/// Asserts that a retry queues the job, puts the ended attempt into its history with its
/// host, makes the next attempt placeable the wait after the store's now, moves the
/// allocation to releasing and counts a lost host. Catches a retry that does not queue, or
/// counts as a start failure.
#[test]
fn end_or_retry_retries_a_lost_host() {
    let (_store, jobs) = tables();
    let metrics = Counts::default();
    let job = running(&jobs, "job-lost", Some(policy(3)));
    let now = jobs.now().unwrap();
    let Decision::Retry { wait } = decide(&policy(3), &AttemptEnd::HostLost, 1, &job.id) else {
        panic!("a lost host retries");
    };
    let settled = end_or_retry(&jobs, &job, JobEnd::Lost, &metrics).unwrap();
    let Settled::Retried(queued) = settled else {
        panic!("retried, not {settled:?}");
    };
    assert_eq!(queued.state, JobState::Queued);
    assert_eq!(queued.host, None);
    assert_eq!(queued.attempt.number(), 1);
    assert_eq!(
        queued.attempt.retry_not_before(),
        Some(now.saturating_add(wait))
    );
    assert_eq!(queued.attempt.history().len(), 1);
    let ended = &queued.attempt.history()[0];
    assert_eq!(ended.host, host("h1"));
    assert!(ended.retried);
    assert_eq!(ended.end, AttemptRecord::of_end(&JobEnd::Lost));
    assert!(queued.attempt.hosts().contains(&host("h1")));
    assert_eq!(reread(&jobs, &job.id).state, JobState::Queued);
    let allocation = jobs.get_allocation(&job.id).unwrap().unwrap();
    assert_eq!(allocation.step, AllocationStep::Releasing);
    assert_eq!(metrics.retried(), vec![RetryOutcome::HostLost]);
    assert_eq!(metrics.exhausted(), 0);
}

/// Asserts that a start that failed for a cause of the host retries and is counted as a start
/// failure, for a start timeout of a placed job and a refusal of a running one. Catches a
/// start failure counted as a lost host.
#[test]
fn end_or_retry_retries_a_start_failure() {
    let (_store, jobs) = tables();
    let metrics = Counts::default();
    let timed_out = on_h1(&jobs, "job-timeout", Some(policy(2)));
    let end = JobEnd::FailedToStart(StartFailure::StartTimeout);
    let settled = end_or_retry(&jobs, &timed_out, end.clone(), &metrics).unwrap();
    assert!(matches!(settled, Settled::Retried(_)), "{settled:?}");
    assert_eq!(
        reread(&jobs, &timed_out.id).attempt.history()[0].end,
        AttemptRecord::of_end(&end)
    );

    let held = running(&jobs, "job-refused", Some(policy(2)));
    let end = refused(StartFailure::AgentRefused, "host-held: h1, h2");
    let settled = end_or_retry(&jobs, &held, end, &metrics).unwrap();
    assert!(matches!(settled, Settled::Retried(_)), "{settled:?}");
    assert_eq!(
        reread(&jobs, &held.id).attempt.history()[0]
            .end
            .start_failure_detail,
        Some("host-held: h1, h2".to_owned())
    );
    assert_eq!(
        metrics.retried(),
        vec![RetryOutcome::StartFailed, RetryOutcome::StartFailed]
    );
    assert_eq!(metrics.exhausted(), 0);
}

/// Asserts that a retrying end on the last attempt ends the job as the end says, counts the
/// exhausted attempts once and no retry. Catches an exhausted job left uncounted, or counted
/// twice.
#[test]
fn end_or_retry_ends_and_counts_an_exhausted_job() {
    let (_store, jobs) = tables();
    let metrics = Counts::default();
    let job = running(&jobs, "job-spent", Some(policy(1)));
    let settled = end_or_retry(&jobs, &job, JobEnd::Lost, &metrics).unwrap();
    let Settled::Ended(ended) = settled else {
        panic!("ended, not {settled:?}");
    };
    assert_eq!(ended.state, JobState::Lost);
    assert_eq!(reread(&jobs, &job.id).state, JobState::Lost);
    assert_eq!(metrics.exhausted(), 1);
    assert_eq!(metrics.retried(), Vec::<RetryOutcome>::new());
}

/// Asserts that an end that never retries ends the job with nothing counted, and so does an end
/// of a job with no policy. Catches an exit counted as exhaustion.
#[test]
fn end_or_retry_counts_nothing_for_an_end_that_is_not_exhaustion() {
    let (_store, jobs) = tables();
    let metrics = Counts::default();
    let exits = running(&jobs, "job-exit", Some(policy(3)));
    let settled = end_or_retry(&jobs, &exits, exited(1), &metrics).unwrap();
    let Settled::Ended(ended) = settled else {
        panic!("ended, not {settled:?}");
    };
    assert_eq!(ended.state, JobState::Failed);

    let bare = running(&jobs, "job-bare", None);
    let settled = end_or_retry(&jobs, &bare, JobEnd::Lost, &metrics).unwrap();
    let Settled::Ended(ended) = settled else {
        panic!("ended, not {settled:?}");
    };
    assert_eq!(ended.state, JobState::Lost);
    assert_eq!(metrics.exhausted(), 0);
    assert_eq!(metrics.retried(), Vec::<RetryOutcome>::new());
}

/// Asserts that an end another writer made first keeps that end and is not counted as
/// exhaustion by this one. Catches a double count of one job's end.
#[test]
fn end_or_retry_does_not_count_an_end_another_writer_made() {
    let (_store, jobs) = tables();
    let metrics = Counts::default();
    let job = running(&jobs, "job-raced", Some(policy(1)));
    jobs.end_job(&job.id, exited(1)).unwrap();
    let settled = end_or_retry(&jobs, &job, JobEnd::Lost, &metrics).unwrap();
    let Settled::Ended(ended) = settled else {
        panic!("ended, not {settled:?}");
    };
    assert_eq!(ended.state, JobState::Failed);
    assert_eq!(metrics.exhausted(), 0);
    assert_eq!(metrics.retried(), Vec::<RetryOutcome>::new());
}

/// Asserts that a retry of a job that is not live, or has no attempt left, is refused as an
/// illegal move to queued, writes nothing and counts nothing. Catches a retry of a job that
/// ended, which would resurrect it.
#[test]
fn retry_of_a_job_that_cannot_retry_is_refused() {
    let (_store, jobs) = tables();
    let metrics = Counts::default();
    let job = running(&jobs, "job-done", Some(policy(3)));
    let over = jobs.end_job(&job.id, exited(1)).unwrap();
    let err = retry(
        &jobs,
        &over,
        &JobEnd::Lost,
        Duration::from_secs(10),
        &metrics,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            JobStoreError::IllegalTransition {
                to: JobState::Queued,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(reread(&jobs, &job.id).state, JobState::Failed);

    let spent = running(&jobs, "job-spent", Some(policy(1)));
    let err = retry(
        &jobs,
        &spent,
        &JobEnd::Lost,
        Duration::from_secs(10),
        &metrics,
    )
    .unwrap_err();
    assert!(
        matches!(err, JobStoreError::IllegalTransition { .. }),
        "{err:?}"
    );
    assert_eq!(reread(&jobs, &spent.id).state, JobState::Running);
    assert_eq!(metrics.retried(), Vec::<RetryOutcome>::new());
}

/// Asserts that a commit that fails is the caller's error and counts nothing, for a retry and
/// for an end that would be counted exhausted. Catches a count made before the commit.
#[test]
fn end_or_retry_passes_on_a_store_failure_uncounted() {
    let (store, jobs) = tables();
    let metrics = Counts::default();
    let retrying = running(&jobs, "job-fail-retry", Some(policy(3)));
    store.fail_next();
    let err = end_or_retry(&jobs, &retrying, JobEnd::Lost, &metrics).unwrap_err();
    assert!(matches!(err, JobStoreError::Store(_)), "{err:?}");
    assert_eq!(reread(&jobs, &retrying.id).state, JobState::Running);

    let spent = running(&jobs, "job-fail-end", Some(policy(1)));
    store.fail_next();
    let err = end_or_retry(&jobs, &spent, JobEnd::Lost, &metrics).unwrap_err();
    assert!(matches!(err, JobStoreError::Store(_)), "{err:?}");
    assert_eq!(reread(&jobs, &spent.id).state, JobState::Running);
    assert_eq!(metrics.retried(), Vec::<RetryOutcome>::new());
    assert_eq!(metrics.exhausted(), 0);
}

/// Asserts that a store whose clock cannot be read fails a retry with the store's error before
/// anything is written. Catches a retry made with a made-up time.
#[test]
fn retry_passes_on_a_clock_that_cannot_be_read() {
    let (store, jobs) = tables();
    let metrics = Counts::default();
    let job = running(&jobs, "job-clock", Some(policy(3)));
    store.fail_clock(true);
    let err = end_or_retry(&jobs, &job, JobEnd::Lost, &metrics).unwrap_err();
    store.fail_clock(false);
    assert!(matches!(err, JobStoreError::Store(_)), "{err:?}");
    assert_eq!(reread(&jobs, &job.id).state, JobState::Running);
    assert_eq!(metrics.retried(), Vec::<RetryOutcome>::new());
}
