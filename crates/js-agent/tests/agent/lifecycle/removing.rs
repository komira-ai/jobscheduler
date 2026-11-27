//! A finished job's cleanup off the worker: the container's removal asked of the runtime
//! (`Runtime::removing`, held pending by the [`Deferring`] gate as a crun removal on its pool
//! is) and the removal of the run's directories on the lifecycle's own threads
//! (`Lifecycle::with_cleanup_threads`). The job waits in its stage with its slot booked, every
//! other job goes on, a failure taints as it always did, and the cleanup's timeline says
//! where its time went. Nothing sleeps: a held removal is released by the test, and an
//! answer of the lifecycle's threads is awaited by the wake it rings.

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use js_agent::lifecycle::taint::reason;
use js_agent::lifecycle::{CleanupTimeline, Clock, Event, Lifecycle, Wake, code};

use super::deferring::Deferring;
use super::per_job::{job_start, start};
use super::{Harness, cleanup, hook, job, names, reply};

/// The record of the checks owed by `run_id`.
fn owed(h: &Harness, run_id: &str) -> PathBuf {
    h.dir.path("state").join(format!("owed-{run_id}"))
}

/// The timeline of the cleanup of `run_id` among `events`, which must hold one.
fn cleaned(events: &[Event], run_id: &str) -> CleanupTimeline {
    let found = events.iter().find_map(|event| match event {
        Event::Cleaned {
            run_id: id,
            timeline,
        } if id == run_id => Some(timeline.clone()),
        _ => None,
    });
    found.expect("the cleanup's timeline")
}

/// Asserts that a start received while a finished job's container is being removed is taken
/// and admitted as soon as the step that asked for the removal returns, with nothing of the
/// removal's time in its `taken`: the removal (which takes 500 ms on the clock when the
/// runtime makes it) is pending, the step returns at once, and the start's timeline says it
/// was taken 0 ms after it arrived. The removal is asked again at each step until it is
/// done, and the cleanup then goes on in order. Catches a cleanup that removes the container
/// on the worker, which holds every reply behind it (the `taken` of a start, and a full
/// queue of replies on a busy host).
#[test]
fn a_start_received_while_a_removal_runs_is_taken_and_admitted_at_once() {
    let (mut h, gate) = Deferring::harness("removing-start");
    gate.remove_takes(&h.clock, 500);
    h.start(job("r1"));
    gate.hold_removal("c1");
    h.fake.exit("r1", 0);
    let received = h.clock.now_ms();
    assert_eq!(names(&h.life.step()), ["result r1"]);
    assert_eq!(
        h.clock.now_ms(),
        received,
        "the step did not remove the container"
    );
    assert_eq!(gate.removal_asks("c1"), 1);

    let start = reply(vec![job_start(start("r2"))]);
    let events = h.life.apply_at(&start, received);
    let Some(Event::Started {
        run_id, timeline, ..
    }) = events.first()
    else {
        panic!("r2 did not start: {events:?}");
    };
    assert_eq!(run_id, "r2");
    assert_eq!(timeline.steps.first(), Some(&("taken", 0)));
    assert_eq!(
        h.life.free().cpu_millis,
        2000,
        "r1's slot and r2's are booked"
    );
    assert!(names(&h.life.step()).is_empty(), "the removal still runs");
    assert_eq!(
        gate.removal_asks("c1"),
        2,
        "asked again by the step after the reply"
    );

    gate.release_removal("c1");
    assert_eq!(names(&h.life.step()), cleanup("r1"));
    assert_eq!(gate.removal_asks("c1"), 3);
    assert_eq!(h.life.free().cpu_millis, 3000, "r2 runs on");
}

/// Asserts that the timeline of a cleanup says when each step was done, counted from the
/// step that took the run into its cleanup, and how long the worker spent itself: a removal
/// off the worker that answers 300 ms later is in the steps (`removed`, `scratch` and
/// `checks` at 300) and not in `worker_ms` (0), and a removal on the worker that takes 40 ms
/// is in both. Catches a timeline that counts a removal off the worker as the worker's (the
/// measurement would not show the worker freed), and one that loses a step.
#[test]
fn the_cleanup_timeline_says_when_each_step_was_done_and_what_the_worker_spent() {
    let (mut h, gate) = Deferring::harness("removing-timeline");
    h.start(job("r1"));
    gate.hold_removal("c1");
    h.fake.exit("r1", 0);
    assert_eq!(names(&h.life.step()), ["result r1"]);
    h.clock.advance(300);
    gate.release_removal("c1");
    let events = h.life.step();
    assert_eq!(names(&events), cleanup("r1"));
    assert!(matches!(events.last(), Some(Event::Cleaned { .. })));
    let off_worker = CleanupTimeline {
        steps: vec![("removed", 300), ("scratch", 300), ("checks", 300)],
        worker_ms: 0,
    };
    assert_eq!(cleaned(&events, "r1"), off_worker);

    let (mut h, gate) = Deferring::harness("removing-timeline-worker");
    gate.remove_takes(&h.clock, 40);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = h.life.step();
    let on_worker = CleanupTimeline {
        steps: vec![("removed", 40), ("scratch", 40), ("checks", 40)],
        worker_ms: 40,
    };
    assert_eq!(cleaned(&events, "r1"), on_worker);
}

