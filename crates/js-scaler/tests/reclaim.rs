//! A machine the provider says it reclaimed, by a notice of its own (`Input::reclaimed`)
//! rather than in a list: it takes the route of a machine a list shows preempted, whatever
//! the list says, and is released and counted once however many times it is named.

mod common;

use common::*;
use js_elastic::HostStatus;
use js_scaler::{Blocked, Mark, Phase};

/// The count of the pool's failure brake, zero when it has none.
fn failures(rig: &Rig) -> u32 {
    rig.state.records.brake.get("gpu").map_or(0, |b| b.failures)
}

/// Asserts that a registered machine named by a notice while the list still shows it
/// running is revoked and released on that step, strikes the brake once and starts no
/// cooldown; and that a later step, whose list now shows it preempted and which carries
/// the notice again, only repeats the release and does not strike again. It catches a
/// notice the step ignores (the machine would stay registered until the list caught up),
/// a notice that waits for the list, and a notice and a list that count one reclaim twice.
#[test]
fn a_reclaimed_registered_machine_is_released_at_once_and_counted_once() {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Active)]);
    rig.input.reclaimed = vec!["i-1".to_owned()];
    let actions = rig.tick(at(3 * SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert_eq!(failures(&rig), 1);
    assert!(rig.state.records.cooldowns.is_empty());

    rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Preempted)]);
    rig.input.reclaimed = vec!["i-1".to_owned()];
    let actions = rig.tick(at(4 * SECOND));
    assert_eq!(kinds(&actions), ["Release"]);
    assert_eq!(failures(&rig), 1);
}

/// Asserts that a notice is acted on when the list could not be had: the step reads no
/// list (`held` is `None`) and still releases the machine the notice names. It catches a
/// notice read only alongside a list.
#[test]
fn a_notice_is_acted_on_when_the_list_failed() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.pool().held = None;
    rig.input.reclaimed = vec!["i-1".to_owned()];
    let actions = rig.tick(at(3 * SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert_eq!(failures(&rig), 1);
}

/// Asserts the route of the two other held phases: a booting machine named by a notice is
/// released and starts the boot cooldown (as a booting machine the list shows dead does),
/// and a draining one is released and strikes the brake. It catches a notice that only
/// handles registered machines.
#[test]
fn a_reclaimed_booting_or_draining_machine_takes_the_list_route() {
    let (mut booting_rig, plan, booted) = booting(settings("gpu"), "i-1");
    booting_rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Booting)]);
    booting_rig.input.reclaimed = vec!["i-1".to_owned()];
    let actions = booting_rig.tick(at(booted + SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(booting_rig.phase(0), Phase::Releasing);
    assert_eq!(booting_rig.state.blocked["gpu"], Blocked::BootFailed);

    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    let now = 2 * SECOND + 10 * MINUTE;
    rig.tick(at(now));
    rig.input.hosts[0].mark = Some(Mark::Draining);
    rig.tick(at(now + SECOND));
    assert_eq!(rig.phase(0), Phase::Draining);
    rig.input.reclaimed = vec!["i-1".to_owned()];
    let actions = rig.tick(at(now + 2 * SECOND));
    assert_eq!(count(&actions, "Revoke"), 1);
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert_eq!(failures(&rig), 1);
}

/// Asserts that a notice naming no machine of the records, and one naming a machine that
/// is not held (a release already under way), change nothing: no revoke, no release that
/// was not already due, no strike. It catches a notice that acts on the wrong record or
/// strikes for a machine already being released.
#[test]
fn a_notice_for_an_unknown_or_ending_machine_changes_nothing() {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Active)]);
    rig.input.reclaimed = vec!["i-9".to_owned()];
    let actions = rig.tick(at(3 * SECOND));
    assert_eq!(count(&actions, "Revoke"), 0);
    assert_eq!(count(&actions, "Release"), 0);
    assert_eq!(rig.phase(0), Phase::Registered);
    assert_eq!(failures(&rig), 0);

    rig.input.reclaimed = vec!["i-1".to_owned()];
    rig.tick(at(4 * SECOND));
    assert_eq!(failures(&rig), 1);
    rig.input.reclaimed = vec!["i-1".to_owned()];
    let actions = rig.tick(at(5 * SECOND));
    assert_eq!(kinds(&actions), ["Release"]);
    assert_eq!(failures(&rig), 1);
}

/// Asserts that a machine named twice in one step, and named by a notice in the step whose
/// list shows it preempted, is revoked once and counted once. It catches a duplicate notice
/// that strikes or revokes twice.
#[test]
fn a_machine_named_twice_in_one_step_is_released_once() {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Preempted)]);
    rig.input.reclaimed = vec!["i-1".to_owned(), "i-1".to_owned()];
    let actions = rig.tick(at(3 * SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(failures(&rig), 1);
}
