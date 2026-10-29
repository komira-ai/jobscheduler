//! Scratch probes of the machine lifecycle.

mod common;

use common::*;
use js_elastic::{Held, HostRef, HostStatus};
use js_scaler::{HostState, Mark, Phase};

/// A host the registry still shows as draining, after the scaler took it back and the
/// registry call that undid the mark never ran, is eventually drained again or used.
#[test]
fn a_taken_back_host_whose_undrain_was_lost_is_not_stuck() {
    let (mut rig, _plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    let now = 2 * SECOND + 10 * MINUTE;
    rig.tick(at(now));
    assert_eq!(rig.phase(0), Phase::Draining);
    rig.input.hosts[0].mark = Some(Mark::Draining);
    rig.input.waiting = vec![request("r2")];
    let undrain = rig.tick(at(now + SECOND));
    assert_eq!(count(&undrain, "Undrain"), 1);
    assert_eq!(rig.phase(0), Phase::Registered);
    // The Undrain call is lost: the registry still shows the mark.
    rig.input.waiting = Vec::new();
    let mut seen = 0;
    for n in 2..=240 {
        seen += rig.tick(at(now + n * MINUTE)).len();
    }
    assert!(seen > 0, "four hours later the host is still marked and nothing is asked");
}

/// An orphan the list stops showing once, then shows again, is counted again.
#[test]
fn an_orphan_that_reappears_is_counted_again() {
    let mut rig = Rig::new(settings("gpu"));
    let stray = Held {
        id: HostRef("i-stray".to_owned()),
        acquire: "stray-1".to_owned(),
        name: None,
        offer: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        status: HostStatus::Running,
    };
    rig.pool().held = Some(vec![stray.clone()]);
    rig.tick(at(0));
    assert_eq!(rig.phase(0), Phase::Orphan);
    rig.pool().held = Some(Vec::new());
    rig.tick(at(MINUTE));
    assert_eq!(rig.phase(0), Phase::Gone);
    rig.pool().held = Some(vec![stray]);
    rig.tick(at(2 * MINUTE));
    let counted = rig.state.records.instances.iter().filter(|r| r.counts()).count();
    assert_eq!(counted, 1, "a running machine the list shows is not counted");
}

/// A machine whose host said hello and is silent past grace, and holds work, is not
/// released by the boot timeout.
#[test]
fn a_silent_busy_host_is_not_released_by_the_boot_timeout() {
    let (mut rig, plan, booted) = booting(settings("gpu"), "i-1");
    let mut silent = host(&plan.name, "gpu");
    silent.state = HostState::Lost;
    silent.busy = true;
    rig.input.hosts = vec![silent];
    let actions = rig.tick(at(booted + 15 * MINUTE));
    assert_eq!(count(&actions, "Release"), 0, "{:?}", kinds(&actions));
}
