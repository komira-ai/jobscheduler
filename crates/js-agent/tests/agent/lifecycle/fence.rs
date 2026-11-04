//! The fence of a job start, the job id with the allocation id: a start whose fence is
//! retired runs nothing and books nothing, the same job under a new allocation runs, a run
//! start is judged by its run id as before, and the fences retired by a stop or a silence
//! are in the state root before the stop is reported and survive a restart.

use std::fs;
use std::path::PathBuf;

use js_agent::lifecycle::{Clock, detail};
use js_agent_wire::{Command, Outcome};
use serde_json::{Value, json};

use super::silence::{BOUND_MS, fenced, job_start, long_run, result_of};
use super::{Harness, names, reply_json};

/// The retired list's file.
fn retired_file(h: &Harness) -> PathBuf {
    h.dir.path("state").join("retired.json")
}

/// The retired list as its file says it.
fn retired_list(h: &Harness) -> Value {
    serde_json::from_str(&fs::read_to_string(retired_file(h)).unwrap()).unwrap()
}

/// Applies a reply that acknowledges the result of the job `job_id`.
fn ack_job(h: &mut Harness, job_id: &str) -> Vec<String> {
    let reply = reply_json(&json!({ "job_acks": [{ "job_id": job_id, "ack": "ACCEPTED" }] }));
    names(&h.life.apply(&reply))
}

/// The run ids of the containers created, in order.
fn created(h: &Harness) -> Vec<String> {
    h.fake.created().into_iter().map(|c| c.run_id).collect()
}

/// A harness in which the job `j1` ran under `a1`, ended and had its result acknowledged:
/// its fence is retired.
fn ended_job(name: &str) -> Harness {
    let mut h = Harness::new(name);
    h.apply(vec![job_start("j1", "a1")]);
    h.fake.exit("j1", 0);
    h.life.step();
    ack_job(&mut h, "j1");
    assert!(h.life.held_jobs().is_empty());
    h
}

/// Asserts that a job that ran and ended, its result acknowledged, is not run again by the
/// same start: the event is `FenceRetired`, the runtime is asked for nothing, and nothing
/// is booked (the whole offer is free, no slot is held, no scratch directory is made). It
/// catches a start of a finished job that runs it twice, and a fence check placed after
/// the admission, which would book the start before refusing it.
#[test]
fn a_start_with_a_retired_fence_is_refused() {
    let mut h = ended_job("fence-retired");
    let calls = h.log.borrow().len();
    let events = names(&h.apply(vec![job_start("j1", "a1")]));
    assert_eq!(events, ["skipped j1 FenceRetired"]);
    assert_eq!(created(&h), ["j1"]);
    assert_eq!(h.log.borrow().len(), calls, "no call to the runtime");
    assert_eq!(h.life.free(), h.config.capacity);
    assert!(h.life.busy_slots().is_empty());
    assert!(!h.scratch_of("j1").exists());
    assert!(h.life.held_jobs().is_empty());
}

/// Asserts that the same job under a new allocation is a new fence: after `j1` under `a1`
/// ended and was acknowledged, its start under `a2` runs, and the start under `a1` stays
/// refused beside it. It catches a retired set keyed by the job id alone, which would
/// refuse a job the server placed here again.
#[test]
fn the_same_job_with_a_new_allocation_is_accepted() {
    let mut h = ended_job("fence-new-allocation");
    let events = names(&h.apply(vec![job_start("j1", "a2")]));
    assert_eq!(events, ["started j1"]);
    assert_eq!(created(&h), ["j1", "j1"]);
    h.fake.exit("j1", 0);
    h.life.step();
    ack_job(&mut h, "j1");
    let events = names(&h.apply(vec![job_start("j1", "a1"), job_start("j1", "a2")]));
    assert_eq!(
        events,
        ["skipped j1 FenceRetired", "skipped j1 FenceRetired"]
    );
    assert_eq!(created(&h).len(), 2);
}

