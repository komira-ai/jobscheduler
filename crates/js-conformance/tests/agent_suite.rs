//! Runs the host-agent suite on the correct fake agent and on each broken one.

use js_conformance::Verdict;
use js_conformance::agent::{
    self, AgentHarness, Answer, Defect, Delivery, FENCE_WINDOW, FakeAgent, Flaw, HostCheck,
    Scenario,
};

#[test]
fn the_correct_agent_passes_every_scenario() {
    let report = agent::run(FakeAgent::new);
    assert_eq!(report.results.len(), Scenario::ALL.len());
    assert!(report.results.iter().all(|(_, v)| *v == Verdict::Passed));
    assert!(report.failed().is_empty());
}

/// Each broken agent fails the scenario for its missing behaviour and no other, so a
/// scenario cannot pass without reading that behaviour.
fn only_fails(defect: Defect, scenario: Scenario) {
    let report = agent::run(|| FakeAgent::broken(defect));
    assert_eq!(report.failed(), vec![scenario], "{defect:?}");
}

#[test]
fn a_repeated_start_that_runs_twice_fails_start_is_idempotent() {
    only_fails(Defect::StartRunsTwice, Scenario::StartIsIdempotent);
}

#[test]
fn a_cancel_that_keeps_the_lease_listed_fails_cancel_until_absent() {
    only_fails(Defect::CancelKeepsListed, Scenario::CancelUntilAbsent);
}

#[test]
fn forgetting_retired_ids_fails_cancel_until_absent() {
    only_fails(Defect::ForgetsRetiredIds, Scenario::CancelUntilAbsent);
}

#[test]
fn an_agent_with_no_self_fence_fails_only_the_fence_scenario_and_says_why() {
    let report = agent::run(|| FakeAgent::with_window(None));
    assert_eq!(report.failed(), vec![Scenario::SelfFencesAfterSilence]);
    let reason = report.results.iter().find_map(|(_, v)| match v {
        Verdict::Failed(text) => Some(text.clone()),
        _ => None,
    });
    assert!(reason.unwrap().contains("no self-fence"));
}

#[test]
fn the_fence_window_is_the_agents_own() {
    let report = agent::run(|| FakeAgent::with_window(Some(2)));
    assert!(report.failed().is_empty());
}

#[test]
fn running_a_late_start_fails_late_start_is_dropped() {
    only_fails(Defect::RunsLateStart, Scenario::LateStartIsDropped);
}

#[test]
fn never_fencing_fails_self_fences_after_silence() {
    only_fails(Defect::NeverFences, Scenario::SelfFencesAfterSilence);
}

#[test]
fn forgetting_an_unacked_result_fails_unacked_result_is_resent() {
    only_fails(
        Defect::ForgetsUnackedResult,
        Scenario::UnackedResultIsResent,
    );
}

#[test]
fn running_a_retired_fence_fails_retired_fence_is_refused() {
    only_fails(Defect::RunsRetiredFence, Scenario::RetiredFenceIsRefused);
}

#[test]
fn a_fence_kept_by_the_job_alone_fails_retired_fence_is_refused() {
    only_fails(
        Defect::FenceKeyedByJobAlone,
        Scenario::RetiredFenceIsRefused,
    );
}

#[test]
fn the_failure_text_names_what_was_wrong() {
    let report = agent::run(|| FakeAgent::broken(Defect::NeverFences));
    let text = report.results.iter().find_map(|(_, v)| match v {
        Verdict::Failed(text) => Some(text.clone()),
        _ => None,
    });
    assert!(text.unwrap().contains("reported fenced"));
}

/// The fake itself: the operations the scenarios do not reach on their own.
#[test]
fn messages_on_a_dropped_connection_are_lost_and_change_nothing() {
    let mut agent = FakeAgent::default();
    assert_eq!(agent.fence_window(), Some(FENCE_WINDOW));
    agent.disconnect();
    assert_eq!(agent.start("a", 10, 0), Delivery::Lost);
    assert_eq!(agent.ack("a"), Delivery::Lost);
    assert_eq!(agent.start_fenced("a", "first"), Delivery::Lost);
    assert_eq!(agent.cancel("a"), Delivery::Lost);
    assert_eq!(agent.answer("a", Answer::Stale), Delivery::Lost);
    assert_eq!(agent.start_flawed("a", Flaw::NoWallTime), Delivery::Lost);
    assert_eq!(agent.start_host("a"), Delivery::Lost);
    assert_eq!(agent.ack_taint(), Delivery::Lost);
    assert_eq!(agent.hold_whole("w"), Delivery::Lost);
    assert_eq!(agent.start_whole("a"), Delivery::Lost);
    assert!(!agent.whole_ready(), "no lease holds the host");
    assert_eq!(agent.observed(), Default::default());
}

