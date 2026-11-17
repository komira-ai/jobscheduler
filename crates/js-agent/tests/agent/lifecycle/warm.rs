//! Containers made ahead of their jobs, as the lifecycle sees them through the runtime's
//! hooks: the step times a start reports (and whether it started warm), the call of
//! `tend` once at every step, and the idle containers counted as live at every check. The
//! clock is the test's, so no time passes unless a test moves it.

use js_agent::lifecycle::{CheckFinding, Event, FsPrepared, Quiet, StartSteps};
use js_agent_wire::{Command, Layer};
use serde_json::json;

use super::per_job::{job_start, start};
use super::{Harness, Options, job, reply, reply_json};

const COLD: &str = "registry.example/cold:1@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// The steps of the one `Started` event among `events`.
fn steps_of(events: &[Event]) -> StartSteps {
    let started: Vec<StartSteps> = events
        .iter()
        .filter_map(|event| match event {
            Event::Started { steps, .. } => Some(*steps),
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 1, "{events:?}");
    started[0]
}

/// The "live beside" lines the scripted checks logged.
fn live_beside(h: &Harness) -> Vec<String> {
    let log = h.log.borrow();
    log.iter()
        .filter(|line| line.starts_with("live beside"))
        .cloned()
        .collect()
}

/// A start that waits for its image reports each step at the clock's time after the reply
/// came: admitted at once, ready and created at the step that found the image here, started
/// when the create returned. Catches steps counted from the wrong moment (the step's, not
/// the reply's), and a ready time taken at admission for a start that waited.
#[test]
fn a_start_that_waits_reports_its_steps_from_the_reply() {
    let mut h = Harness::new("warm-steps-wait");
    h.clock.advance(1_000);
    h.fake.pull_pending(COLD);
    h.fake.create_takes(70);
    let mut spec = job("r1");
    spec.image = COLD.to_owned();
    assert_eq!(h.start(spec), []);
    h.clock.advance(300);
    assert_eq!(h.life.step(), []);
    h.fake.pull_done(COLD);
    h.clock.advance(200);
    let steps = steps_of(&h.life.step());
    assert_eq!(
        steps,
        StartSteps {
            admitted: 0,
            ready: 500,
            dirs: 500,
            create: 500,
            started: 570,
            warm: false,
        }
    );
}

/// A start of a file system that is assembled after its directories were made reports the
/// time of each: ready when its paths were found, its directories then, created when the
/// assembly answered. Catches a create time stamped at the ready step for a start that
/// waited on its assembly after it.
#[test]
fn a_start_that_waits_for_its_assembly_reports_ready_before_create() {
    let mut h = Harness::new("warm-steps-assembly");
    h.fake.give_layers();
    h.fake.keep_lease_dirs_in(&h.dir.path("scratch"));
    h.clock.advance(1_000);
    h.fake.create_takes(40);
    h.fake.resolve_answers("j1", FsPrepared::Pending);
    let mut spec = start("j1");
    spec.layers = vec![Layer::Image {
        image: COLD.to_owned(),
    }];
    assert_eq!(h.apply(vec![job_start(spec)]), []);
    h.fake.pull_done(COLD);
    h.clock.advance(250);
    h.fake.resolve_answers("j1", FsPrepared::Ready);
    h.fake.assembly_answers("j1", FsPrepared::Pending);
    assert_eq!(h.life.step(), []);
    h.clock.advance(350);
    h.fake.assembly_answers("j1", FsPrepared::Ready);
    let steps = steps_of(&h.life.step());
    assert_eq!(
        steps,
        StartSteps {
            admitted: 0,
            ready: 250,
            dirs: 250,
            create: 600,
            started: 640,
            warm: false,
        }
    );
}

/// A start that does not wait reports admitted, ready, dirs and create all at the time of
/// the reply, and started when the create returned. Catches a step left at a default when
/// the start never waited.
#[test]
fn a_start_that_does_not_wait_reports_every_step_at_the_receipt() {
    let mut h = Harness::new("warm-steps-now");
    h.clock.advance(2_000);
    h.fake.create_takes(70);
    let steps = steps_of(&h.start(job("r1")));
    assert_eq!(
        steps,
        StartSteps {
            admitted: 0,
            ready: 0,
            dirs: 0,
            create: 0,
            started: 70,
            warm: false,
        }
    );
}

/// A reply that was received before the lifecycle got to it has its steps counted from the
/// receipt: a start applied 40 ms after it arrived is admitted at 40. Catches steps counted
/// from the moment the apply began.
#[test]
fn steps_are_counted_from_the_receipt_of_the_reply() {
    let mut h = Harness::new("warm-steps-received");
    h.clock.advance(1_000);
    let received = 1_000;
    h.clock.advance(40);
    let reply = reply(vec![Command::Start(Box::new(job("r1")))]);
    let steps = steps_of(&h.life.apply_at(&reply, received));
    assert_eq!(
        steps,
        StartSteps {
            admitted: 40,
            ready: 40,
            dirs: 40,
            create: 40,
            started: 40,
            warm: false,
        }
    );
}

/// `warm` is what the runtime says of the container the run started in: a run in a
/// container made ahead reports it, and another run beside it does not. Catches a flag read
/// for the wrong handle, and one never read.
#[test]
fn a_start_reports_whether_the_runtime_made_its_container_ahead() {
    let mut h = Harness::new("warm-flag");
    h.fake.ahead.started_in("c2");
    let first = steps_of(&h.start(job("r1")));
    let second = steps_of(&h.start(job("r2")));
    assert!(!first.warm);
    assert!(second.warm);
}

/// The runtime is tended once at every step with the clock's reading, and not by a reply.
/// Catches a tend from `apply` (a start's path), a step that never tends, and one that
/// tends twice.
#[test]
fn the_runtime_is_tended_once_per_step_with_the_clock_and_not_by_a_reply() {
    let mut h = Harness::new("warm-tend");
    h.clock.advance(5);
    h.start(job("r1"));
    h.life.apply(&reply_json(&json!({})));
    assert!(h.fake.ahead.tended().is_empty());
    h.life.step();
    h.clock.advance(100);
    h.life.step();
    h.life.step();
    assert_eq!(h.fake.ahead.tended(), [5, 105, 105]);
}

/// The idle containers of the runtime are live beside a job at its post-job check. Catches a
/// check that is given the jobs' containers alone, which would blame a job for the processes
/// of a sandbox.
#[test]
fn the_idle_containers_are_live_at_the_check_after_a_job() {
    let mut h = Harness::new("warm-live-job");
    h.fake.ahead.hold_idle(&["warm-1", "warm-2"]);
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(live_beside(&h), ["live beside r1: c2,warm-1,warm-2"]);
}

/// The idle containers are live at the re-check of a taint, which has no job of its own.
/// Catches a re-check that is given nothing live.
#[test]
fn the_idle_containers_are_live_at_the_recheck_of_a_taint() {
    let mut h = Harness::with(
        "warm-live-recheck",
        Options {
            findings: vec![("r1", CheckFinding::residue("daemonised-child", "pid 41"))],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(h.life.is_tainted());
    h.life.apply(&reply_json(&json!({ "taint_ack": true })));
    h.fake.ahead.hold_idle(&["warm-1"]);
    h.life.step();
    h.clock.advance(60_000);
    h.life.step();
    assert_eq!(live_beside(&h), ["live beside taint-recheck: warm-1"]);
}

/// The idle containers are live at the check that hands a host to an exclusive lease.
/// Catches a quiet check that is given nothing live.
#[test]
fn the_idle_containers_are_live_at_the_check_for_an_exclusive_lease() {
    let mut h = Harness::new("warm-live-exclusive");
    h.fake.ahead.hold_idle(&["warm-1"]);
    h.life
        .apply(&reply_json(&json!({ "exclusive": "whole-1" })));
    let mut events = Vec::new();
    assert_eq!(h.life.prepare("whole-1", &mut events), Quiet::Ready);
    assert_eq!(live_beside(&h), ["live beside whole-1: warm-1"]);
}
