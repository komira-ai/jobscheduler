//! The timeline a start is logged with (`Event::Started`): the steps of the lifecycle, each in
//! milliseconds after the reply arrived on the test's clock, and the runtime's own steps as
//! the runtime gives them. The clock moves only when a test moves it (and while the fake's
//! create takes its scripted time), so every number here is the test's.

use js_agent::lifecycle::{Clock, Event, StartTimeline};
use js_agent::{Machine, System};
use js_agent_wire::Command;

use super::fake::{NoUnixClock, UNIX_ORIGIN_MS};
use super::per_job::{job_start, start};
use super::rootfs::{harness, layered};
use super::{Harness, job, names, reply};

const COLD: &str = "registry.example/cold:1@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// The one `Started` event of `run_id` with `timeline`.
fn started(run_id: &str, timeline: StartTimeline) -> Vec<Event> {
    vec![Event::Started {
        run_id: run_id.to_owned(),
        timeline,
    }]
}

/// Asserts the timeline of a start that waits for nothing: it arrived at 4, the worker took
/// it at 10, it was admitted, ready and given its directories then, and its create (which
/// takes 7) returned at 17; the runtime's own steps come as the runtime gave them. Catches a
/// step measured from the wrong origin (the clock's, not the arrival), a create not counted,
/// and the runtime's steps lost.
#[test]
fn a_start_is_logged_with_each_step_from_its_arrival_to_its_create() {
    let mut h = Harness::new("timeline-plain");
    h.clock.advance(10);
    h.fake.create_takes(7);
    h.fake
        .steps_of("r1", &[("begin", 1_000), ("bundle", 1_003)]);
    let start = reply(vec![Command::Start(Box::new(job("r1")))]);
    let events = h.life.apply_at(&start, 4);
    let timeline = StartTimeline {
        received_ms: 4,
        placed_at_ms: None,
        received_at_ms: Some(UNIX_ORIGIN_MS + 4),
        steps: vec![
            ("taken", 6),
            ("admitted", 6),
            ("ready", 6),
            ("dirs", 6),
            ("created", 13),
        ],
        runtime: vec![("begin", 1_000), ("bundle", 1_003)],
    };
    assert_eq!(events, started("r1", timeline));
}

/// Asserts that a start whose image is being pulled shows the wait: admitted at once, ready
/// only at the step after the image arrived, 250 later. Catches a wait for the image that
/// the timeline hides (`ready` marked at the admission).
#[test]
fn a_start_that_waits_for_its_image_shows_the_wait() {
    let mut h = Harness::new("timeline-image");
    h.fake.pull_pending(COLD);
    let mut spec = job("r1");
    spec.image = COLD.to_owned();
    assert!(h.start(spec).is_empty());
    h.clock.advance(100);
    assert!(h.life.step().is_empty());
    h.fake.pull_done(COLD);
    h.clock.advance(150);
    let timeline = StartTimeline {
        received_ms: 0,
        placed_at_ms: None,
        received_at_ms: Some(UNIX_ORIGIN_MS),
        steps: vec![
            ("taken", 0),
            ("admitted", 0),
            ("ready", 250),
            ("dirs", 250),
            ("created", 250),
        ],
        runtime: Vec::new(),
    };
    assert_eq!(h.life.step(), started("r1", timeline));
}

/// Asserts the timeline of a start with layers and mounts and no provider: its directories
/// are made at its admission, its host paths found at the step 20 after it arrived, its
/// assembly answered at the next (50), and its container created then; it has no `provided`
/// step. Catches the steps of a start that waits for its file system dropped or put out of
/// order, and a `provided` mark on a start with no provider.
#[test]
fn a_start_with_layers_shows_its_lookups_and_its_assembly() {
    let mut h = harness("timeline-layers");
    let spec = layered(&h, "j1");
    assert!(h.apply(vec![job_start(spec)]).is_empty());
    h.clock.advance(20);
    assert!(h.life.step().is_empty());
    h.clock.advance(30);
    let timeline = StartTimeline {
        received_ms: 0,
        placed_at_ms: None,
        received_at_ms: Some(UNIX_ORIGIN_MS),
        steps: vec![
            ("taken", 0),
            ("admitted", 0),
            ("dirs", 0),
            ("resolved", 20),
            ("ready", 50),
            ("created", 50),
        ],
        runtime: Vec::new(),
    };
    assert_eq!(h.life.step(), started("j1", timeline));
}

