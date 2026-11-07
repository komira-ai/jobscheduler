//! The driver against a simulated farm on a fake clock, and against a farm that answers
//! as the test says.

use js_measure::{FakeClock, FakeFarm, FarmError, HostShape, drive};

use crate::stub::{Stub, ended, refused, standing};
use crate::support::{GIB, T0, job};

fn one_host(clock: &FakeClock) -> FakeFarm<'_> {
    let shape = HostShape {
        cpu_millis: 32_000,
        memory_bytes: 128 * GIB,
    };
    FakeFarm::new(clock, vec![("h1".to_owned(), shape)])
}

/// Asserts what the driver notes for one job on a free host: the job is submitted at the
/// first poll, seen placed and running at the first poll after it (so a time is good to a
/// poll), and over at the poll after its wall time; and that it ends there.
#[test]
fn one_job_on_a_free_host() {
    let clock = FakeClock::at(T0);
    let farm = one_host(&clock);
    let obs = drive(&farm, &clock, &[job("a", 60_000)], 10_000, 3_600_000);
    let a = &obs.jobs[0];
    assert_eq!((a.requested_ms, a.placed_ms), (Some(T0), Some(T0 + 10_000)));
    assert_eq!(
        (a.started_ms, a.finished_ms),
        (Some(T0 + 10_000), Some(T0 + 60_000))
    );
    assert_eq!(a.host.as_deref(), Some("h1"));
    assert_eq!(a.outcome.as_deref(), Some("succeeded"));
    assert_eq!(a.failure, None);
    assert_eq!((obs.t0_ms, obs.end_ms), (T0, T0 + 60_000));
}

/// Asserts that a second job on a one-host farm waits for the first: it is seen placed
/// at the poll that sees the first job over, and its queue time is the first job's run.
/// It catches a driver that does not poll a queued job.
#[test]
fn a_second_job_waits_for_the_host() {
    let clock = FakeClock::at(T0);
    let farm = one_host(&clock);
    let jobs = [job("a", 60_000), job("b", 60_000)];
    let obs = drive(&farm, &clock, &jobs, 10_000, 3_600_000);
    let (a, b) = (&obs.jobs[0], &obs.jobs[1]);
    assert_eq!(a.finished_ms, Some(T0 + 60_000));
    assert_eq!(b.requested_ms, Some(T0));
    assert_eq!(b.placed_ms, Some(T0 + 60_000));
    assert_eq!(b.started_ms, Some(T0 + 60_000));
    assert_eq!(b.finished_ms, Some(T0 + 120_000));
}

/// Asserts that a job planned for later is not submitted before its time, and that the
/// planned time counts from the start of the drive.
#[test]
fn a_job_is_submitted_at_its_planned_time() {
    let clock = FakeClock::at(T0);
    let farm = one_host(&clock);
    let mut late = job("late", 10_000);
    late.submit_at_ms = 25_000;
    let obs = drive(&farm, &clock, &[late], 10_000, 3_600_000);
    let late = &obs.jobs[0];
    assert_eq!(late.submit_at_ms, T0 + 25_000);
    assert_eq!(late.requested_ms, Some(T0 + 30_000));
}

/// Asserts that the time from the submit to the placed job is what the submit took plus
/// the wait for the poll that saw the job placed.
#[test]
fn submit_to_placed_includes_the_time_the_answer_took() {
    let clock = FakeClock::at(T0);
    let farm = one_host(&clock).with_submit_latency_ms(500);
    let obs = drive(&farm, &clock, &[job("a", 20_000)], 10_000, 3_600_000);
    assert_eq!(obs.jobs[0].requested_ms, Some(T0));
    assert_eq!(obs.jobs[0].placed_ms, Some(T0 + 10_500));
}

/// Asserts that a refusal ends that job with the server's code in its failure, without a
/// cancel (there is no job), and that the drive ends at once.
#[test]
fn a_refused_submit_is_a_failure() {
    let farm = Stub {
        submit: Err(refused()),
        ..Stub::running()
    };
    let clock = FakeClock::at(T0);
    let obs = drive(&farm, &clock, &[job("a", 1)], 10, 1_000);
    assert_eq!(
        obs.jobs[0].failure.as_deref(),
        Some("the server answered 409 no-capacity")
    );
    assert_eq!(obs.jobs[0].placed_ms, None);
    assert!(farm.cancelled.borrow().is_empty());
}

/// Asserts that a job that cannot be read after it was submitted fails with the server's
/// error and is cancelled, so it does not go on holding a host.
#[test]
fn a_job_that_cannot_be_read_is_cancelled() {
    let clock = FakeClock::at(T0);
    let unreadable = Stub {
        job: Err(FarmError::Transport("down".to_owned())),
        ..Stub::running()
    };
    let obs = drive(&unreadable, &clock, &[job("a", 1)], 10, 1_000);
    assert_eq!(
        obs.jobs[0].failure.as_deref(),
        Some("cannot reach the server: down")
    );
    assert_eq!(obs.jobs[0].placed_ms, None);
    assert_eq!(*unreadable.cancelled.borrow(), vec!["j1".to_owned()]);
}

