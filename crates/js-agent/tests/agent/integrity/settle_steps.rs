//! The settle wait of the post-job checks as the lifecycle takes it: the real [`Integrity`],
//! given a wait, over the in-memory host, on the lifecycle's manual clock. A job whose first
//! look sees a process of the job users' range waits in its cleanup with its slot booked
//! while the lifecycle goes on, and the first step once the wait has passed on the clock
//! takes the second look. Nothing sleeps: the clock is moved by hand.

use std::time::Duration;

use js_agent::integrity::{FakeHost, Integrity};
use js_agent::lifecycle::{CleanupTimeline, Event};

use super::{JOB_UID, process, started};
use crate::lifecycle::{Harness, Options, job, names};

/// The wait of these tests, in milliseconds.
const WAIT_MS: u64 = 300;

/// The control group of a job's container, where a leftover of its user runs.
const LEFTOVER: &str = "0::/machine.slice/libpod-9e1.scope";

/// The harness with `host` checked after every job, with the wait of these tests.
fn checked(name: &str, host: &FakeHost) -> Harness {
    let checks: Integrity = started(host, vec![]).with_settle(Duration::from_millis(WAIT_MS));
    Harness::with(
        name,
        Options {
            hook: Some(Box::new(checks)),
            ..Options::default()
        },
    )
}

/// Asserts that a job whose first look sees a process of the job user waits in its cleanup
/// and does not hold the lifecycle: the step after its end stops at the scratch directory
/// with the slot booked and the host untainted, a start taken next books its slot at once,
/// a step a millisecond before the wait has passed looks no further, and the step at the
/// wait's end, the process gone, passes the checks and frees the slot. The view is never
/// asked to wait, and the cleanup's timeline puts the wait in its `checks` step, not in
/// the worker's milliseconds. It catches the checks' wait taken on the worker's thread (the
/// step would go through to a verdict, which here is a taint for a process that was
/// ending), a second look taken before the wait has passed, and a job that never looks again.
#[test]
fn a_job_whose_checks_wait_waits_in_its_cleanup_while_a_start_goes_on() {
    let host = FakeHost::quiet();
    let mut h = checked("settle-steps", &host);
    h.start(job("r1"));
    host.start_process(process(300, 100, JOB_UID, 900, LEFTOVER));
    h.fake.exit("r1", 0);
    assert_eq!(
        names(&h.life.step()),
        ["result r1", "container removed r1", "scratch removed r1"]
    );
    assert_eq!(h.life.free().cpu_millis, 3000, "the slot stays booked");
    assert!(!h.life.is_tainted());

    h.start(job("r2"));
    assert_eq!(h.life.free().cpu_millis, 2000, "the start took its slot");
    host.end_process(300);
    h.clock.advance(WAIT_MS - 1);
    assert!(names(&h.life.step()).is_empty(), "the wait has not passed");
    assert_eq!(h.life.free().cpu_millis, 2000);

    h.clock.advance(1);
    let events = h.life.step();
    assert_eq!(names(&events), ["checks passed r1", "freed r1"]);
    assert_eq!(h.life.free().cpu_millis, 3000, "r1's slot is back");
    assert!(!h.life.is_tainted());
    assert!(host.settled().is_empty(), "nothing waited on the thread");
    let timeline = events.iter().find_map(|event| match event {
        Event::Cleaned { timeline, .. } => Some(timeline.clone()),
        _ => None,
    });
    assert_eq!(
        timeline,
        Some(CleanupTimeline {
            steps: vec![("removed", 0), ("scratch", 0), ("checks", WAIT_MS)],
            worker_ms: 0,
        })
    );
}

/// Asserts that a process still there at the second look taints the host as it did when
/// the wait held the worker: the step at the wait's end taints `residue` and keeps the slot.
/// It catches a wait that lets a real leftover go.
#[test]
fn a_leftover_still_there_after_the_wait_taints_the_host() {
    let host = FakeHost::quiet();
    let mut h = checked("settle-steps-left", &host);
    h.start(job("r1"));
    host.start_process(process(300, 100, JOB_UID, 900, LEFTOVER));
    h.fake.exit("r1", 0);
    h.life.step();
    h.clock.advance(WAIT_MS);
    assert_eq!(
        names(&h.life.step()),
        ["tainted r1 residue", "finding r1 process-as-job-user"]
    );
    assert_eq!(h.life.free().cpu_millis, 3000, "the slot is kept");
    assert!(h.life.is_tainted());
}

/// Asserts that the checks of one job wait at a time: a job that ends while another job's
/// checks wait is taken through its removals and then waits for that check, with its slot
/// booked, and is checked in the same step as the first once the wait has passed, against
/// the baseline the first check left. It catches two checks whose looks interleave, each
/// judging the other's half-settled host.
#[test]
fn a_second_job_is_checked_after_the_check_that_waits() {
    let host = FakeHost::quiet();
    let mut h = checked("settle-steps-two", &host);
    h.start(job("r1"));
    h.start(job("r2"));
    host.start_process(process(300, 100, JOB_UID, 900, LEFTOVER));
    h.fake.exit("r1", 0);
    h.life.step();
    h.fake.exit("r2", 0);
    assert_eq!(
        names(&h.life.step()),
        ["result r2", "container removed r2", "scratch removed r2"]
    );
    assert_eq!(h.life.free().cpu_millis, 2000, "both slots stay booked");
    host.end_process(300);
    h.clock.advance(WAIT_MS);
    assert_eq!(
        names(&h.life.step()),
        ["checks passed r1", "freed r1", "checks passed r2", "freed r2"]
    );
    assert_eq!(h.life.free().cpu_millis, 4000);
    assert!(!h.life.is_tainted());
}
