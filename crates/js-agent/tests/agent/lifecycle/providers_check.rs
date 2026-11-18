//! The check of a run's provider sources after a job that did not succeed has ended
//! (`docs/design/mount-providers.md`): the result waits until each source is answered, a
//! source lost for `tree-lost` makes the result's `input_failure` `tree-lost` (the server runs
//! the job again), another cause makes it `input-unreadable`, a check that fails is read as
//! not lost, and a start of a run whose earlier release is still owed waits for it.

use js_agent::lifecycle::{Event, ProviderAnswer};
use js_agent_wire::{HeldState, Outcome, refusal};
use serde_json::json;

use super::fake_providers::{
    WAIT_MS, check, release, rig, run_one, tree, with_layer, with_layer_and_mount,
};
use super::per_job::{job_start, listing, start};
use super::{Harness, names, reply_json};

/// The messages of the findings of `code` among `events`, in order.
fn findings(events: &[Event], code: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Finding(found) if found.code == code => Some(found.message.clone()),
            _ => None,
        })
        .collect()
}

/// Asserts that a job that failed and whose provider says it lost the source for `tree-lost`
/// ends `failed` with its exit code kept and `input_failure` `tree-lost`, and that the
/// provider's cause is reported in a `provider-source-lost` finding. Catches a lost tree
/// reported as the job's own failure, which the server would not run again.
#[test]
fn a_failed_job_whose_tree_was_lost_reports_tree_lost() {
    let (mut h, providers) = rig("check-tree-lost", |_| {});
    run_one(&mut h, &providers, "j1");
    providers.loses("tier", "j1", 0, "tree-lost");
    h.fake.exit("j1", 5);
    let events = h.life.step();
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::Failed);
    assert_eq!(result.exit_code, Some(5));
    assert_eq!(result.input_failure.as_deref(), Some(refusal::TREE_LOST));
    assert_eq!(
        findings(&events, "provider-source-lost"),
        ["tier source 0: lost while the run used it: tree-lost"]
    );
    assert!(findings(&events, "provider-check-failed").is_empty());
}

/// Asserts that a job whose provider says it lost the source for a cause other than
/// `tree-lost` ends with `input_failure` `input-unreadable`, and that the provider's own text
/// is in the `provider-source-lost` finding. Catches an unknown cause passed on as a code the
/// server reads as a retry.
#[test]
fn another_lost_cause_reports_input_unreadable_and_a_finding() {
    let (mut h, providers) = rig("check-other-cause", |_| {});
    run_one(&mut h, &providers, "j1");
    providers.loses("tier", "j1", 0, "disk-gone");
    h.fake.exit("j1", 1);
    let events = h.life.step();
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(
        result.input_failure.as_deref(),
        Some(refusal::INPUT_UNREADABLE)
    );
    assert_eq!(
        findings(&events, "provider-source-lost"),
        ["tier source 0: lost while the run used it: disk-gone"]
    );
}

/// Asserts that a run with two lost sources, the first for another cause and the second for
/// `tree-lost`, reports `tree-lost`, and that each source is asked once, in order of the
/// sources. Catches a run that is not run again because an earlier source was lost for a
/// lesser cause.
#[test]
fn a_lost_tree_is_reported_before_another_cause_of_an_earlier_source() {
    let (mut h, providers) = rig("check-two-lost", |_| {});
    providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "t1"));
    providers.mounts_at("tier", "j1", 1, &tree(&h, "ro", "t2"));
    providers.loses("tier", "j1", 0, "disk-gone");
    providers.loses("tier", "j1", 1, "tree-lost");
    h.apply(vec![job_start(with_layer_and_mount("j1"))]);
    for _ in 0..4 {
        h.life.step();
    }
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
    h.fake.exit("j1", 1);
    h.life.step();
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.input_failure.as_deref(), Some(refusal::TREE_LOST));
    assert_eq!(
        providers.sent("check"),
        ["provider check tier j1#0", "provider check tier j1#1"]
    );
}

