//! Runs the core-driver suite on the reference core and on each broken one.

use std::time::Duration;

use js_conformance::Verdict;
use js_conformance::core::{self, CoreHarness, Defect, ReferenceCore, Scenario, UNPLACEABLE_JOB};

#[test]
fn the_reference_core_passes_every_scenario() {
    let report = core::run(ReferenceCore::new);
    assert_eq!(report.results.len(), Scenario::ALL.len());
    assert!(report.results.iter().all(|(_, v)| *v == Verdict::Passed));
    assert!(report.failed().is_empty());
}

/// Each broken core fails the scenario for its broken rule and no other, so a scenario
/// cannot pass without reading that rule.
fn only_fails(defect: Defect, scenario: Scenario) {
    let report = core::run(|| ReferenceCore::broken(defect));
    assert_eq!(report.failed(), vec![scenario], "{defect:?}");
}

#[test]
fn a_start_released_before_its_commit_fails_start_waits_for_commit() {
    only_fails(Defect::StartBeforeCommit, Scenario::StartWaitsForCommit);
}

#[test]
fn a_finished_operation_kept_in_the_store_fails_answered_once_across_restart() {
    only_fails(Defect::AnswerTwice, Scenario::AnsweredOnceAcrossRestart);
}

#[test]
fn numbering_from_the_surviving_leases_fails_lease_ids_never_repeat() {
    only_fails(Defect::ReusesSeqAfterRestart, Scenario::LeaseIdsNeverRepeat);
}

#[test]
fn feeding_a_replaced_streams_heartbeat_fails_stale_heartbeat_is_not_fed() {
    only_fails(
        Defect::FeedsStaleHeartbeat,
        Scenario::StaleHeartbeatIsNotFed,
    );
}

#[test]
fn forgetting_a_refused_write_fails_refused_write_is_retried_in_order() {
    only_fails(
        Defect::DropsUnwrittenOnStoreError,
        Scenario::RefusedWriteIsRetriedInOrder,
    );
}

#[test]
fn recovering_without_the_leases_fails_recovered_lease_waits_for_its_worker() {
    only_fails(
        Defect::RecoversWithoutGrace,
        Scenario::RecoveredLeaseWaitsForItsWorker,
    );
}

#[test]
fn forgetting_the_queue_when_no_worker_had_registered_fails_queued_at_crash_runs_once() {
    only_fails(
        Defect::ForgetsQueueWithoutWorkers,
        Scenario::QueuedAtCrashRunsOnce,
    );
}

#[test]
fn the_failure_text_names_what_was_wrong() {
    let report = core::run(|| ReferenceCore::broken(Defect::StartBeforeCommit));
    let text = report.results.iter().find_map(|(_, v)| match v {
        Verdict::Failed(text) => Some(text.clone()),
        _ => None,
    });
    assert!(text.unwrap().contains("refuses its lease record"));
}

/// The model itself: a refusal is written to the store and answered, and the operation
/// is gone from the store after it.
#[test]
fn the_reference_core_answers_a_refusal_after_writing_it() {
    let mut core = ReferenceCore::default();
    core.worker_up("node-a");
    let operation = core.submit(9, UNPLACEABLE_JOB);
    core.advance(Duration::from_secs(1));
    assert!(core.observed().answered.is_empty());
    core.advance(Duration::from_secs(301));
    core.advance(Duration::from_secs(1));
    let answered = core.observed().answered;
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].operation, operation);
    core.crash_and_restart();
    core.advance(Duration::from_secs(1));
    assert_eq!(
        core.observed().answered,
        answered,
        "refused work is not taken up again"
    );
}
