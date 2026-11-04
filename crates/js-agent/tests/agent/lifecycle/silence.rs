//! The stop after a silence of the server (`Lifecycle::stop_on_silence`): the bound counts
//! on the lifecycle's clock from the last contact, every job the agent holds is stopped as
//! a `Stop` stops it once the bound is reached, each result says `silence-stop`, and the
//! fence of each is retired. The clock and the runtime are the fakes of this module; no
//! test waits.

use std::time::Duration;

use js_agent::lifecycle::{Clock, Contact, detail};
use js_agent_wire::{BookedResources, Command, JobSpec, JobStartSpec, Outcome};
use serde_json::json;

use super::{GRACE, Harness, job, names, reply_json};

/// The silence bound of these tests.
const BOUND: Duration = Duration::from_secs(300);

/// The bound in milliseconds of the lifecycle's clock.
pub const BOUND_MS: u64 = 300_000;

/// A wall limit no test reaches: a day.
const DAY_MS: u64 = 86_400_000;

const IMAGE: &str = "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A harness whose lifecycle stops its jobs after [`BOUND`] of silence, and the contact
/// the test stamps.
pub fn fenced(name: &str) -> (Harness, Contact) {
    let mut h = Harness::new(name);
    let contact = Contact::default();
    h.life.stop_on_silence(BOUND, contact.clone());
    (h, contact)
}

/// A run start for `run_id` with a wall limit of a day.
pub fn long_run(run_id: &str) -> JobSpec {
    let mut spec = job(run_id);
    spec.limits.wall_ms = DAY_MS;
    spec
}