/// Asserts that a check the provider refuses or cannot answer gives no input failure (the
/// source is read as not lost), builds the result at once, and makes one
/// `provider-check-failed` finding for each source with the provider's reason. Catches a
/// provider that is down holding a result back, and a failed check read as a lost tree.
#[test]
fn a_check_that_fails_gives_no_input_failure_and_a_finding() {
    let (mut h, providers) = rig("check-fails", |_| {});
    providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "t1"));
    providers.mounts_at("tier", "j1", 1, &tree(&h, "ro", "t2"));
    let refused = ProviderAnswer::Refused("no such run".to_owned());
    providers.always("tier", &check("j1", 0), refused);
    let down = ProviderAnswer::Unavailable("unimplemented".to_owned());
    providers.always("tier", &check("j1", 1), down);
    h.apply(vec![job_start(with_layer_and_mount("j1"))]);
    for _ in 0..4 {
        h.life.step();
    }
    h.fake.exit("j1", 1);
    let events = h.life.step();
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::Failed);
    assert_eq!(result.input_failure, None);
    assert!(findings(&events, "provider-source-lost").is_empty());
    assert_eq!(
        findings(&events, "provider-check-failed"),
        [
            "tier source 0: not checked, read as not lost: no such run",
            "tier source 1: not checked, read as not lost: unimplemented"
        ]
    );
    assert!(findings(&h.life.step(), "provider-check-failed").is_empty());
}

/// Asserts that a job that succeeded is never checked, whatever its provider would say, and
/// that a job that failed without a provider source is never checked either. Catches a check
/// sent for every job, which costs a call for each of the jobs that run.
#[test]
fn a_succeeded_job_and_a_job_without_provider_sources_are_not_checked() {
    let (mut h, providers) = rig("check-not-asked", |_| {});
    run_one(&mut h, &providers, "j1");
    providers.loses("tier", "j1", 0, "tree-lost");
    h.fake.exit("j1", 0);
    h.life.step();
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::Succeeded);
    assert_eq!(result.input_failure, None);
    h.apply(vec![job_start(start("j2"))]);
    for _ in 0..4 {
        h.life.step();
    }
    assert_eq!(listing(&h, "j2").state, HeldState::Running);
    h.fake.exit("j2", 3);
    h.life.step();
    let result = listing(&h, "j2").result.expect("a result");
    assert_eq!(result.outcome, Outcome::Failed);
    assert_eq!(result.input_failure, None);
    assert!(providers.sent("check").is_empty());
}

/// Asserts that while a check is not answered the job's result is not built (no `result`
/// event, no result in its listing, the container not removed), the check is sent once, and
/// that the next step after the answer builds the result with the provider's verdict. Catches
/// a result sent before the provider said whether it lost the tree.
#[test]
fn the_result_waits_while_a_check_is_pending() {
    let (mut h, providers) = rig("check-pending", |_| {});
    run_one(&mut h, &providers, "j1");
    providers.then("tier", &check("j1", 0), ProviderAnswer::Pending);
    providers.then("tier", &check("j1", 0), ProviderAnswer::Pending);
    providers.loses("tier", "j1", 0, "tree-lost");
    h.fake.exit("j1", 1);
    for _ in 0..2 {
        let events = names(&h.life.step());
        assert!(!events.contains(&"result j1".to_owned()), "{events:?}");
        assert!(!events.contains(&"container removed j1".to_owned()));
        assert_eq!(listing(&h, "j1").result, None);
        assert_eq!(listing(&h, "j1").state, HeldState::Running);
    }
    assert_eq!(
        providers.sent("check").len(),
        1,
        "the call out is not repeated"
    );
    let events = names(&h.life.step());
    assert!(events.contains(&"result j1".to_owned()), "{events:?}");
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.input_failure.as_deref(), Some(refusal::TREE_LOST));
    assert_eq!(providers.sent("check").len(), 1, "asked once in all");
}

