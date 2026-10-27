//! Results are held until the server acknowledges them.

use js_agent::lifecycle::Capacity;
use js_agent_wire::{HeldRun, HeldState, Outcome};
use serde_json::json;

use super::{Harness, job, reply_json};

/// Asserts that a result stays in the results and its run stays in the held list, after
/// the container and the scratch directory are gone and the slot is free, for as long as
/// no acknowledgement comes: across steps and across replies that acknowledge other
/// runs. It catches a result dropped when it is sent once, which the server may never
/// have received.
#[test]
fn unacked_result_stays_in_held() {
    let mut h = Harness::new("unacked");
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(h.life.free().cpu_millis, 3000, "r1's slot is free");
    for _ in 0..3 {
        h.life.step();
        h.ack("r2", "ACCEPTED");
    }
    assert_eq!(h.life.results().len(), 1);
    assert_eq!(h.life.results()[0].run_id, "r1");
    assert_eq!(h.life.results()[0].lease_id, "lease-r1");
    assert_eq!(
        h.life.held(),
        [
            HeldRun {
                run_id: "r1".to_owned(),
                state: HeldState::ExitedUnacked
            },
            HeldRun {
                run_id: "r2".to_owned(),
                state: HeldState::Running
            },
        ]
    );
}

/// Asserts that every verdict the agent knows ends the wait for a result, that a verdict
/// it does not know keeps the result to be sent again, and that an acknowledgement for a
/// run the agent does not hold is ignored. It catches a result dropped on an unknown
/// verdict (the server never recorded it) and one resent for ever after `STALE`.
#[test]
fn every_known_verdict_drops_the_result_and_an_unknown_one_keeps_it() {
    let mut h = Harness::new("verdicts");
    let verdicts = [
        "ACCEPTED",
        "DUPLICATE",
        "STALE",
        "UNKNOWN_LEASE",
        "MISMATCH",
    ];
    let runs: Vec<String> = (0..=verdicts.len()).map(|n| format!("r{n}")).collect();
    for run in &runs {
        let mut spec = job(run);
        spec.limits.cpu_millis = 100;
        spec.limits.memory_bytes = 1 << 20;
        h.start(spec);
        h.fake.exit(run, 0);
    }
    h.life.step();
    assert_eq!(h.life.results().len(), runs.len());
    h.ack("stranger", "ACCEPTED");
    h.ack("r5", "A_VERDICT_FROM_THE_FUTURE");
    assert_eq!(
        h.life.results().len(),
        runs.len(),
        "unknown verdicts keep it"
    );
    for (run, verdict) in runs.iter().zip(verdicts) {
        h.ack(run, verdict);
    }
    assert_eq!(h.held(), ["r5"]);
    h.ack("r5", "ACCEPTED");
    assert!(h.held().is_empty());
}

/// Asserts that acknowledgements that do not validate (the same run twice) are all
/// ignored while the commands beside them are still acted on. It catches an agent that
/// trusts a malformed acknowledgement list.
#[test]
fn malformed_acknowledgements_are_ignored() {
    let mut h = Harness::new("bad-acks");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    let reply = reply_json(&json!({
        "commands": [{ "type": "start", "run_id": "r2", "lease_id": "l2", "image": "img:1",
                       "limits": { "cpu_millis": 100, "memory_bytes": 1048576, "wall_ms": 1000 } }],
        "result_acks": [
            { "run_id": "r1", "ack": "ACCEPTED" },
            { "run_id": "r1", "ack": "ACCEPTED" }
        ]
    }));
    h.life.apply(&reply);
    assert_eq!(
        h.life.results()[0].outcome,
        Outcome::Succeeded,
        "not dropped"
    );
    assert_eq!(h.held(), ["r1", "r2"]);
}

/// Asserts that the capacity of a hello is its shape and the indices of the GPUs it
/// lists. It catches a booking against numbers other than the ones the server was told.
#[test]
fn capacity_is_what_the_hello_offers() {
    let mut hello = crate::support::hello("h-1");
    hello.shape.cpu_millis = 6000;
    hello.shape.memory_bytes = 1 << 33;
    hello.gpu_devices = [3, 5]
        .map(|index| js_agent_wire::GpuDevice {
            index,
            uuid: format!("GPU-{index}"),
            model: "m".to_owned(),
            memory_bytes: 1,
        })
        .to_vec();
    let capacity = Capacity::of(&hello);
    assert_eq!(capacity.cpu_millis, 6000);
    assert_eq!(capacity.memory_bytes, 1 << 33);
    assert_eq!(capacity.gpus.into_iter().collect::<Vec<_>>(), [3, 5]);
}
