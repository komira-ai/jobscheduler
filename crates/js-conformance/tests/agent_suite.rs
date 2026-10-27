//! Runs the host-agent suite on the correct fake agent and on each broken one.

use js_conformance::Verdict;
use js_conformance::agent::{
    self, AgentHarness, Defect, Delivery, FENCE_WINDOW, FakeAgent, Scenario,
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
    assert_eq!(agent.cancel("a"), Delivery::Lost);
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
