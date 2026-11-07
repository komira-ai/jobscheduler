//! The job keying through the worker and the session: what the snapshot and the heartbeat
//! carry of the jobs a job start began, and what a drain and a stop of everything do to
//! them.

use std::time::Duration;

use js_agent::{Delivery, Drain, HEARTBEAT_PATH, HELLO_PATH, Session};
use js_agent_wire::{BookedResources, Command, HeartbeatReply, HeldState, JobStartSpec, Outcome};
use serde_json::{Value, json};

use super::fake::FakeMachine;
use super::{start, starts};
use crate::support::{Scratch, Script, beat_ok, hello, lowest, reply, welcome};

/// The drain the sessions of these tests carry; none of them drains.
const DRAIN: Drain = Drain {
    wait: Duration::from_secs(10),
    settle: Duration::from_secs(30),
};

/// A reply that starts these jobs, keyed by job id.
fn job_starts(jobs: &[&str]) -> HeartbeatReply {
    let commands = jobs
        .iter()
        .map(|id| {
            Command::JobStart(Box::new(JobStartSpec {
                job_id: (*id).to_owned(),
                allocation_id: format!("alloc-{id}"),
                image: "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
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
                wall_ms: 60_000,
                network: Default::default(),
                ports: Vec::new(),
                log_sink: None,
                kind: Default::default(),
            }))
        })
        .collect();
    HeartbeatReply {
        commands,
        ..HeartbeatReply::default()
    }
}

/// A reply of this JSON body.
fn body(value: Value) -> HeartbeatReply {
    serde_json::from_value(value).unwrap()
}

/// Asserts that the worker publishes a job a job start began in the snapshot's `jobs` and
/// not in `held`, counts it as running and as something held; that `stop_all` stops the
/// job as well as the run, the job's result in its listing; that a stop for a job it does
/// not hold is published in `retired_jobs`; and that the snapshot is empty only once every
/// result and the retired id are acknowledged. It catches a drain or a refusal for good that
/// leaves a job running past its wait (nobody would enforce its wall limit), and a worker
/// that winds down while it still owes the server an answer.
#[test]
fn the_worker_publishes_jobs_and_stops_them_with_the_runs() {
    let dir = Scratch::new("jobs-per-job-worker");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let mut both = job_starts(&["j1"]);
    both.commands.extend(starts(&["r1"]).commands);
    assert_eq!(worker.try_deliver(both), Delivery::Delivered);
    assert!(worker.handle().settle());
    let snapshot = worker.snapshot();
    assert_eq!(snapshot.jobs.len(), 1);
    assert_eq!(snapshot.jobs[0].job_id, "j1");
    assert_eq!(snapshot.jobs[0].state, HeldState::Running);
    assert_eq!(snapshot.held.len(), 1);
    assert_eq!(snapshot.held[0].run_id, "r1");
    assert!(snapshot.running());
    assert!(!snapshot.is_empty());

    worker.stop_all();
    assert!(worker.handle().settle());
    let log = machine.world.log();
    assert!(
        log.contains(&"stop j1".to_owned()) && log.contains(&"stop r1".to_owned()),
        "{log:?}"
    );
    let snapshot = worker.snapshot();
    assert!(!snapshot.running());
    let result = snapshot.jobs[0].result.clone().expect("the job's result");
    assert_eq!(result.outcome, Outcome::Killed);
    assert_eq!(snapshot.results.len(), 1);

    let stop = body(json!({ "commands": [{ "type": "job_stop", "job_id": "j9" }] }));
    assert_eq!(worker.try_deliver(stop), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert_eq!(worker.snapshot().retired_jobs, ["j9"]);
    let acks = body(json!({
        "result_acks": [{ "run_id": "r1", "ack": "ACCEPTED" }],
        "job_acks": [{ "job_id": "j1", "ack": "ACCEPTED" }]
    }));
    assert_eq!(worker.try_deliver(acks), Delivery::Delivered);
    assert!(worker.handle().settle());
    let snapshot = worker.snapshot();
    assert!(snapshot.jobs.is_empty() && snapshot.held.is_empty());
    assert!(!snapshot.is_empty(), "the retired id is still owed");
    let retired = body(json!({ "retired_acks": ["j9"] }));
    assert_eq!(worker.try_deliver(retired), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert!(worker.snapshot().is_empty());
}

/// Asserts that a closed (draining) worker takes no job start, and says so in a line,
/// while a job stop beside it is still answered. It catches a drain that goes on taking
/// work that comes keyed by job id.
#[test]
fn a_closed_worker_takes_no_job_start() {
    let dir = Scratch::new("jobs-per-job-closed");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    worker.close();
    let mut reply = job_starts(&["j2"]);
    reply.commands.push(Command::JobStop {
        job_id: "j3".to_owned(),
    });
    assert_eq!(worker.try_deliver(reply), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert!(machine.world.created().is_empty());
    let snapshot = worker.snapshot();
    assert!(snapshot.jobs.is_empty());
    assert_eq!(snapshot.retired_jobs, ["j3"]);
    let lines = worker.take_lines();
    assert!(
        lines.contains(&"draining: the start of job j2 is not taken".to_owned()),
        "{lines:?}"
    );
}

/// Asserts what the session says of the jobs: the hello lists no job (its held list is for
/// runs), the heartbeat lists the job and the retired id, a reply that carries only a job
/// acknowledgement and a retired acknowledgement reaches the worker, and the heartbeat
/// after it leaves both lists out. It catches a heartbeat that never carries the job
/// listing, a job listed where a lease server reads runs, and a reply of acknowledgements
/// alone that the session drops.
#[test]
fn the_heartbeat_carries_the_jobs_and_the_retired_ids() {
    let dir = Scratch::new("jobs-per-job-session");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let mut reply_in = job_starts(&["j1"]);
    reply_in.commands.push(Command::JobStop {
        job_id: "j9".to_owned(),
    });
    assert_eq!(worker.try_deliver(reply_in), Delivery::Delivered);
    assert!(worker.handle().settle());
    machine.world.exit("j1", 0);
    assert!(worker.handle().settle());

    let acks = json!({
        "commands": [],
        "job_acks": [{ "job_id": "j1", "ack": "ACCEPTED" }],
        "retired_acks": ["j9"]
    });
    let replies = vec![welcome(1), reply(200, acks), beat_ok()];
    let (mut script, sent) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    session.tick();
    session.tick();
    assert!(worker.handle().settle());
    session.tick();

    let sent = sent.borrow();
    let paths: Vec<&str> = sent.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(paths, [HELLO_PATH, HEARTBEAT_PATH, HEARTBEAT_PATH]);
    assert_eq!(sent[0].1["held"], json!([]));
    assert!(sent[0].1.get("jobs").is_none());
    let beat = &sent[1].1;
    assert_eq!(beat["held"], json!([]));
    assert_eq!(beat["results"], json!([]));
    assert_eq!(beat["jobs"][0]["job_id"], "j1");
    assert_eq!(beat["jobs"][0]["state"], "exited_unacked");
    assert_eq!(beat["jobs"][0]["result"]["outcome"], "succeeded");
    assert_eq!(beat["jobs"][0]["result"]["allocation_id"], "alloc-j1");
    assert_eq!(beat["retired_jobs"], json!(["j9"]));
    assert!(sent[2].1.get("jobs").is_none(), "{}", sent[2].1);
    assert!(sent[2].1.get("retired_jobs").is_none(), "{}", sent[2].1);
}
