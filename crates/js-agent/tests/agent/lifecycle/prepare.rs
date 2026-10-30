//! A start whose image is still being pulled: it waits as `starting` with its booking,
//! costs the worker nothing, and is created when the image is here. The pull itself is
//! scripted in the fake runtime (`pull_pending`, `pull_done`, `pull_fails`) and the clock
//! is the test's, so nothing here waits or measures time.

use js_agent::lifecycle::code;
use js_agent_wire::{Command, HeldRun, HeldState, JobSpec, Outcome};

use super::{Harness, cleanup, job, names};

const COLD: &str = "registry.example/cold:1@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// A start of `run_id` for the cold image.
fn cold(run_id: &str) -> JobSpec {
    let mut spec = job(run_id);
    spec.image = COLD.to_owned();
    spec
}

fn held(h: &Harness) -> Vec<(String, HeldState)> {
    h.life
        .held()
        .into_iter()
        .map(|HeldRun { run_id, state }| (run_id, state))
        .collect()
}

/// A run with no image (a host or native job) is never asked about an image: even with a
/// runtime scripted to answer `Pending` for the empty name, it starts at once and the
/// runtime is not consulted. It catches a start that waits on the image store for a job
/// that does not use it.
#[test]
fn a_run_with_no_image_starts_at_once_and_prepare_is_not_asked() {
    let mut h = Harness::new("prepare-no-image");
    h.fake.pull_pending("");
    let mut spec = job("r1");
    spec.image = String::new();
    assert_eq!(names(&h.start(spec)), ["started r1"]);
    assert_eq!(held(&h), [("r1".to_owned(), HeldState::Running)]);
    assert!(h.fake.asks("").is_empty());
}

/// A run whose image is being pulled is held as `starting`, books its CPU, memory and
/// slot, and makes nothing: no event, no scratch directory, no container, however many
/// steps pass. It catches a start that waits for the pull inside the create (the worker
/// stuck), and one that forgets the booking so the host takes more than it has.
#[test]
fn a_run_whose_image_is_being_pulled_waits_as_starting_and_creates_nothing() {
    let mut h = Harness::new("prepare-waits");
    h.fake.pull_pending(COLD);
    assert!(h.start(cold("r1")).is_empty());
    assert_eq!(held(&h), [("r1".to_owned(), HeldState::Starting)]);
    assert_eq!(h.life.free().cpu_millis, 3000);
    assert_eq!(h.life.busy_slots(), [0]);
    for _ in 0..3 {
        assert!(h.life.step().is_empty());
    }
    assert!(h.fake.created().is_empty());
    assert!(!h.scratch_of("r1").exists());
    assert!(h.life.results().is_empty());
}

/// With the image here the container is created at the next step and the run is held as
/// running; the image is asked for at the start and at each step while it is pending, and
/// not otherwise.
#[test]
fn the_container_is_created_at_the_step_after_the_image_arrives() {
    let mut h = Harness::new("prepare-ready");
    let ticket = h.fake.pull_pending(COLD);
    h.start(cold("r1"));
    h.life.step();
    h.life.step();
    // The ask of the start names no ticket; the steps name the one the runtime gave.
    assert_eq!(h.fake.asks(COLD), [None, Some(ticket), Some(ticket)]);
    h.fake.pull_done(COLD);
    assert_eq!(names(&h.life.step()), ["started r1"]);
    assert_eq!(held(&h), [("r1".to_owned(), HeldState::Running)]);
    assert_eq!(h.fake.created().len(), 1);
    assert_eq!(h.fake.created()[0].run_id, "r1");
    assert!(h.scratch_of("r1").exists());
    assert_eq!(h.fake.asks(COLD).len(), 3);
}