/// Asserts that a job that is over at the first poll that sees it, never seen running, has
/// no start (not a guessed one), that it is placed at that poll when it names a host, and
/// that the outcome the server gave stands.
#[test]
fn a_job_never_seen_running_has_no_start() {
    let clock = FakeClock::at(T0);
    let farm = Stub {
        job: Ok(ended("JOB_STATE_FAILED", "failed")),
        ..Stub::running()
    };
    let obs = drive(&farm, &clock, &[job("a", 1)], 10, 1_000);
    let a = &obs.jobs[0];
    assert_eq!(
        (a.placed_ms, a.started_ms, a.finished_ms),
        (Some(T0 + 10), None, Some(T0 + 10))
    );
    assert_eq!(a.outcome.as_deref(), Some("failed"));
    assert_eq!(a.host.as_deref(), Some("h1"));
    assert!(farm.cancelled.borrow().is_empty());
}

/// Asserts that a job that ends without ever being placed (it failed to start) has no
/// placed time and no host, and keeps the outcome.
#[test]
fn a_job_that_never_got_a_host_has_none() {
    let clock = FakeClock::at(T0);
    let farm = Stub {
        job: Ok(standing("JOB_STATE_FAILED_TO_START", None)),
        ..Stub::running()
    };
    let obs = drive(&farm, &clock, &[job("a", 1)], 10, 1_000);
    let a = &obs.jobs[0];
    assert_eq!((a.placed_ms, a.host.clone()), (None, None));
    assert_eq!(a.finished_ms, Some(T0 + 10));
}

/// Asserts the deadline: a job that never ends, a job that stays queued and a job that
/// was never due are each given up with `deadline`, the submitted ones cancelled, and the
/// drive ends at the deadline.
#[test]
fn open_jobs_are_given_up_at_the_deadline() {
    let clock = FakeClock::at(T0);
    let forever = Stub::running();
    let mut never_due = job("later", 1);
    never_due.submit_at_ms = 10_000_000;
    let obs = drive(&forever, &clock, &[job("a", 1), never_due], 10_000, 30_000);
    assert_eq!(obs.jobs[0].failure.as_deref(), Some("deadline"));
    assert_eq!(obs.jobs[0].started_ms, Some(T0 + 10_000));
    assert_eq!(obs.jobs[1].failure.as_deref(), Some("deadline"));
    assert_eq!(obs.jobs[1].requested_ms, None);
    assert_eq!(*forever.cancelled.borrow(), vec!["j1".to_owned()]);
    assert_eq!(obs.end_ms, T0 + 30_000);

    let queued = Stub::queued();
    let obs = drive(&queued, &clock, &[job("a", 1)], 10_000, 20_000);
    assert_eq!(obs.jobs[0].failure.as_deref(), Some("deadline"));
    assert_eq!(obs.jobs[0].started_ms, None);
    assert_eq!(obs.jobs[0].placed_ms, None);
    assert_eq!(*queued.cancelled.borrow(), vec!["j1".to_owned()]);
}

/// Asserts that giving up at the deadline leaves a job that is already over as it was:
/// its outcome stands, only the job still running is marked `deadline` and cancelled.
#[test]
fn a_finished_job_is_not_marked_at_the_deadline() {
    let clock = FakeClock::at(T0);
    let farm = one_host(&clock);
    let jobs = [job("short", 5_000), job("long", 10_000_000)];
    let obs = drive(&farm, &clock, &jobs, 10_000, 60_000);
    assert_eq!(obs.jobs[0].failure, None);
    assert_eq!(obs.jobs[0].outcome.as_deref(), Some("succeeded"));
    assert_eq!(obs.jobs[1].failure.as_deref(), Some("deadline"));
    assert_eq!(obs.jobs[1].finished_ms, None);
}

/// Asserts that a cancel the server refuses is noted on the job, whichever way the job
/// was given up (unreadable, past the deadline), and that the failure stands. Without the
/// note a job that still holds its host makes the next run's minutes shared, and null,
/// with no word of why.
#[test]
fn a_failed_cancel_is_noted_on_the_job() {
    let clock = FakeClock::at(T0);
    let unreadable = Stub {
        job: Err(FarmError::Transport("down".to_owned())),
        cancel_fails: true,
        ..Stub::running()
    };
    let obs = drive(&unreadable, &clock, &[job("a", 1)], 10, 1_000);
    assert_eq!(
        obs.jobs[0].failure.as_deref(),
        Some(
            "cannot reach the server: down; cancel of j1 failed: the server answered 409 no-capacity"
        )
    );

    let forever = Stub {
        cancel_fails: true,
        ..Stub::running()
    };
    let obs = drive(&forever, &clock, &[job("a", 1)], 10_000, 20_000);
    assert_eq!(
        obs.jobs[0].failure.as_deref(),
        Some("deadline; cancel of j1 failed: the server answered 409 no-capacity")
    );
}
