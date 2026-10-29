//! What a scaler does with the times in its records when it was down for longer than
//! `unknown_hold`. The hosts could not say hello while it was down, so the downtime proves
//! nothing about them.

mod common;

use common::*;
use js_scaler::{HostState, Phase, Records, State};

/// A scaler started from the records file that `rig` has written, given the input `rig`
/// was last given.
fn restart(rig: &Rig) -> Rig {
    let text = rig.state.records.encode().unwrap();
    let mut fresh = Rig::new(settings("gpu"));
    fresh.input = rig.input.clone();
    fresh.state = State::new(Records::decode(&text).unwrap());
    fresh
}

/// Asserts that a machine whose host was missing from a complete view just before the
/// scaler went down for 40 minutes is not released at the first view after the restart:
/// the agent could not say hello meanwhile and has not yet retried. It is released once
/// the hold has passed since the restart, to the millisecond.
#[test]
fn a_missing_host_is_not_released_at_the_first_view_after_a_long_downtime() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts = Vec::new();
    assert_eq!(kinds(&rig.tick(at(10 * SECOND))), ["Save"]);

    let mut again = restart(&rig);
    let back = 40 * MINUTE;
    let actions = again.tick(at(back));
    assert_eq!(count(&actions, "Revoke") + count(&actions, "Release"), 0);
    assert_eq!(again.phase(0), Phase::Registered);

    let almost = again.tick(at(back + 30 * MINUTE - 1));
    assert_eq!(count(&almost, "Release"), 0);
    let actions = again.tick(at(back + 30 * MINUTE));
    assert_eq!(count(&actions, "Release"), 1);
}

/// Asserts that a silent host whose booked lease the registry then gives up, after the
/// scaler was down for 40 minutes, is not drained and marked for release at once: the age
/// of its silence counts from the restart.
#[test]
fn a_silent_host_is_not_drained_when_its_lease_is_given_up_after_a_long_downtime() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].state = HostState::Lost;
    rig.input.hosts[0].busy = true;
    assert_eq!(kinds(&rig.tick(at(10 * SECOND))), ["Save"]);

    let mut again = restart(&rig);
    again.input.hosts[0].state = HostState::Awaiting;
    let back = 40 * MINUTE;
    let actions = again.tick(at(back));
    assert_eq!(count(&actions, "Drain") + count(&actions, "ReleaseHost"), 0);

    again.input.hosts[0].busy = false;
    again.input.hosts[0].lease_given_up = true;
    let actions = again.tick(at(back + MINUTE));
    assert_eq!(count(&actions, "Drain") + count(&actions, "ReleaseHost"), 0);
    assert_eq!(again.phase(0), Phase::Registered);

    let actions = again.tick(at(back + 30 * MINUTE));
    assert_eq!(count(&actions, "Drain"), 1);
}

/// Asserts that records which name no last step, as an older file does, give no span to
/// take away: the whole age of a silent host is taken as downtime, so its count begins at
/// the first step and the hold runs from there, to the millisecond.
#[test]
fn records_without_a_last_step_begin_the_count_at_the_first_step() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].state = HostState::Lost;
    rig.input.hosts[0].lease_given_up = true;
    assert_eq!(kinds(&rig.tick(at(10 * SECOND))), ["Save"]);
    rig.state.records.last_step = None;

    let mut again = restart(&rig);
    let back = 3 * HOUR;
    assert_eq!(count(&again.tick(at(back)), "Drain"), 0);
    assert_eq!(again.state.silent.values().copied().next(), Some(at(back)));
    assert!(only_saves(&again.tick(at(back + 30 * MINUTE - 1))));
    let actions = again.tick(at(back + 30 * MINUTE));
    assert_eq!(count(&actions, "Drain"), 1);
}

/// Asserts that a step which changes nothing does not write the time of the step: the file
/// keeps the time of the last step that wrote it.
#[test]
fn a_step_that_changes_nothing_writes_nothing() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].state = HostState::Lost;
    rig.input.hosts[0].lease_given_up = true;
    assert_eq!(kinds(&rig.tick(at(10 * SECOND))), ["Save"]);
    assert!(rig.tick(at(20 * SECOND)).is_empty());
    assert_eq!(rig.state.records.last_step, Some(at(10 * SECOND)));
}