/// Asserts that a start whose create failed is logged with no `Started` line (so with no
/// timeline), and that the start after it is logged from its own arrival with its own steps.
/// Catches a failed create logged as a start.
#[test]
fn a_failed_start_leaves_no_timeline_behind() {
    let mut h = Harness::new("timeline-failed");
    h.fake.fail("create", 1);
    let events = names(&h.start(job("r1")));
    assert!(events.contains(&"result r1".to_owned()), "{events:?}");
    assert!(!events.iter().any(|event| event.starts_with("started")));
    h.clock.advance(5);
    let timeline = StartTimeline {
        received_ms: 5,
        placed_at_ms: None,
        received_at_ms: Some(UNIX_ORIGIN_MS + 5),
        steps: vec![
            ("taken", 0),
            ("admitted", 0),
            ("ready", 0),
            ("dirs", 0),
            ("created", 0),
        ],
        runtime: Vec::new(),
    };
    let start = reply(vec![Command::Start(Box::new(job("r2")))]);
    assert_eq!(h.life.apply_at(&start, 5), started("r2", timeline));
}

/// Asserts that a job start carries its placement time into the timeline as sent, and that the
/// timeline gives the reply's arrival in Unix time: the start arrived at 4 on the clock, was
/// taken at 10, and the clock's Unix time at 10 is the origin plus 10, so the arrival is the
/// origin plus 4. Catches a placement time dropped or changed on its way to the line, and an
/// arrival read as the Unix time at the admission instead of at the arrival.
#[test]
fn a_job_start_is_logged_with_its_placement_time_and_its_arrival_in_unix_time() {
    let mut h = Harness::new("timeline-placed");
    h.clock.advance(10);
    let mut spec = start("j1");
    spec.placed_at_ms = Some(UNIX_ORIGIN_MS - 250);
    let events = h.life.apply_at(&reply(vec![job_start(spec)]), 4);
    let timeline = StartTimeline {
        received_ms: 4,
        placed_at_ms: Some(UNIX_ORIGIN_MS - 250),
        received_at_ms: Some(UNIX_ORIGIN_MS + 4),
        steps: vec![
            ("taken", 6),
            ("admitted", 6),
            ("ready", 6),
            ("dirs", 6),
            ("created", 6),
        ],
        runtime: Vec::new(),
    };
    assert_eq!(events, started("j1", timeline));
}

/// Asserts that a job start from a server that sent no placement time has none in its
/// timeline, while its arrival in Unix time is still given. Catches a placement time made up
/// when the start has none.
#[test]
fn a_job_start_without_a_placement_time_has_none_in_its_timeline() {
    let mut h = Harness::new("timeline-unplaced");
    h.clock.advance(3);
    let events = h.life.apply_at(&reply(vec![job_start(start("j1"))]), 3);
    let timeline = StartTimeline {
        received_ms: 3,
        placed_at_ms: None,
        received_at_ms: Some(UNIX_ORIGIN_MS + 3),
        steps: vec![
            ("taken", 0),
            ("admitted", 0),
            ("ready", 0),
            ("dirs", 0),
            ("created", 0),
        ],
        runtime: Vec::new(),
    };
    assert_eq!(events, started("j1", timeline));
}

/// Asserts that on a clock with no Unix time the timeline has no arrival in Unix time,
/// while the placement time as sent and the steps are kept. Catches an arrival made up from
/// a clock that cannot give one.
#[test]
fn a_clock_without_unix_time_leaves_the_arrival_out_of_the_timeline() {
    let mut h = Harness::new("timeline-no-unix");
    h.restart_with_clock(Box::new(NoUnixClock(h.clock.clone())))
        .unwrap();
    let mut spec = start("j1");
    spec.placed_at_ms = Some(1_800_000_000_123);
    let events = h.life.apply_at(&reply(vec![job_start(spec)]), 0);
    let timeline = StartTimeline {
        received_ms: 0,
        placed_at_ms: Some(1_800_000_000_123),
        received_at_ms: None,
        steps: vec![
            ("taken", 0),
            ("admitted", 0),
            ("ready", 0),
            ("dirs", 0),
            ("created", 0),
        ],
        runtime: Vec::new(),
    };
    assert_eq!(events, started("j1", timeline));
}

/// Asserts that the agent's own clock has a Unix time, and that it is after a fixed past
/// instant (1 September 2026). Catches a real clock left on the default of none, or one
/// read in seconds.
#[test]
fn the_agents_clock_has_a_unix_time() {
    let clock = System::new().clock();
    assert!(clock.unix_ms() >= Some(1_788_220_800_000));
}