/// A job start of `job_id` under `allocation`, with a wall limit of a day.
pub fn job_start(job_id: &str, allocation: &str) -> Command {
    Command::JobStart(Box::new(JobStartSpec {
        job_id: job_id.to_owned(),
        allocation_id: allocation.to_owned(),
        image: IMAGE.to_owned(),
        command: vec!["--go".to_owned()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: DAY_MS,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
    }))
}

/// The outcome and detail of the result of the run or job `id`, which must be held.
pub fn result_of(h: &Harness, id: &str) -> (Outcome, Option<String>) {
    let run = h.life.results().into_iter().find(|r| r.run_id == id);
    let run = run.map(|r| (r.outcome, r.detail));
    let jobs = h.life.held_jobs().into_iter().filter_map(|j| j.result);
    let job = jobs
        .filter(|r| r.job_id == id)
        .map(|r| (r.outcome, r.detail))
        .next();
    run.or(job).expect("the result is held")
}

/// The ids of the runs and jobs listed as running.
fn running(h: &Harness) -> Vec<String> {
    use js_agent_wire::HeldState::Running;
    let runs = h.life.held().into_iter().filter(|r| r.state == Running);
    let jobs = h
        .life
        .held_jobs()
        .into_iter()
        .filter(|j| j.state == Running);
    runs.map(|r| r.run_id)
        .chain(jobs.map(|j| j.job_id))
        .collect()
}

/// Asserts that with two jobs running (a run and a job) and no contact since the first
/// stamp, a step one millisecond before the bound stops nothing and both still run; that
/// the step at the bound stops both, each once and with the stop grace (the runtime's stop
/// is SIGTERM, that grace, then the kill), each container having taken the TERM (one
/// handles it and exits 143) and been removed; that each result is `killed` with the
/// detail `silence-stop` and a `silence-stop` finding names each; and that steps after
/// that stop nothing more. It catches a check that reads another clock than the
/// lifecycle's, one that counts from the start of the jobs or from the arming instead of
/// the last contact, a stop of one job only, a stop without the grace, and a result that
/// does not say why.
#[test]
fn a_partitioned_agent_stops_its_jobs_after_the_bound() {
    let (mut h, contact) = fenced("silence-partitioned");
    h.clock.advance(10_000);
    h.start(long_run("r1"));
    h.clock.advance(20_000);
    contact.heard(h.clock.now_ms());
    h.apply(vec![job_start("j1", "a1")]);
    h.fake.on_term("j1", 143);
    h.clock.advance(BOUND_MS - 1);
    let events = names(&h.life.step());
    assert!(events.is_empty(), "{events:?}");
    assert!(h.fake.stops().is_empty());
    assert_eq!(running(&h), ["r1", "j1"]);

    h.clock.advance(1);
    let events = names(&h.life.step());
    assert_eq!(
        h.fake.stops(),
        [("c1".to_owned(), GRACE), ("c2".to_owned(), GRACE)]
    );
    for id in ["r1", "j1"] {
        assert!(
            events.contains(&format!("finding {id} silence-stop")),
            "{events:?}"
        );
        assert!(events.contains(&format!("result {id}")), "{events:?}");
        let silenced = (Outcome::Killed, Some(detail::SILENCE_STOP.to_owned()));
        assert_eq!(result_of(&h, id), silenced, "{id}");
        assert!(h.fake.gone(id), "{id}");
    }
    assert!(running(&h).is_empty());
    let log = h.log.borrow().clone();
    let stop = log.iter().position(|call| call == "stop c1");
    let remove = log.iter().position(|call| call.starts_with("remove c1 "));
    assert!(stop.is_some() && stop < remove, "{log:?}");

    h.clock.advance(BOUND_MS);
    h.life.step();
    assert_eq!(h.fake.stops().len(), 2, "stopped once for the silence");
}

/// Asserts that a contact within the bound restarts the count: stamps every 299 s for
/// three bounds stop nothing, and the silence after the last stamp stops the job at that
/// stamp plus the bound, not before. That a job started during a silence that has already
/// been answered by a stop is stopped at the next bound after the contact that brought it.
/// It catches a silence counted from the first contact, or from a stamp the session never
/// made.
#[test]
fn every_contact_restarts_the_count() {
    let (mut h, contact) = fenced("silence-contact");
    h.start(long_run("r1"));
    for _ in 0..3 {
        h.clock.advance(BOUND_MS - 1_000);
        contact.heard(h.clock.now_ms());
        h.life.step();
    }
    assert!(h.fake.stops().is_empty());
    h.clock.advance(BOUND_MS - 1);
    h.life.step();
    assert!(h.fake.stops().is_empty());
    h.clock.advance(1);
    h.life.step();
    assert_eq!(h.fake.stops().len(), 1);

    contact.heard(h.clock.now_ms());
    h.start(long_run("r2"));
    h.life.step();
    assert_eq!(running(&h), ["r2"]);
    h.clock.advance(BOUND_MS);
    h.life.step();
    assert_eq!(h.fake.stops().len(), 2, "the new silence stops the new job");
    let silenced = (Outcome::Killed, Some(detail::SILENCE_STOP.to_owned()));
    assert_eq!(result_of(&h, "r2"), silenced);
}

/// Asserts that a lifecycle with no contact yet counts the silence from the moment the
/// watch was set: a job adopted after a restart (or started before any welcome) is stopped
/// at that moment plus the bound. And that a lifecycle on which no watch was set stops
/// nothing however long it hears nothing. It catches a watch that never fires before the
/// first contact (an agent that cannot reach its server would run its jobs for ever) and
/// a stop made by a lifecycle that was not told to.
#[test]
fn before_any_contact_the_count_starts_with_the_watch() {
    let mut h = Harness::new("silence-unarmed");
    h.start(long_run("r1"));
    h.clock.advance(10 * BOUND_MS);
    h.life.step();
    assert!(h.fake.stops().is_empty(), "no watch, no stop");

    let contact = Contact::default();
    h.life.stop_on_silence(BOUND, contact);
    h.clock.advance(BOUND_MS - 1);
    h.life.step();
    assert!(h.fake.stops().is_empty());
    h.clock.advance(1);
    h.life.step();
    assert_eq!(h.fake.stops().len(), 1);
}

/// Asserts that a job still waiting for its image is ended by a silence without a
/// container (`killed`, `silence-stop`), that a job already stopped by the server keeps
/// the server's reason (no `silence-stop` detail), and that a cancelled job stays silent:
/// no result. It catches a silence that makes a container for a job it stops, one that
/// rewrites the reason of a stop the server asked for, and a cancel that reports.
#[test]
fn a_silence_ends_a_waiting_job_and_keeps_the_reason_of_a_stop_already_asked() {
    let (mut h, contact) = fenced("silence-kinds");
    contact.heard(h.clock.now_ms());
    let mut waiting = long_run("r1");
    waiting.image = "registry.example/slow:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned();
    h.fake.pull_pending(&waiting.image);
    h.start(waiting);
    h.start(long_run("r2"));
    h.start(long_run("r3"));
    h.fake.fail("stop", 1);
    h.stop("r2");
    h.life.step();
    h.cancel("r3");
    h.clock.advance(BOUND_MS);
    h.life.step();
    let silenced = (Outcome::Killed, Some(detail::SILENCE_STOP.to_owned()));
    assert_eq!(result_of(&h, "r1"), silenced);
    assert_eq!(result_of(&h, "r2"), (Outcome::Killed, None));
    assert!(h.life.results().iter().all(|r| r.run_id != "r3"));
    let created: Vec<String> = h.fake.created().into_iter().map(|c| c.run_id).collect();
    assert_eq!(created, ["r2", "r3"], "the waiting job made no container");
}

/// Asserts that the stop of a silence is written down before it is reported: in the step
/// that makes the `silence-stop` results, the retired list in the state root already names
/// each fence (a run with its lease, a job with its allocation). It catches a fence written
/// after the result went out, which a restart in between would forget.
#[test]
fn the_fences_of_a_silence_are_on_disk_before_the_results() {
    let (mut h, _contact) = fenced("silence-disk");
    h.start(long_run("r1"));
    h.apply(vec![job_start("j1", "a1")]);
    h.clock.advance(BOUND_MS);
    h.life.step();
    assert_eq!(h.life.results().len(), 1);
    assert_eq!(h.life.held_jobs().len(), 1);
    let path = h.dir.path("state").join("retired.json");
    let list: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(
        list,
        json!({ "retired": [
            { "job_id": "r1", "allocation_id": "lease-r1" },
            { "job_id": "j1", "allocation_id": "a1" },
        ] })
    );
    // The reply that acknowledges both drops the results and adds nothing to the list.
    h.life.apply(&reply_json(&json!({
        "result_acks": [{ "run_id": "r1", "ack": "ACCEPTED" }],
        "job_acks": [{ "job_id": "j1", "ack": "ACCEPTED" }],
    })));
    assert!(h.life.results().is_empty() && h.life.held_jobs().is_empty());
}
