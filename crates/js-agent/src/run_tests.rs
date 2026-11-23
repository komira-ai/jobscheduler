//! What a drain waits on ([`drain_waits`]).

use js_agent_wire::{HeldJob, HeldRun, HeldState, JobResult, Outcome, RunResult};

use super::*;

fn run(id: &str) -> HeldRun {
    HeldRun {
        run_id: id.to_owned(),
        state: HeldState::ExitedUnacked,
    }
}

fn job(id: &str, result: bool) -> HeldJob {
    HeldJob {
        job_id: id.to_owned(),
        state: HeldState::ExitedUnacked,
        result: result.then(|| JobResult {
            job_id: id.to_owned(),
            allocation_id: "alloc".to_owned(),
            outcome: Outcome::Succeeded,
            exit_code: Some(0),
            detail: None,
            log_tail: None,
            logs: None,
            input_failure: None,
            usage: None,
            cpu_series: None,
            memory_series: None,
        }),
    }
}

fn blocked(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

/// Proves what a drain waits on: nothing for an empty snapshot or one that holds only
/// blocked runs and jobs with no result; and a run or a job that is not blocked, a blocked
/// job whose result is not acknowledged, a result or a retired job id each make it wait.
/// Catches a drain that waits for a run the taint blocks, and one that leaves something a
/// wait can still see end.
#[test]
fn a_drain_waits_on_all_but_the_runs_and_jobs_the_taint_blocks() {
    let none = Snapshot::default();
    assert!(!drain_waits(&none));

    let only_blocked = Snapshot {
        held: vec![run("r1")],
        jobs: vec![job("j1", false)],
        blocked: blocked(&["r1", "j1"]),
        ..Snapshot::default()
    };
    assert!(!drain_waits(&only_blocked));

    let cases = [
        Snapshot {
            held: vec![run("r1"), run("r2")],
            blocked: blocked(&["r1"]),
            ..Snapshot::default()
        },
        Snapshot {
            jobs: vec![job("j2", false)],
            blocked: blocked(&["j1"]),
            ..Snapshot::default()
        },
        Snapshot {
            jobs: vec![job("j1", true)],
            blocked: blocked(&["j1"]),
            ..Snapshot::default()
        },
        Snapshot {
            results: vec![RunResult {
                run_id: "r1".to_owned(),
                lease_id: "lease".to_owned(),
                outcome: Outcome::Succeeded,
                exit_code: Some(0),
                detail: None,
                log_tail: None,
            }],
            held: vec![run("r1")],
            blocked: blocked(&["r1"]),
            ..Snapshot::default()
        },
        Snapshot {
            retired_jobs: vec!["j9".to_owned()],
            ..Snapshot::default()
        },
    ];
    for (index, snapshot) in cases.iter().enumerate() {
        assert!(drain_waits(snapshot), "case {index}: {snapshot:?}");
    }
}