/// The wall-time limit counts from the moment the container is created, not from the
/// start or the pull: after a long wait the run still gets its whole limit, to the
/// millisecond. It catches a limit measured from the start, which would kill a run the
/// moment its slow pull finished.
#[test]
fn the_wall_clock_begins_when_the_container_is_created_not_while_the_image_is_pulled() {
    let mut h = Harness::new("prepare-wall");
    h.fake.pull_pending(COLD);
    let mut spec = cold("r1");
    spec.limits.wall_ms = 10_000;
    h.start(spec);
    h.clock.advance(500_000);
    assert!(h.life.step().is_empty());
    assert!(h.fake.stops().is_empty());
    h.fake.pull_done(COLD);
    assert_eq!(names(&h.life.step()), ["started r1"]);
    h.clock.advance(9_999);
    assert!(h.life.step().is_empty());
    assert!(h.fake.stops().is_empty());
    h.clock.advance(1);
    assert_eq!(names(&h.life.step())[0], "result r1");
    assert_eq!(h.life.results()[0].outcome, Outcome::TimedOut);
}

/// A slow pull does not hold up another run: in one reply, the run whose image is here
/// starts and the cold one waits; the first then runs and ends, with its cleanup, while the
/// second still waits. It catches a worker that is busy with one lease's pull while the
/// others' starts, polls and stops queue behind it.
#[test]
fn a_slow_pull_does_not_hold_up_another_run() {
    let mut h = Harness::new("prepare-others");
    h.fake.pull_pending(COLD);
    let events = h.apply(vec![
        Command::Start(Box::new(cold("slow"))),
        Command::Start(Box::new(job("quick"))),
    ]);
    assert_eq!(names(&events), ["started quick"]);
    assert_eq!(
        held(&h),
        [
            ("slow".to_owned(), HeldState::Starting),
            ("quick".to_owned(), HeldState::Running)
        ]
    );
    h.fake.exit("quick", 0);
    let mut expected = vec!["result quick".to_owned()];
    expected.extend(cleanup("quick"));
    assert_eq!(names(&h.life.step()), expected);
    // The finished run stays held until its result is acknowledged; the cold one waits on.
    assert_eq!(
        held(&h),
        [
            ("slow".to_owned(), HeldState::Starting),
            ("quick".to_owned(), HeldState::ExitedUnacked)
        ]
    );
    assert_eq!(h.fake.created().len(), 1);
}

/// An image that could not be had ends the run `failed_to_start` with the runtime's
/// failure as its finding, whether the failure is found when the start is admitted or at
/// a later step, and creates nothing; the cleanup gives the slot back.
#[test]
fn an_image_that_cannot_be_had_ends_the_run_failed_to_start() {
    let mut h = Harness::new("prepare-fails");
    h.fake.pull_fails(COLD, "registry down");
    let events = h.start(cold("early"));
    assert_eq!(
        names(&events),
        [
            format!("finding early {}", code::START_FAILED),
            "result early".to_owned()
        ]
    );
    h.life.step();
    h.fake.pull_pending(COLD);
    h.start(cold("late"));
    h.fake.pull_fails(COLD, "registry down");
    let events = h.life.step();
    assert_eq!(
        names(&events)[..2],
        [
            format!("finding late {}", code::START_FAILED),
            "result late".to_owned()
        ]
    );
    for run in ["early", "late"] {
        let result = h
            .life
            .results()
            .into_iter()
            .find(|result| result.run_id == run)
            .expect("a result");
        assert_eq!(result.outcome, Outcome::FailedToStart, "{run}");
    }
    assert!(h.fake.created().is_empty());
    h.life.step();
    assert!(h.life.busy_slots().is_empty());
}

/// A `Stop` while the image is pulled ends the run `killed` without ever making a
/// container, and a `Cancel` ends it reporting nothing; the slot comes back, and the
/// image arriving later creates nothing.
#[test]
fn a_stop_or_cancel_while_the_image_is_pulled_ends_the_run_without_a_container() {
    let mut h = Harness::new("prepare-stop");
    h.fake.pull_pending(COLD);
    h.start(cold("stopped"));
    h.start(cold("cancelled"));
    h.stop("stopped");
    h.cancel("cancelled");
    let events = names(&h.life.step());
    assert!(events.contains(&"result stopped".to_owned()), "{events:?}");
    assert!(
        !events.contains(&"result cancelled".to_owned()),
        "{events:?}"
    );
    assert_eq!(h.life.results().len(), 1);
    assert_eq!(h.life.results()[0].outcome, Outcome::Killed);
    h.life.step();
    h.fake.pull_done(COLD);
    h.life.step();
    assert!(h.fake.created().is_empty());
    assert!(h.life.busy_slots().is_empty());
    assert_eq!(h.life.free().cpu_millis, 4000);
}

