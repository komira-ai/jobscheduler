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
    h.fake.pull_pending(COLD);
    h.start(cold("r1"));
    h.life.step();
    h.life.step();
    assert_eq!(h.fake.asks(COLD), 3);
    h.fake.pull_done(COLD);
    assert_eq!(names(&h.life.step()), ["started r1"]);
    assert_eq!(held(&h), [("r1".to_owned(), HeldState::Running)]);
    assert_eq!(h.fake.created().len(), 1);
    assert_eq!(h.fake.created()[0].run_id, "r1");
    assert!(h.scratch_of("r1").exists());
    assert_eq!(h.fake.asks(COLD), 3);
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