/// Asserts that a run start, which carries no fence beyond its run id, is judged as before
/// the fence: a run that ended is not run again under another lease id (`Retired`), nor is
/// one stopped before its start, and a run whose id was never retired runs. It catches a
/// fence check that reaches run starts and lets a finished run in under a new lease.
#[test]
fn a_start_without_a_fence_behaves_as_before() {
    let mut h = Harness::new("fence-runs");
    h.start(long_run("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    h.ack("r1", "ACCEPTED");
    let mut again = long_run("r1");
    again.lease_id = "another-lease".to_owned();
    assert_eq!(names(&h.start(again)), ["skipped r1 Retired"]);
    assert!(h.stop("r2").is_empty());
    assert_eq!(names(&h.start(long_run("r2"))), ["skipped r2 Retired"]);
    assert_eq!(names(&h.start(long_run("r3"))), ["started r3"]);
    assert_eq!(created(&h), ["r1", "r3"]);
}

/// Asserts that a stop retires the fence so that a late start never runs: a stop of a job
/// the agent does not hold, then its start, runs nothing (`FenceRetired`); a stop of a job
/// it holds puts the job with its allocation in the retired list on disk in the same call,
/// before the step that reports the stop; and a job stopped for a silence, once contact
/// is back and its result acknowledged, is not run again by its start. It catches a stop
/// that is forgotten when its start comes later, a fence written only once the job leaves
/// the listing (a restart before that would run it again), and a silence stop that does
/// not retire.
#[test]
fn a_stopped_job_retires_its_fence_so_a_late_start_never_runs() {
    let (mut h, contact) = fenced("fence-stopped");
    h.apply(vec![Command::JobStop {
        job_id: "j1".to_owned(),
    }]);
    assert_eq!(
        names(&h.apply(vec![job_start("j1", "a1")])),
        ["skipped j1 FenceRetired"]
    );
    assert!(created(&h).is_empty());
    assert!(!h.scratch_of("j1").exists());

    h.apply(vec![job_start("j2", "a2")]);
    h.apply(vec![Command::JobStop {
        job_id: "j2".to_owned(),
    }]);
    assert_eq!(
        retired_list(&h),
        json!({ "retired": [{ "job_id": "j1" }, { "job_id": "j2", "allocation_id": "a2" }] })
    );
    assert!(h.life.held_jobs()[0].result.is_none(), "not reported yet");
    h.life.step();
    assert_eq!(result_of(&h, "j2"), (Outcome::Killed, None));

    contact.heard(h.clock.now_ms());
    h.apply(vec![job_start("j3", "a3")]);
    h.clock.advance(BOUND_MS);
    h.life.step();
    let silenced = (Outcome::Killed, Some(detail::SILENCE_STOP.to_owned()));
    assert_eq!(result_of(&h, "j3"), silenced);
    contact.heard(h.clock.now_ms());
    ack_job(&mut h, "j2");
    ack_job(&mut h, "j3");
    let events = names(&h.apply(vec![job_start("j2", "a2"), job_start("j3", "a3")]));
    assert_eq!(
        events,
        ["skipped j2 FenceRetired", "skipped j3 FenceRetired"]
    );
    assert_eq!(created(&h), ["j2", "j3"]);
}

/// Asserts that the fences survive a restart: a second lifecycle over the same state root
/// refuses the fence of a job that ended, of one the server stopped while it ran, and of
/// one stopped for a silence (its fence on disk in the step that made its result, before
/// any heartbeat could carry it), once each result is acknowledged; and takes the last
/// job under a new allocation. It catches a fence kept in memory only, and one written
/// after its stop was reported.
#[test]
fn retired_fences_survive_an_agent_restart() {
    let (mut h, _contact) = fenced("fence-restart");
    h.apply(vec![job_start("j1", "a1")]);
    h.fake.exit("j1", 0);
    h.life.step();
    ack_job(&mut h, "j1");
    h.apply(vec![job_start("j2", "a2"), job_start("j3", "a3")]);
    h.apply(vec![Command::JobStop {
        job_id: "j2".to_owned(),
    }]);
    h.clock.advance(BOUND_MS);
    h.life.step();
    assert_eq!(h.life.held_jobs().len(), 2);
    assert!(h.life.held_jobs().iter().all(|job| job.result.is_some()));
    assert_eq!(
        retired_list(&h),
        json!({ "retired": [
            { "job_id": "j1", "allocation_id": "a1" },
            { "job_id": "j2", "allocation_id": "a2" },
            { "job_id": "j3", "allocation_id": "a3" },
        ] })
    );

    h.restart().unwrap();
    ack_job(&mut h, "j2");
    ack_job(&mut h, "j3");
    assert!(h.life.held_jobs().is_empty());
    let starts = vec![
        job_start("j1", "a1"),
        job_start("j2", "a2"),
        job_start("j3", "a3"),
    ];
    let events = names(&h.apply(starts));
    assert_eq!(
        events,
        [
            "skipped j1 FenceRetired",
            "skipped j2 FenceRetired",
            "skipped j3 FenceRetired",
        ]
    );
    assert_eq!(created(&h), ["j1", "j2", "j3"]);
    assert_eq!(names(&h.apply(vec![job_start("j3", "a4")])), ["started j3"]);
}

/// Asserts the end of a job the server gave up while the host could not hear it: job A
/// runs and can make its one write (the fake lets a write through only while the
/// container runs); the server is silent past the bound (it cancelled A and gave the host
/// to B meanwhile), so A's container is stopped and its write now fails; when contact is
/// back the server takes A's result, B's start runs and can write, and A's old start, sent
/// again, runs nothing. It catches a job that can still write when the host's next job
/// starts, and a fence of A that blocks B.
#[test]
fn a_cancelled_job_cannot_write_after_the_host_is_reassigned() {
    let (mut h, contact) = fenced("fence-reassigned");
    contact.heard(h.clock.now_ms());
    h.apply(vec![job_start("ja", "alloc-a")]);
    assert!(h.fake.running("ja"), "A can write while it runs");

    h.clock.advance(BOUND_MS);
    h.life.step();
    assert!(!h.fake.running("ja"), "A's write fails once it is stopped");

    contact.heard(h.clock.now_ms());
    ack_job(&mut h, "ja");
    let events = names(&h.apply(vec![job_start("ja", "alloc-a"), job_start("jb", "alloc-b")]));
    assert_eq!(events, ["skipped ja FenceRetired", "started jb"]);
    assert!(h.fake.running("jb"), "B runs and can write");
    assert!(!h.fake.running("ja"), "A cannot write while B runs");
    assert_eq!(created(&h), ["ja", "jb"]);
}
