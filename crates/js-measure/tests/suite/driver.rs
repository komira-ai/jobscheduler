//! The driver against a simulated farm on a fake clock, and against a farm that answers
//! as the test says.

use js_measure::farm::{RunStanding, Standing};
use js_measure::{Acquired, FakeClock, FakeFarm, FarmError, HostShape, drive};

use crate::stub::{Stub, refused, running};
use crate::support::{GIB, T0, job};

fn one_host(clock: &FakeClock) -> FakeFarm<'_> {
    let shape = HostShape {
        cpu_millis: 32_000,
        memory_bytes: 128 * GIB,
    };
    FakeFarm::new(clock, vec![("h1".to_owned(), shape)])
}

/// Asserts what the driver notes for one job on a free host: the lease is held at the
/// request, the run is seen running at the first poll after it (so a time is good to a
/// poll), and over at the poll after its wall time; and that it ends there.
#[test]
fn one_job_on_a_free_host() {
    let clock = FakeClock::at(T0);
    let farm = one_host(&clock);
    let obs = drive(&farm, &clock, &[job("a", 60_000, None)], 10_000, 3_600_000);
    let a = &obs.jobs[0];
    assert_eq!((a.requested_ms, a.granted_ms), (Some(T0), Some(T0)));
    assert_eq!(
        (a.started_ms, a.finished_ms),
        (Some(T0 + 10_000), Some(T0 + 60_000))
    );
    assert_eq!(a.host.as_deref(), Some("h1"));
    assert_eq!(a.outcome.as_deref(), Some("succeeded"));
    assert_eq!(a.failure, None);
    assert_eq!((obs.t0_ms, obs.end_ms), (T0, T0 + 60_000));
}

/// Asserts that a second job on a one-host farm waits for the first: its lease is
/// granted at the poll that sees the first release, and its queue time is the first
/// job's run. It catches a driver that does not poll a pending lease.
#[test]
fn a_second_job_waits_for_the_host() {
    let clock = FakeClock::at(T0);
    let farm = one_host(&clock);
    let jobs = [job("a", 60_000, None), job("b", 60_000, None)];
    let obs = drive(&farm, &clock, &jobs, 10_000, 3_600_000);
    let (a, b) = (&obs.jobs[0], &obs.jobs[1]);
    assert_eq!(a.finished_ms, Some(T0 + 60_000));
    assert_eq!(b.requested_ms, Some(T0));
    assert_eq!(b.granted_ms, Some(T0 + 60_000));
    assert_eq!(b.started_ms, Some(T0 + 70_000));
    assert_eq!(b.finished_ms, Some(T0 + 120_000));
}

/// Asserts that a job planned for later is not submitted before its time, and that the
/// planned time counts from the start of the drive.
#[test]
fn a_job_is_submitted_at_its_planned_time() {
    let clock = FakeClock::at(T0);
    let farm = one_host(&clock);
    let mut late = job("late", 10_000, None);
    late.submit_at_ms = 25_000;
    let obs = drive(&farm, &clock, &[late], 10_000, 3_600_000);
    let late = &obs.jobs[0];
    assert_eq!(late.submit_at_ms, T0 + 25_000);
    assert_eq!(late.requested_ms, Some(T0 + 30_000));
}

/// Asserts that the time from the request to the held lease is what the request took.
#[test]
fn request_to_token_is_the_time_the_answer_took() {
    let clock = FakeClock::at(T0);
    let farm = one_host(&clock).with_acquire_latency_ms(500);
    let obs = drive(&farm, &clock, &[job("a", 20_000, None)], 10_000, 3_600_000);
    assert_eq!(obs.jobs[0].requested_ms, Some(T0));
    assert_eq!(obs.jobs[0].granted_ms, Some(T0 + 500));
}

/// Asserts that a refusal ends that job with the server's code in its failure, without a
/// release (there is no lease), and that the drive ends at once.
#[test]
fn a_refused_request_is_a_failure() {
    let farm = Stub {
        acquire: Err(refused()),
        ..Stub::held()
    };
    let clock = FakeClock::at(T0);
    let obs = drive(&farm, &clock, &[job("a", 1, None)], 10, 1_000);
    assert_eq!(
        obs.jobs[0].failure.as_deref(),
        Some("the server answered 409 no-capacity")
    );
    assert_eq!(obs.jobs[0].granted_ms, None);
    assert!(farm.released.borrow().is_empty());
}