/// Asserts that a removal that fails off the worker taints exactly as one on the worker did:
/// nothing is tainted while it is pending, and once it answers the failure the host is
/// tainted `cleanup-failed` with the check `container-remove-failed`, the slot stays booked,
/// and the next step removes it and frees the slot. Catches a failure answered late that is
/// taken for a success, or lost.
#[test]
fn a_removal_that_fails_off_the_worker_taints_as_before() {
    let (mut h, gate) = Deferring::harness("removing-fails");
    h.start(job("r1"));
    gate.hold_removal("c1");
    h.fake.exit("r1", 0);
    assert_eq!(names(&h.life.step()), ["result r1"]);
    assert!(!h.life.is_tainted());

    h.fake.fail("remove", 1);
    gate.release_removal("c1");
    assert_eq!(
        names(&h.life.step()),
        [
            "tainted r1 cleanup-failed",
            "finding r1 container-remove-failed"
        ]
    );
    let taint = h.life.pending_taint().expect("tainted");
    assert_eq!(taint.reason, reason::CLEANUP_FAILED);
    assert_eq!(taint.check.as_deref(), Some(code::CONTAINER_REMOVE_FAILED));
    assert_eq!(h.life.free().cpu_millis, 3000, "the slot is kept");
    assert_eq!(names(&h.life.step()), cleanup("r1"));
    assert_eq!(h.life.free().cpu_millis, 4000);
}

/// Asserts that the record of the owed checks is on disk before the removal is asked, so an
/// agent that restarts while the removal runs off the worker finds it and taints the host
/// `check-owed`. Catches a record written after the removal is handed off (a restart in
/// between would forget that the host was never checked).
#[test]
fn a_restart_while_a_removal_runs_finds_the_owed_record() {
    let (mut h, gate) = Deferring::harness("removing-restart");
    h.start(job("r1"));
    gate.hold_removal("c1");
    h.fake.exit("r1", 0);
    assert_eq!(names(&h.life.step()), ["result r1"]);
    assert!(
        owed(&h, "r1").exists(),
        "written before the removal was asked"
    );
    assert!(!h.marker().exists());

    let events = names(&h.restart().expect("restarts"));
    assert_eq!(events, ["tainted r1 check-owed", "adopted r1"]);
    let taint = h.life.pending_taint().expect("tainted");
    assert_eq!(
        (taint.reason.as_str(), taint.run_id.as_deref()),
        (reason::CHECK_OWED, Some("r1"))
    );
}

/// A harness whose lifecycle removes the runs' directories on one thread of its own, and what
/// that thread rings when a removal has answered.
fn pooled(name: &str) -> (Harness, Receiver<()>) {
    let mut h = Harness::new(name);
    let scratch = h.dir.path("scratch");
    let checks = hook(&h.log, &h.fake, &scratch, &[]);
    let (life, _) = Lifecycle::adopt(
        h.config.clone(),
        Box::new(h.fake.clone()),
        checks,
        Box::new(h.clock.clone()),
    )
    .expect("adopts");
    let (ring, rung) = channel();
    let wake = Wake::new(move || {
        let _ = ring.send(());
    });
    h.life = life.with_cleanup_threads(1).with_wake(wake);
    (h, rung)
}

/// Asserts that with threads of its own the lifecycle removes a run's directories off the
/// worker: the step that removed the container ends with the directories asked of the
/// thread, and the step after the thread rang takes the answer and goes on with the checks
/// and the slot; the scratch directory is gone. Catches a scratch removal that still runs on
/// the worker, one whose answer is never taken (the slot held for good), and one that skips
/// the order.
#[test]
fn a_runs_directories_are_removed_off_the_worker_and_taken_after_the_ring() {
    let (mut h, rung) = pooled("removing-dirs");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    assert_eq!(names(&h.life.step()), ["result r1", "container removed r1"]);
    assert_eq!(
        h.life.free().cpu_millis,
        3000,
        "the slot is booked meanwhile"
    );
    rung.recv().expect("the thread rang");
    assert_eq!(
        names(&h.life.step()),
        ["scratch removed r1", "checks passed r1", "freed r1"]
    );
    assert!(!h.scratch_of("r1").exists());
    assert_eq!(h.life.free().cpu_millis, 4000);
}

/// Asserts that a removal of a run's directories that fails off the worker taints exactly as
/// one on the worker did (`cleanup-failed`, check `scratch-failed`, the slot kept), and that
/// the next step asks the thread again, whose success then frees the slot. Catches a failure
/// answered off the worker that is taken for a success, and one never tried again.
#[test]
fn a_directory_removal_that_fails_off_the_worker_taints_as_before() {
    let (mut h, rung) = pooled("removing-dirs-fail");
    h.start(job("r1"));
    let scratch = h.scratch_of("r1");
    fs::remove_dir_all(&scratch).unwrap();
    fs::write(&scratch, "not a directory").unwrap();
    h.fake.exit("r1", 0);
    assert_eq!(names(&h.life.step()), ["result r1", "container removed r1"]);
    rung.recv().expect("the thread rang");
    assert_eq!(
        names(&h.life.step()),
        ["tainted r1 cleanup-failed", "finding r1 scratch-failed"]
    );
    let taint = h.life.pending_taint().expect("tainted");
    assert_eq!(taint.check.as_deref(), Some(code::SCRATCH_FAILED));
    assert_eq!(h.life.free().cpu_millis, 3000, "the slot is kept");

    fs::remove_file(&scratch).unwrap();
    assert!(
        names(&h.life.step()).is_empty(),
        "asked of the thread again"
    );
    rung.recv().expect("the thread rang");
    assert_eq!(
        names(&h.life.step()),
        ["scratch removed r1", "checks passed r1", "freed r1"]
    );
    assert_eq!(h.life.free().cpu_millis, 4000);
}
