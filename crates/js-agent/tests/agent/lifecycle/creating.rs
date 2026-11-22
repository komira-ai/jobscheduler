//! A start whose container the runtime creates off the worker waits as `starting` in the
//! phase `Creating`: its slot is booked, no other start waits for it, and a stop, a cancel
//! or the end of its lease that comes meanwhile is acted on once the create answered. The
//! create is scripted by [`Deferring`](super::deferring::Deferring): pending until the test
//! releases it. The clock is the test's, so nothing here waits or measures time.

use js_agent::lifecycle::{Event, StartTimeline, code, detail};
use js_agent_wire::{Command, HeldRun, HeldState, JobSpec, Outcome};

use super::deferring::{Deferring, lease};
use super::per_job::{job_start, listing, start};
use super::{Harness, job, names};

const COLD: &str = "registry.example/cold:1@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// The run states held, in order.
fn held(h: &Harness) -> Vec<(String, HeldState)> {
    h.life
        .held()
        .into_iter()
        .map(|HeldRun { run_id, state }| (run_id, state))
        .collect()
}

/// A start of `run_id` for the cold image.
fn cold(run_id: &str) -> JobSpec {
    let mut spec = job(run_id);
    spec.image = COLD.to_owned();
    spec
}

/// The events of the next `steps` steps, in order.
fn stepped(h: &mut Harness, steps: usize) -> Vec<String> {
    (0..steps).flat_map(|_| names(&h.life.step())).collect()
}

/// Asserts that a job start whose create is under way is listed `starting` at once with its
/// CPU, memory and slot booked, no `Started` event and no container; that a second job
/// applied meanwhile is created, started and listed `running` beside it (the slow create
/// holds only its own job); that the runtime is asked about the pending create at every
/// step; and that once released the next step lists the job `running`, logs `Started` for it
/// with `created` as the last step of its timeline, and asks nothing more. Catches a create
/// that blocks the worker or the starts behind it, a start reported before its container
/// exists, and a booking not held while the create is under way.
#[test]
fn a_job_whose_create_is_under_way_is_starting_and_holds_only_itself() {
    let (mut h, gate) = Deferring::harness("creating-job");
    gate.hold("j1");
    assert!(h.apply(vec![job_start(start("j1"))]).is_empty());
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert_eq!(h.life.free().cpu_millis, 3000);
    assert_eq!(h.life.busy_slots(), [0]);
    assert!(h.fake.created().is_empty());
    assert_eq!(gate.begun(), ["j1"]);
    assert_eq!(gate.polls("j1"), 0);

    let events = h.apply(vec![job_start(start("j2"))]);
    assert_eq!(names(&events), ["started j2"]);
    assert_eq!(listing(&h, "j2").state, HeldState::Running);
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert_eq!(h.life.busy_slots(), [0, 1]);
    assert_eq!(h.fake.created().len(), 1);

    for asked in 1..=3 {
        assert!(h.life.step().is_empty());
        assert_eq!(gate.polls("j1"), asked);
    }
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert!(h.fake.created().iter().all(|run| run.run_id == "j2"));

    gate.release("j1");
    let events = h.life.step();
    let [
        Event::Started {
            run_id, timeline, ..
        },
    ] = events.as_slice()
    else {
        panic!("one start: {events:?}");
    };
    assert_eq!(run_id, "j1");
    let StartTimeline { steps, .. } = timeline;
    assert_eq!(steps.last().map(|step| step.0), Some("created"));
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
    assert_eq!(h.fake.created().len(), 2);
    assert!(h.life.step().is_empty());
    assert_eq!(gate.polls("j1"), 4, "no ask once it answered");
    assert_eq!(gate.begun(), ["j1", "j2"]);
}