/// Asserts the failures after the lease exists: a pending lease that vanishes, a pending
/// lease whose poll fails, a run that cannot be queued and a run that cannot be read. The
/// lease is released in each case but the first, where the server no longer has it.
#[test]
fn a_failure_after_the_lease_releases_it() {
    let clock = FakeClock::at(T0);
    let gone = Stub {
        lease: Ok(Standing::Gone),
        ..Stub::pending()
    };
    let obs = drive(&gone, &clock, &[job("a", 1, None)], 10, 1_000);
    assert_eq!(obs.jobs[0].failure.as_deref(), Some("lease-gone"));
    assert!(gone.released.borrow().is_empty());

    let poll_fails = Stub {
        lease: Err(refused()),
        ..Stub::pending()
    };
    let obs = drive(&poll_fails, &clock, &[job("a", 1, None)], 10, 1_000);
    assert!(obs.jobs[0].failure.is_some());
    assert_eq!(*poll_fails.released.borrow(), vec!["l1".to_owned()]);

    let no_start = Stub {
        start: Err(FarmError::Transport("down".to_owned())),
        ..Stub::held()
    };
    let obs = drive(&no_start, &clock, &[job("a", 1, None)], 10, 1_000);
    assert_eq!(
        obs.jobs[0].failure.as_deref(),
        Some("cannot reach the server: down")
    );
    assert!(obs.jobs[0].granted_ms.is_some());
    assert_eq!(*no_start.released.borrow(), vec!["l1".to_owned()]);

    let no_read = Stub {
        run: Err(refused()),
        ..Stub::held()
    };
    let obs = drive(&no_read, &clock, &[job("a", 1, None)], 10, 1_000);
    assert!(obs.jobs[0].failure.is_some());
    assert_eq!(*no_read.released.borrow(), vec!["l1".to_owned()]);
}

/// Asserts that a lease that becomes held after pending is granted at the poll that saw
/// it, and that a run that is over at the first poll that sees it, never seen running,
/// has no start (not a guessed one).
#[test]
fn a_run_never_seen_running_has_no_start() {
    let clock = FakeClock::at(T0);
    let farm = Stub {
        acquire: Ok(Acquired::Pending {
            id: "l1".to_owned(),
        }),
        lease: Ok(Standing::Held {
            host: "h1".to_owned(),
        }),
        run: Ok(RunStanding {
            state: "exited".to_owned(),
            outcome: Some("failed".to_owned()),
        }),
        ..Stub::held()
    };
    let obs = drive(&farm, &clock, &[job("a", 1, None)], 10, 1_000);
    let a = &obs.jobs[0];
    assert_eq!(
        (a.granted_ms, a.started_ms, a.finished_ms),
        (Some(T0 + 10), None, Some(T0 + 20))
    );
    assert_eq!(a.outcome.as_deref(), Some("failed"));
    assert_eq!(*farm.released.borrow(), vec!["l1".to_owned()]);
}

/// Asserts the deadline: a run that never ends, a lease that stays pending, a run still
/// queued and a job that was never due are each given up with `deadline`, the leases
/// released, and the drive ends at the deadline.
#[test]
fn open_jobs_are_given_up_at_the_deadline() {
    let clock = FakeClock::at(T0);
    let forever = Stub::held();
    let mut never_due = job("later", 1, None);
    never_due.submit_at_ms = 10_000_000;
    let obs = drive(
        &forever,
        &clock,
        &[job("a", 1, None), never_due],
        10_000,
        30_000,
    );
    assert_eq!(obs.jobs[0].failure.as_deref(), Some("deadline"));
    assert_eq!(obs.jobs[0].started_ms, Some(T0 + 10_000));
    assert_eq!(obs.jobs[1].failure.as_deref(), Some("deadline"));
    assert_eq!(obs.jobs[1].requested_ms, None);
    assert_eq!(*forever.released.borrow(), vec!["l1".to_owned()]);
    assert_eq!(obs.end_ms, T0 + 30_000);

    let pending = Stub::pending();
    let obs = drive(&pending, &clock, &[job("a", 1, None)], 10_000, 20_000);
    assert_eq!(obs.jobs[0].failure.as_deref(), Some("deadline"));
    assert_eq!(*pending.released.borrow(), vec!["l1".to_owned()]);

    let queued = Stub {
        run: Ok(running("queued")),
        ..Stub::held()
    };
    let obs = drive(&queued, &clock, &[job("a", 1, None)], 10_000, 20_000);
    assert_eq!(obs.jobs[0].failure.as_deref(), Some("deadline"));
    assert_eq!(obs.jobs[0].started_ms, None);
}

/// Asserts that giving up at the deadline leaves a job that is already over as it was:
/// its outcome stands, only the job still running is marked `deadline`.
#[test]
fn a_finished_job_is_not_marked_at_the_deadline() {
    let clock = FakeClock::at(T0);
    let farm = one_host(&clock);
    let jobs = [job("short", 5_000, None), job("long", 10_000_000, None)];
    let obs = drive(&farm, &clock, &jobs, 10_000, 60_000);
    assert_eq!(obs.jobs[0].failure, None);
    assert_eq!(obs.jobs[0].outcome.as_deref(), Some("succeeded"));
    assert_eq!(obs.jobs[1].failure.as_deref(), Some("deadline"));
    assert_eq!(obs.jobs[1].finished_ms, None);
}
