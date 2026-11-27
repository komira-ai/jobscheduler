//! What a silence of the server stops (`Lifecycle::stop_on_silence`): the bound counts on
//! the lifecycle's clock from the last contact; once it is reached, every job that waits to
//! start is ended without a container (`failed_to_start`, `silence-stop`, its fence
//! retired), every start that comes while the silence lasts is refused with the same code,
//! and the running jobs run on to their own ends, which are reported as they were. The
//! clock and the runtime are the fakes of this module; no test waits.

use std::time::Duration;

use js_agent::lifecycle::{Clock, Contact, detail};
use js_agent_wire::{BookedResources, Command, JobSpec, JobStartSpec, Outcome};
use serde_json::json;

use super::deferring::Deferring;
use super::{Harness, job, names, reply_json};

/// The silence bound of these tests.
const BOUND: Duration = Duration::from_secs(300);

/// The bound in milliseconds of the lifecycle's clock.
pub const BOUND_MS: u64 = 300_000;

/// A wall limit no test reaches: a day.
const DAY_MS: u64 = 86_400_000;

const IMAGE: &str = "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// An image whose pull the tests hold open, so a start of it waits.
const SLOW_IMAGE: &str = "registry.example/slow:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The outcome and detail of a start the agent did not make for a silence.
pub fn not_started() -> (Outcome, Option<String>) {
    (
        Outcome::FailedToStart,
        Some(detail::SILENCE_STOP.to_owned()),
    )
}

/// A harness whose lifecycle stops starting work after [`BOUND`] of silence, and the
/// contact the test stamps.
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

/// A run start for `run_id` of [`SLOW_IMAGE`]: it waits while the test holds that pull
/// open ([`hold_pull`]).
fn waiting_run(run_id: &str) -> JobSpec {
    let mut spec = long_run(run_id);
    spec.image = SLOW_IMAGE.to_owned();
    spec
}

/// Holds the pull of [`SLOW_IMAGE`] open, so every start of it waits.
pub fn hold_pull(h: &Harness) {
    h.fake.pull_pending(SLOW_IMAGE);
}

/// A job start of `job_id` under `allocation`, with a wall limit of a day.
pub fn job_start(job_id: &str, allocation: &str) -> Command {
    job_start_of(job_id, allocation, IMAGE)
}

/// A job start of `job_id` under `allocation` of [`SLOW_IMAGE`].
pub fn waiting_job_start(job_id: &str, allocation: &str) -> Command {
    job_start_of(job_id, allocation, SLOW_IMAGE)
}