/// Asserts that a create that answers a failure ends the start `failed_to_start` with the
/// runtime's text as its finding and the `runtime-failed` detail, makes no container, and
/// gives the booking and the slot back after the cleanup. Catches a start left `starting`
/// for ever by a create that failed, and a slot held by a start that never began.
#[test]
fn a_create_that_fails_ends_the_start_failed_and_frees_its_slot() {
    let (mut h, gate) = Deferring::harness("creating-fails");
    gate.hold("r1");
    assert!(h.start(job("r1")).is_empty());
    h.fake.fail("create", 1);
    gate.release("r1");
    let events = h.life.step();
    let [
        Event::Finding(finding),
        Event::ResultReady { run_id, .. },
        ..,
    ] = events.as_slice()
    else {
        panic!("a finding and a result: {events:?}");
    };
    assert_eq!(finding.code, code::START_FAILED);
    assert_eq!(finding.message, "create failed");
    assert_eq!(run_id, "r1");
    let results = h.life.results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, Outcome::FailedToStart);
    assert_eq!(results[0].detail.as_deref(), Some(detail::RUNTIME_FAILED));
    assert!(h.fake.created().is_empty());
    for _ in 0..3 {
        h.life.step();
    }
    assert!(h.life.busy_slots().is_empty());
    assert_eq!(h.life.free().cpu_millis, 4000);
}

/// Asserts that a run's `Stop` asked while its create is under way does not end the start:
/// it stays `starting` with no result and no container for as many steps as the create is
/// pending, and once the create answers the container is stopped (one stop call) and the
/// result is `killed`, then the cleanup frees the slot. Catches a start ended before its
/// create answered (a container left running beyond the agent's sight) and a stop lost
/// because it was asked before the container existed.
#[test]
fn a_stop_asked_while_the_create_is_under_way_stops_what_it_made() {
    let (mut h, gate) = Deferring::harness("creating-stop");
    gate.hold("r1");
    h.start(job("r1"));
    h.stop("r1");
    for _ in 0..3 {
        assert!(h.life.step().is_empty());
    }
    assert_eq!(held(&h), [("r1".to_owned(), HeldState::Starting)]);
    assert!(h.life.results().is_empty());
    assert!(h.fake.stops().is_empty());
    assert_eq!(gate.polls("r1"), 3);

    gate.release("r1");
    let events = stepped(&mut h, 4);
    assert_eq!(events[0], "started r1");
    assert!(events.contains(&"result r1".to_owned()), "{events:?}");
    assert_eq!(h.fake.stops().len(), 1);
    assert_eq!(h.life.results().len(), 1);
    assert_eq!(h.life.results()[0].outcome, Outcome::Killed);
    stepped(&mut h, 3);
    assert!(h.life.busy_slots().is_empty());
}

/// Asserts that a `JobStop` asked while the job's create is under way leaves the job
/// `starting` until the create answers, then stops the container once and gives the job a
/// `killed` result. Catches the stop of a job start dropped or applied to a container that
/// did not exist yet.
#[test]
fn a_job_stop_asked_while_the_create_is_under_way_kills_the_job_once_created() {
    let (mut h, gate) = Deferring::harness("creating-job-stop");
    gate.hold("j1");
    h.apply(vec![job_start(start("j1"))]);
    h.apply(vec![Command::JobStop {
        job_id: "j1".to_owned(),
    }]);
    assert!(h.life.step().is_empty());
    let job = listing(&h, "j1");
    assert_eq!(job.state, HeldState::Starting);
    assert!(job.result.is_none());
    assert!(h.fake.stops().is_empty());

    gate.release("j1");
    stepped(&mut h, 3);
    assert_eq!(h.fake.stops().len(), 1);
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::Killed);
}

/// Asserts that a `Cancel` asked while the create is under way is carried over silently:
/// once the create answered the container is stopped and no result is reported for the run.
/// Catches a cancelled start whose container, made after the cancel, runs on, and a cancel
/// that turns into a reported result.
#[test]
fn a_cancel_asked_while_the_create_is_under_way_stops_what_it_made_without_a_result() {
    let (mut h, gate) = Deferring::harness("creating-cancel");
    gate.hold("r1");
    h.start(job("r1"));
    h.cancel("r1");
    assert!(h.life.step().is_empty());
    assert!(h.fake.stops().is_empty());
    gate.release("r1");
    stepped(&mut h, 4);
    assert_eq!(h.fake.stops().len(), 1);
    assert!(h.life.results().is_empty());
}