/// Asserts that a source is asked `Check` before it is released, and that the release still
/// comes once with the cleanup. Catches a release that gives the tree up before the provider
/// is asked whether it lost it.
#[test]
fn a_source_is_checked_before_it_is_released() {
    let (mut h, providers) = rig("check-before-release", |_| {});
    run_one(&mut h, &providers, "j1");
    h.fake.exit("j1", 1);
    for _ in 0..4 {
        h.life.step();
    }
    let log = h.log.borrow().clone();
    let checked = log
        .iter()
        .position(|line| line == "provider check tier j1#0");
    let released = log
        .iter()
        .position(|line| line == "provider release tier j1#0");
    assert!(checked.is_some() && checked < released, "{log:?}");
    assert_eq!(providers.sent("release").len(), 1);
}

/// Acknowledges the result of `job_id` as accepted, so the job is no longer held.
fn accept(h: &mut Harness, job_id: &str) {
    let reply = reply_json(&json!({ "job_acks": [{ "job_id": job_id, "ack": "ACCEPTED" }] }));
    h.life.apply(&reply);
}

/// Ends `j1` (run with one provider source), holds its release unanswered, and acknowledges
/// its result, so a start of `j1` with a new allocation can come while the release is owed.
fn ended_with_release_owed(h: &mut Harness, providers: &super::fake_providers::FakeProviders) {
    run_one(h, providers, "j1");
    providers.always("tier", &release("j1", 0), ProviderAnswer::Pending);
    h.fake.exit("j1", 1);
    for _ in 0..4 {
        h.life.step();
    }
    accept(h, "j1");
    assert!(h.life.held_jobs().is_empty());
}

/// The start of `j1` again, on a new allocation, as a job queued again is placed.
fn start_again(h: &mut Harness) {
    let mut spec = with_layer("j1");
    spec.allocation_id = "alloc-j1-again".to_owned();
    h.apply(vec![job_start(spec)]);
}

/// Asserts that a start of a run whose release is still owed does not ask `Prepare` while
/// the release is unanswered, and goes on (`Prepare`, `Mount`, the container) in the step
/// after the release is answered. Catches a release that comes after the new run's `Prepare`
/// and gives up the new run's source.
#[test]
fn a_start_waits_for_the_release_its_run_owes() {
    let (mut h, providers) = rig("check-owed-wait", |_| {});
    ended_with_release_owed(&mut h, &providers);
    assert_eq!(providers.sent("prepare").len(), 1);
    start_again(&mut h);
    h.life.step();
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert_eq!(providers.sent("prepare").len(), 1, "no Prepare while owed");
    providers.always("tier", &release("j1", 0), ProviderAnswer::Done(None));
    providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "again"));
    for _ in 0..3 {
        h.life.step();
    }
    assert_eq!(providers.sent("prepare").len(), 2);
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
    let record = h.dir.path("state/providers/j1.json");
    assert!(record.is_file(), "the new run's record was removed");
}

/// Asserts that a release that is never answered stops holding the start when it is given up
/// a minute after the run ended: the start, admitted half a minute after it, asks `Prepare`
/// then and runs. Catches a start held for ever by a provider that does not answer a release.
#[test]
fn a_start_goes_on_when_the_owed_release_is_given_up() {
    let (mut h, providers) = rig("check-owed-give-up", |_| {});
    ended_with_release_owed(&mut h, &providers);
    h.clock.advance(WAIT_MS + 1_000);
    start_again(&mut h);
    h.life.step();
    assert_eq!(providers.sent("prepare").len(), 1, "no Prepare while owed");
    providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "again"));
    h.clock.advance(29_000);
    let events: Vec<Event> = (0..3).flat_map(|_| h.life.step()).collect();
    assert_eq!(providers.sent("prepare").len(), 2);
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
    assert_eq!(findings(&events, "provider-release-failed").len(), 1);
    let record = h.dir.path("state/providers/j1.json");
    assert!(record.is_file(), "the new run's record was removed");
}