/// A run keeps the ticket of the fetch it waits on, and takes the new one when the runtime
/// answers with another (the image was forgotten and is fetched again): each ask names the
/// ticket of the last answer.
#[test]
fn a_waiting_run_names_the_fetch_it_waits_on_and_follows_a_new_one() {
    let mut h = Harness::new("prepare-ticket");
    let first = h.fake.pull_pending(COLD);
    h.start(cold("r1"));
    h.life.step();
    let second = h.fake.pull_pending(COLD);
    h.life.step();
    h.life.step();
    assert_eq!(
        h.fake.asks(COLD),
        [None, Some(first), Some(first), Some(second)]
    );
}

/// Every run that waited on a fetch that failed ends `failed_to_start`, not only the first
/// to ask: two runs wait, the fetch fails, and both end at the next step.
#[test]
fn every_run_that_waited_on_a_failed_fetch_ends() {
    let mut h = Harness::new("prepare-all-fail");
    h.fake.pull_pending(COLD);
    h.start(cold("r1"));
    h.start(cold("r2"));
    h.fake.pull_fails(COLD, "registry down");
    h.life.step();
    let mut ended: Vec<_> = h
        .life
        .results()
        .into_iter()
        .map(|result| (result.run_id, result.outcome))
        .collect();
    ended.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        ended,
        [
            ("r1".to_owned(), Outcome::FailedToStart),
            ("r2".to_owned(), Outcome::FailedToStart)
        ]
    );
}

/// A run that waited for its image is held to the end of its lease: a start that
/// is valid for 30 s and asks for a minute, whose image arrives after 20 s, gets 10 s, not
/// a fresh minute. (A start that names no lease end gets its whole limit: that half is in
/// the wall-clock test above.)
#[test]
fn a_run_that_waited_never_outlives_its_lease() {
    let mut h = Harness::new("prepare-lease");
    h.fake.pull_pending(COLD);
    let mut spec = cold("r1");
    spec.limits.wall_ms = 60_000;
    spec.valid_for_ms = 30_000;
    h.start(spec);
    h.clock.advance(20_000);
    h.fake.pull_done(COLD);
    assert_eq!(names(&h.life.step()), ["started r1"]);
    h.clock.advance(9_999);
    assert!(h.life.step().is_empty());
    assert!(h.fake.stops().is_empty());
    h.clock.advance(1);
    assert_eq!(names(&h.life.step())[0], "result r1");
    assert_eq!(h.life.results()[0].outcome, Outcome::TimedOut);
}

/// A stop that lands in the same step as the image creates nothing: the stop is looked at
/// before the image is asked for.
#[test]
fn a_stop_and_the_image_in_the_same_step_create_nothing() {
    let mut h = Harness::new("prepare-race");
    h.fake.pull_pending(COLD);
    h.start(cold("r1"));
    h.stop("r1");
    h.fake.pull_done(COLD);
    assert_eq!(names(&h.life.step())[0], "result r1");
    assert!(h.fake.created().is_empty());
    assert_eq!(h.life.results()[0].outcome, Outcome::Killed);
}

/// A run whose lease ends while it waits for its image is not created, however late the
/// image comes: it ends `killed` at the first step past the lease end, with no scratch
/// directory and no container, and the image arriving then creates nothing.
#[test]
fn a_run_whose_lease_ended_while_it_waited_is_not_created() {
    let mut h = Harness::new("prepare-lease-gone");
    h.fake.pull_pending(COLD);
    let mut spec = cold("r1");
    spec.valid_for_ms = 30_000;
    h.start(spec);
    h.clock.advance(29_999);
    assert!(h.life.step().is_empty());
    h.clock.advance(1);
    h.fake.pull_done(COLD);
    assert_eq!(names(&h.life.step())[0], "result r1");
    assert_eq!(h.life.results()[0].outcome, Outcome::Killed);
    h.life.step();
    assert!(h.fake.created().is_empty());
    assert!(!h.scratch_of("r1").exists());
    assert!(h.life.busy_slots().is_empty());
}