/// Asserts that a start that first waits for its image (the pull pending, then done) and
/// then finds its create pending is held `starting` with nothing created until the create
/// answers, is not asked about the image again, and runs once the create is released. Catches
/// a start that is asked to begin twice, one that loses its booking between the image and the
/// create, and one started before its create answered.
#[test]
fn a_start_that_waited_for_its_image_then_waits_for_its_create() {
    let (mut h, gate) = Deferring::harness("creating-after-image");
    h.fake.pull_pending(COLD);
    gate.hold("r1");
    h.start(cold("r1"));
    assert!(h.life.step().is_empty());
    assert!(gate.begun().is_empty(), "no create while the image waits");
    h.fake.pull_done(COLD);
    let asked = h.fake.asks(COLD).len();
    assert!(h.life.step().is_empty());
    assert_eq!(gate.begun(), ["r1"]);
    assert_eq!(gate.polls("r1"), 0);
    assert_eq!(held(&h), [("r1".to_owned(), HeldState::Starting)]);
    assert_eq!(h.life.free().cpu_millis, 3000);
    assert!(h.fake.created().is_empty());
    assert!(h.life.step().is_empty());
    assert_eq!(gate.polls("r1"), 1);
    assert_eq!(
        h.fake.asks(COLD).len(),
        asked,
        "not asked again once the pull is done"
    );

    gate.release("r1");
    assert_eq!(names(&h.life.step()), ["started r1"]);
    assert_eq!(held(&h), [("r1".to_owned(), HeldState::Running)]);
    assert_eq!(h.fake.created().len(), 1);
    assert_eq!(
        h.fake.asks(COLD).len(),
        asked,
        "never asked after the pull is done"
    );
}

/// Asserts that when the end of a run start's lease (`valid_for_ms`) passes while its create
/// is under way, the start is not ended before the create answers; once made, the container
/// runs under a deadline that is already past, so the step after is stopped and the result is
/// `timed_out`. Catches a lease end that drops a start whose container is then made and
/// never stopped, and a container that outlives the lease it was started under.
#[test]
fn the_end_of_a_lease_while_the_create_is_under_way_times_out_what_it_made() {
    let (mut h, gate) = Deferring::harness("creating-lease");
    gate.hold("r1");
    let mut spec = job("r1");
    spec.valid_for_ms = 5000;
    h.start(spec);
    h.clock.advance(6000);
    for _ in 0..2 {
        assert!(h.life.step().is_empty());
    }
    assert!(h.life.results().is_empty());
    assert!(h.fake.created().is_empty());
    assert_eq!(held(&h), [("r1".to_owned(), HeldState::Starting)]);

    gate.release("r1");
    let events = stepped(&mut h, 4);
    assert_eq!(events[0], "started r1");
    assert!(events.contains(&"result r1".to_owned()), "{events:?}");
    assert_eq!(h.fake.stops().len(), 1);
    assert_eq!(h.life.results()[0].outcome, Outcome::TimedOut);
}

/// Asserts that the checks of a run that ends are told, as live beside it, the lease of a
/// run whose create is under way (its container may already run) and nothing of a start
/// that still waits for its image (no create is begun, so nothing of it runs). Catches a
/// lifecycle that tells the checks only of the containers it holds as running or exited,
/// which makes a starting neighbour's processes the ended run's leftover, and one that names
/// a start with no create under way.
#[test]
fn the_checks_of_a_run_that_ends_are_told_of_a_create_under_way_beside_it() {
    let (mut h, gate) = Deferring::harness("creating-live");
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
    gate.hold("r2");
    assert!(h.start(job("r2")).is_empty());
    h.fake.pull_pending(COLD);
    assert!(h.start(cold("r3")).is_empty());
    assert_eq!(gate.begun(), ["r1", "r2"], "r3 waits for its image");
    h.fake.exit("r1", 0);
    h.log.borrow_mut().clear();
    h.life.step();
    let log = h.log.borrow().clone();
    let live: Vec<&String> = log
        .iter()
        .filter(|line| line.starts_with("live beside"))
        .collect();
    assert_eq!(live, [&format!("live beside r1: {}", lease("r2"))]);
}