fn job_start_of(job_id: &str, allocation: &str, image: &str) -> Command {
    Command::JobStart(Box::new(JobStartSpec {
        job_id: job_id.to_owned(),
        allocation_id: allocation.to_owned(),
        image: image.to_owned(),
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
        kind: Default::default(),
        isolation: Default::default(),
        fuse: false,
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
        placed_at_ms: None,
        xcode: None,
        sizing: None,
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

/// The exit code the result of the run or job `id` carries.
fn exit_of(h: &Harness, id: &str) -> Option<i32> {
    let run = h.life.results().into_iter().find(|r| r.run_id == id);
    let jobs = h.life.held_jobs().into_iter().filter_map(|j| j.result);
    let job = jobs.filter(|r| r.job_id == id).map(|r| r.exit_code).next();
    run.map(|r| r.exit_code).or(job).flatten()
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
/// stamp, the steps at the bound, one millisecond after it and ten bounds later stop
/// neither, make no finding and leave both running; and that each then ends as its
/// container does and is reported as it ended: the run `succeeded` with exit 0, the job
/// `failed` with exit 3, neither with a `silence-stop` detail. It catches a silence that
/// stops a job whose container runs (the work the server never gives away), and one that
/// rewrites the result of a job that ended by itself during the silence.
#[test]
fn a_partitioned_agent_keeps_its_running_jobs_to_their_own_ends() {
    let (mut h, contact) = fenced("silence-partitioned");
    h.clock.advance(10_000);
    h.start(long_run("r1"));
    h.clock.advance(20_000);
    contact.heard(h.clock.now_ms());
    h.apply(vec![job_start("j1", "a1")]);
    for step in [BOUND_MS, 1, 10 * BOUND_MS] {
        h.clock.advance(step);
        let events = names(&h.life.step());
        assert!(events.is_empty(), "{events:?}");
    }
    assert!(h.fake.stops().is_empty());
    assert_eq!(running(&h), ["r1", "j1"]);

    h.fake.exit("r1", 0);
    h.fake.exit("j1", 3);
    let events = names(&h.life.step());
    assert!(events.contains(&"result r1".to_owned()), "{events:?}");
    assert!(events.contains(&"result j1".to_owned()), "{events:?}");
    assert_eq!(result_of(&h, "r1"), (Outcome::Succeeded, None));
    assert_eq!(exit_of(&h, "r1"), Some(0));
    assert_eq!(result_of(&h, "j1"), (Outcome::Failed, None));
    assert_eq!(exit_of(&h, "j1"), Some(3));
    assert!(h.fake.stops().is_empty());
}

/// Asserts that at the bound a run and a job that wait for their image are ended without a
/// container, each `failed_to_start` with the detail `silence-stop` and a `silence-stop`
/// finding, while a job that runs beside them is not touched; that a run start and a job
/// start that come while the silence lasts are refused `silence-stop` (no container, no
/// slot); and that after a contact a start runs again. A start one millisecond before the
/// bound is taken. It catches a silence that lets a waiting start go on to run (its running
/// report could reach no server, whose start timeout may give the job to another attempt),
/// one that takes new work while the server cannot hear the host, one that reports a job
/// that never ran as `killed` (the job's own fault to the server), and one that stays
/// after the server is heard.
#[test]
fn a_silence_ends_the_waiting_starts_and_refuses_new_ones() {
    let (mut h, contact) = fenced("silence-starts");
    contact.heard(h.clock.now_ms());
    hold_pull(&h);
    h.start(long_run("r0"));
    h.start(waiting_run("r1"));
    h.apply(vec![waiting_job_start("j1", "a1")]);
    h.clock.advance(BOUND_MS - 1);
    assert_eq!(names(&h.start(long_run("r2"))), ["started r2"]);

    h.clock.advance(1);
    let events = names(&h.life.step());
    for id in ["r1", "j1"] {
        assert!(
            events.contains(&format!("finding {id} silence-stop")),
            "{events:?}"
        );
        assert!(events.contains(&format!("result {id}")), "{events:?}");
        assert_eq!(result_of(&h, id), not_started(), "{id}");
    }
    assert!(!events.iter().any(|e| e.contains("r0") || e.contains("r2")));
    assert_eq!(running(&h), ["r0", "r2"]);
    let busy = h.life.busy_slots().len();

    assert_eq!(names(&h.start(long_run("r3"))), ["refused r3 silence-stop"]);
    assert_eq!(
        names(&h.apply(vec![job_start("j2", "a2")])),
        ["refused j2 silence-stop"]
    );
    assert_eq!(result_of(&h, "r3"), not_started());
    assert_eq!(result_of(&h, "j2"), not_started());
    assert_eq!(h.life.busy_slots().len(), busy, "a refusal books nothing");
    let created: Vec<String> = h.fake.created().into_iter().map(|c| c.run_id).collect();
    assert_eq!(
        created,
        ["r0", "r2"],
        "nothing that waited or came was made"
    );

    contact.heard(h.clock.now_ms());
    assert_eq!(names(&h.start(long_run("r4"))), ["started r4"]);
    assert!(h.fake.stops().is_empty());
}

/// Asserts that a contact within the bound restarts the count: stamps every 299 s for
/// three bounds leave a start taken, a start one millisecond short of the last stamp plus
/// the bound is taken and one at it is refused; and that a new contact ends the silence
/// so the next start is taken and a later silence refuses again. It catches a silence
/// counted from the first contact, from a stamp the session never made, and a refusal that
/// outlives the silence.
#[test]
fn every_contact_restarts_the_count() {
    let (mut h, contact) = fenced("silence-contact");
    for _ in 0..3 {
        h.clock.advance(BOUND_MS - 1_000);
        contact.heard(h.clock.now_ms());
        h.life.step();
    }
    h.clock.advance(BOUND_MS - 1);
    assert_eq!(names(&h.start(long_run("r1"))), ["started r1"]);
    h.clock.advance(1);
    assert_eq!(names(&h.start(long_run("r2"))), ["refused r2 silence-stop"]);

    contact.heard(h.clock.now_ms());
    assert_eq!(names(&h.start(long_run("r3"))), ["started r3"]);
    h.clock.advance(BOUND_MS);
    assert_eq!(names(&h.start(long_run("r4"))), ["refused r4 silence-stop"]);
    assert_eq!(running(&h), ["r1", "r3"]);
}

/// Asserts that a lifecycle with no contact yet counts the silence from the moment the
/// watch was set: a start one millisecond short of that moment plus the bound is taken and
/// one at it is refused. And that a lifecycle on which no watch was set takes a start however
/// long it hears nothing. It catches a watch that never fires before the first contact (an
/// agent that cannot reach its server would go on taking work it cannot report) and a
/// refusal made by a lifecycle that was not told to.
#[test]
fn before_any_contact_the_count_starts_with_the_watch() {
    let mut h = Harness::new("silence-unarmed");
    h.clock.advance(10 * BOUND_MS);
    assert_eq!(names(&h.start(long_run("r1"))), ["started r1"]);

    let contact = Contact::default();
    h.life.stop_on_silence(BOUND, contact);
    h.clock.advance(BOUND_MS - 1);
    assert_eq!(names(&h.start(long_run("r2"))), ["started r2"]);
    h.clock.advance(1);
    assert_eq!(names(&h.start(long_run("r3"))), ["refused r3 silence-stop"]);
    assert!(h.fake.stops().is_empty());
}

/// Asserts that a waiting job the server already stopped keeps the server's reason
/// (`killed`, no detail), that a cancelled waiting job stays silent (no result), and that a
/// running job the server stopped keeps its own stop (`killed`, no detail) through the
/// silence. It catches a silence that rewrites the reason of a stop the server asked for,
/// and a cancel that reports.
#[test]
fn a_silence_keeps_the_reason_of_a_stop_already_asked() {
    let (mut h, contact) = fenced("silence-kinds");
    contact.heard(h.clock.now_ms());
    hold_pull(&h);
    h.start(waiting_run("r1"));
    h.start(waiting_run("r3"));
    h.start(long_run("r2"));
    h.fake.fail("stop", 1);
    h.stop("r2");
    h.life.step();
    h.stop("r1");
    h.cancel("r3");
    h.clock.advance(BOUND_MS);
    h.life.step();
    assert_eq!(result_of(&h, "r1"), (Outcome::Killed, None));
    assert_eq!(result_of(&h, "r2"), (Outcome::Killed, None));
    assert!(h.life.results().iter().all(|r| r.run_id != "r3"));
    let created: Vec<String> = h.fake.created().into_iter().map(|c| c.run_id).collect();
    assert_eq!(created, ["r2"], "the waiting jobs made no container");
}

/// Asserts that the end of a silence is written down before it is reported: in the step
/// that makes the `silence-stop` results of a waiting run and a waiting job, the retired
/// list in the state root already names each fence (the run with its lease, the job with
/// its allocation), and a job that runs is not in it. It catches a fence written after the
/// result went out, which a restart in between would forget, and one retired for a job the
/// silence left running.
#[test]
fn the_fences_of_a_silence_are_on_disk_before_the_results() {
    let (mut h, _contact) = fenced("silence-disk");
    hold_pull(&h);
    h.apply(vec![job_start("j0", "a0")]);
    h.start(waiting_run("r1"));
    h.apply(vec![waiting_job_start("j1", "a1")]);
    h.clock.advance(BOUND_MS);
    h.life.step();
    assert_eq!(h.life.results().len(), 1);
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
    assert!(h.life.results().is_empty());
    assert_eq!(running(&h), ["j0"]);
}

/// Asserts that a start whose container create is under way when the bound is reached is
/// not ended for the silence: once the create answers, the job is listed running, has no
/// `silence-stop` finding or result, and its container is never stopped. It catches a
/// silence that marks a create in flight for a stop, which would kill the container the
/// runtime started anyway and report a job that ran as `killed`.
#[test]
fn a_create_under_way_at_the_bound_runs() {
    let (mut h, gate) = Deferring::harness("silence-creating");
    let contact = Contact::default();
    h.life.stop_on_silence(BOUND, contact.clone());
    contact.heard(h.clock.now_ms());
    gate.hold("r1");
    assert!(h.start(long_run("r1")).is_empty());
    h.clock.advance(BOUND_MS);
    let events = names(&h.life.step());
    assert!(events.is_empty(), "{events:?}");
    gate.release("r1");
    let events = names(&h.life.step());
    assert_eq!(events, ["started r1"]);
    assert_eq!(running(&h), ["r1"]);
    assert!(h.fake.stops().is_empty());
    assert!(h.life.results().is_empty());
}
