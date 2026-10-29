//! The order of a machine's last calls: its join token is revoked before it is terminated,
//! whichever way it came to be released.

mod common;

use common::*;
use js_elastic::{Acquired, HostStatus};
use js_scaler::{Action, Mark};

/// Asserts that in `history`, the actions of a run in order, every `Release` of a host
/// has a `Revoke` of the same host before it, and that there is a release to check.
fn revoked_before_released(history: &[Action]) {
    let mut released = 0;
    for (index, action) in history.iter().enumerate() {
        if let Action::Release { host, .. } = action {
            released += 1;
            let revoked = history[..index]
                .iter()
                .any(|each| *each == Action::Revoke(host.clone()));
            assert!(revoked, "{host} was released before its token was revoked");
        }
    }
    assert!(released > 0, "no release in {history:?}");
}

/// Asserts the order for a machine released at the end of a quiet life: drained, marked
/// releasing by the registry, then released.
#[test]
fn a_released_host_is_revoked_first() {
    let (mut rig, _plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    let mut history = Vec::new();
    history.extend(rig.tick(at(2 * SECOND + 10 * MINUTE)));
    rig.input.hosts[0].mark = Some(Mark::Draining);
    history.extend(rig.tick(at(2 * SECOND + 11 * MINUTE)));
    rig.input.hosts[0].mark = Some(Mark::Releasing);
    for step in 0..3 {
        history.extend(rig.tick(at(2 * SECOND + 12 * MINUTE + step * SECOND)));
    }
    revoked_before_released(&history);
}

/// Asserts the order for a machine that never said hello and was given up on at
/// `boot_timeout`.
#[test]
fn a_boot_that_timed_out_is_revoked_first() {
    let (mut rig, _plan, _) = booting(settings("gpu"), "i-1");
    let mut history = Vec::new();
    for step in 0..3 {
        history.extend(rig.tick(at(15 * MINUTE + step * SECOND)));
    }
    revoked_before_released(&history);
}

/// Asserts the order for a machine the provider lists as terminated, from every phase it
/// can be in that holds a token.
#[test]
fn a_machine_the_provider_lost_is_revoked_first() {
    let (mut rig, plan, booted) = booting(settings("gpu"), "i-1");
    rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Terminated)]);
    let history = rig.tick(at(booted + SECOND));
    revoked_before_released(&history);

    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Preempted)]);
    let history = rig.tick(at(3 * SECOND));
    revoked_before_released(&history);
}

/// Asserts the order for an acquire that failed and whose machine turned up later, the
/// token having been revoked when it failed and not again: once when the list shows the
/// machine, once when the late answer names it. It catches a release of a failed acquire
/// that skips the revoke because the machine "never existed".
#[test]
fn a_machine_found_after_its_acquire_failed_is_revoked_first() {
    for late_answer in [false, true] {
        let mut rig = Rig::new(settings("gpu"));
        rig.input.waiting = vec![request("r1")];
        let mut history = Vec::new();
        let first = rig.tick(at(0));
        let plan = acquires(&first).remove(0);
        history.extend(first);
        history.extend(rig.tick(at(30 * MINUTE)));
        if late_answer {
            rig.input.acquired = vec![accepted(&plan, "i-late")];
        } else {
            let held = listed(&plan, "i-late", HostStatus::Active);
            rig.pool().held = Some(vec![held]);
        }
        history.extend(rig.tick(at(31 * MINUTE)));
        revoked_before_released(&history);
    }
}

/// Asserts that an acquire the provider refused is revoked and forgotten and never
/// released: no machine exists, so there is nothing to terminate.
#[test]
fn a_refused_acquire_is_revoked_and_never_released() {
    let mut rig = Rig::new(settings("gpu"));
    rig.input.waiting = vec![request("r1")];
    let plan = acquires(&rig.tick(at(0))).remove(0);
    let refusal = js_elastic::Refusal {
        code: js_elastic::RefusalCode::Denied,
        reason: js_elastic::Reason::DryRun,
    };
    rig.input.acquired = vec![answered(&plan, Acquired::Refused(refusal))];
    let actions = rig.tick(at(SECOND));
    assert_eq!(count(&actions, "Revoke"), 1);
    assert_eq!(count(&actions, "Release"), 0);
}