#[test]
fn a_result_finished_while_disconnected_is_sent_on_reconnect() {
    let mut agent = FakeAgent::new();
    agent.start("a", 10, 0);
    agent.disconnect();
    agent.complete("a");
    assert!(agent.observed().results_sent.is_empty());
    agent.reconnect();
    assert_eq!(agent.observed().results_sent, ["a"]);
    assert_eq!(agent.now(), 0);
}

/// Asserts that a host no lease holds whole is never reported ready, quiet as it is, and
/// that a restart reconnects the agent, whose first heartbeat repeats what it holds. It
/// catches a readiness answer given without a hold, and a restart that stays offline.
#[test]
fn readiness_needs_a_hold_and_a_restart_reconnects() {
    let mut agent = FakeAgent::new();
    assert!(!agent.whole_ready());
    agent.start("a", 0, 0);
    agent.complete("a");
    agent.disconnect();
    agent.restart();
    assert_eq!(agent.observed().results_sent, ["a", "a"]);
    assert_eq!(agent.hold_whole("w"), Delivery::Delivered);
}

#[test]
fn keeping_a_result_answered_stale_fails_answered_result_is_dropped() {
    only_fails(
        Defect::KeepsRefusedResult,
        Scenario::AnsweredResultIsDropped,
    );
}

#[test]
fn dropping_a_result_on_an_unknown_verdict_fails_answered_result_is_dropped() {
    only_fails(
        Defect::DropsUnrecognisedAnswer,
        Scenario::AnsweredResultIsDropped,
    );
}

#[test]
fn no_usage_on_failure_fails_usage_is_reported_on_every_end() {
    only_fails(
        Defect::DropsUsageOnFailure,
        Scenario::UsageIsReportedOnEveryEnd,
    );
}

#[test]
fn no_usage_on_kill_fails_usage_is_reported_on_every_end() {
    only_fails(
        Defect::DropsUsageOnKill,
        Scenario::UsageIsReportedOnEveryEnd,
    );
}

/// Each rule of the start contract on its own: an agent that launches a start breaking that
/// one rule fails `FlawedStartIsRefused`, and its verdict names the flaw.
#[test]
fn running_a_flawed_start_fails_flawed_start_is_refused_for_each_flaw() {
    for flaw in Flaw::ALL {
        let report = agent::run(|| FakeAgent::broken(Defect::RunsFlawedStart(flaw)));
        assert_eq!(
            report.failed(),
            vec![Scenario::FlawedStartIsRefused],
            "{flaw:?}"
        );
        let text = failure_of(&report, Scenario::FlawedStartIsRefused);
        let named = format!("a start with {flaw:?} launches nothing");
        assert!(text.contains(&named), "{flaw:?}: {text}");
    }
}

#[test]
fn freeing_the_slot_first_fails_cleanup_frees_the_slot_last() {
    only_fails(
        Defect::FreesSlotBeforeCleanup,
        Scenario::CleanupFreesTheSlotLast,
    );
}

#[test]
fn freeing_the_slot_of_a_stuck_container_fails_cleanup_frees_the_slot_last() {
    only_fails(
        Defect::FreesSlotOnFailedRemoval,
        Scenario::CleanupFreesTheSlotLast,
    );
}

#[test]
fn a_failed_removal_that_taints_nothing_fails_cleanup_frees_the_slot_last() {
    only_fails(
        Defect::FailedRemovalTaintsNothing,
        Scenario::CleanupFreesTheSlotLast,
    );
}

#[test]
fn running_a_host_job_without_consent_fails_host_job_needs_consent() {
    only_fails(
        Defect::RunsHostWithoutConsent,
        Scenario::HostJobNeedsConsent,
    );
}

