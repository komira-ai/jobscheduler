//! The timeline a start is logged with (`Event::Started`): the steps of the lifecycle, each in
//! milliseconds after the reply arrived on the test's clock, and the runtime's own steps as
//! the runtime gives them. The clock moves only when a test moves it (and while the fake's
//! create takes its scripted time), so every number here is the test's.

use js_agent::lifecycle::{Event, StartTimeline};
use js_agent_wire::Command;

use super::per_job::job_start;
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

/// Asserts the timeline of a start with layers and mounts: its host paths are found and its
/// directories made at one step (20 after it arrived), its assembly answered at the next
/// (50), and its container created then. Catches the steps of a start that waits for its
/// file system dropped or put out of order.
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
        steps: vec![
            ("taken", 0),
            ("admitted", 0),
            ("resolved", 20),
            ("dirs", 20),
            ("ready", 50),
            ("created", 50),
        ],
        runtime: Vec::new(),
    };
    assert_eq!(h.life.step(), started("j1", timeline));
}

/// Asserts that a start whose create failed is not logged as started, and that the next
/// start's timeline is its own: it carries none of the failed start's steps. Catches a
/// timeline kept after its start failed and handed to another.
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