/// The host job taints whatever its end: the verdict of an agent that taints only after a
/// job that passed names the failed and the cancelled ends, and not the one that passed.
#[test]
fn tainting_only_on_success_fails_host_job_taints_whatever_its_end() {
    only_fails(
        Defect::HostJobTaintsOnlyOnSuccess,
        Scenario::HostJobTaintsWhateverItsEnd,
    );
    let report = agent::run(|| FakeAgent::broken(Defect::HostJobTaintsOnlyOnSuccess));
    let text = failure_of(&report, Scenario::HostJobTaintsWhateverItsEnd);
    assert_eq!(
        text,
        "a host job that Fails leaves the host tainted; \
         a host job that IsCancelled leaves the host tainted"
    );
}

#[test]
fn a_leftover_process_that_taints_nothing_fails_tainting_check_taints_on_its_own() {
    only_fails(
        Defect::LeftoverProcessTaintsNothing,
        Scenario::TaintingCheckTaintsOnItsOwn,
    );
}

#[test]
fn a_check_that_could_not_run_passing_fails_tainting_check_taints_on_its_own() {
    only_fails(
        Defect::UnrunCheckPasses,
        Scenario::TaintingCheckTaintsOnItsOwn,
    );
}

#[test]
fn forgetting_an_unacked_taint_fails_taint_is_kept() {
    only_fails(Defect::ForgetsUnackedTaint, Scenario::TaintIsKept);
}

#[test]
fn a_taint_cleared_by_its_ack_fails_taint_is_kept() {
    only_fails(Defect::AckClearsTaint, Scenario::TaintIsKept);
}

#[test]
fn forgetting_the_taint_at_a_restart_fails_taint_is_kept() {
    only_fails(Defect::ForgetsTaintAtRestart, Scenario::TaintIsKept);
}

#[test]
fn starting_on_a_tainted_host_fails_taint_is_kept() {
    only_fails(Defect::StartsOnTaintedHost, Scenario::TaintIsKept);
}

#[test]
fn forgetting_runs_at_a_restart_fails_restart_keeps_what_it_holds() {
    only_fails(
        Defect::ForgetsRunsAtRestart,
        Scenario::RestartKeepsWhatItHolds,
    );
}

#[test]
fn forgetting_results_at_a_restart_fails_restart_keeps_what_it_holds() {
    only_fails(
        Defect::ForgetsResultsAtRestart,
        Scenario::RestartKeepsWhatItHolds,
    );
}

#[test]
fn ready_while_busy_fails_whole_node_waits_for_quiet() {
    only_fails(Defect::ReadyWhileBusy, Scenario::WholeNodeWaitsForQuiet);
}

#[test]
fn running_beside_a_whole_node_lease_fails_whole_node_waits_for_quiet() {
    only_fails(
        Defect::RunsBesideWholeLease,
        Scenario::WholeNodeWaitsForQuiet,
    );
}

/// Asserts the check findings the fake reads: a run with no finding frees its slot, and a
/// check that finds a leftover process keeps it and records the finding for the run. It
/// catches a fake that reads the finding of another run.
#[test]
fn a_check_finding_holds_only_its_own_run() {
    let mut agent = FakeAgent::new();
    agent.start("clean", 0, 0);
    agent.start("dirty", 0, 0);
    agent.check_finds("dirty", HostCheck::LeftoverProcess);
    agent.complete("clean");
    agent.complete("dirty");
    let seen = agent.observed();
    assert_eq!(seen.findings.into_iter().collect::<Vec<_>>(), ["dirty"]);
    assert!(seen.tainted);
    let freed: Vec<&str> = seen
        .cleanup
        .iter()
        .filter(|(_, step)| *step == agent::Cleanup::SlotFreed)
        .map(|(run, _)| run.as_str())
        .collect();
    assert_eq!(freed, ["clean"]);
}

/// The text of the failed verdict of `scenario` in `report`.
fn failure_of(report: &agent::AgentReport, scenario: Scenario) -> String {
    let verdict = report.results.iter().find(|(s, _)| *s == scenario);
    match verdict {
        Some((_, Verdict::Failed(text))) => text.clone(),
        other => panic!("{scenario:?} did not fail: {other:?}"),
    }
}
