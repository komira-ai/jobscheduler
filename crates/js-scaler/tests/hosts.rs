//! What the scaler does with a host by what the registry shows of it: one it does not hear
//! from, one it no longer shows, one marked releasing, one still marked draining after
//! the scaler took it back, one that said hello below its offer, and the boot clock.

mod common;

use common::*;
use js_elastic::HostStatus;
use js_scaler::{Action, HostState, Mark, Phase};

/// Asserts that the boot clock runs from the launch, as the token's does, and not from
/// the answer: a machine accepted ten minutes after its launch is given up on at
/// `boot_timeout` after the launch.
#[test]
fn a_boot_is_timed_from_the_launch_and_not_from_the_answer() {
    let mut rig = Rig::new(settings("gpu"));
    rig.input.waiting = vec![request("r1")];
    let plan = acquires(&rig.tick(at(0))).remove(0);
    rig.input.waiting = Vec::new();
    rig.input.acquired = vec![accepted(&plan, "i-1")];
    rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Booting)]);
    rig.tick(at(10 * MINUTE));
    assert_eq!(rig.phase(0), Phase::Booting);
    assert!(rig.tick(at(15 * MINUTE - 1)).is_empty());
    let actions = rig.tick(at(15 * MINUTE));
    assert_eq!(count(&actions, "Release"), 1);
    assert_eq!(rig.phase(0), Phase::Releasing);
}

/// Asserts that a host that said hello and has gone silent is not released by the boot
/// timeout, whatever it holds: it is registered, held, and its silence is shown.
#[test]
fn a_silent_busy_host_is_not_released_by_the_boot_timeout() {
    let (mut rig, plan, booted) = booting(settings("gpu"), "i-1");
    let mut silent = host(&plan.name, "gpu");
    silent.state = HostState::Lost;
    silent.busy = true;
    rig.input.hosts = vec![silent];
    let actions = rig.tick(at(booted + 15 * MINUTE));
    assert_eq!(count(&actions, "Release"), 0, "{:?}", kinds(&actions));
    assert_eq!(count(&actions, "Revoke"), 0);
    assert_eq!(rig.phase(0), Phase::Registered);
    assert!(rig.state.silent.contains_key(&plan.name));
}

/// Asserts that a host that announced less than its offer and is quiet is drained and
/// marked for release in the same step, and that one with work is not touched until it
/// is quiet, then drained and marked at once: no work is cut.
#[test]
fn a_short_hello_releases_a_quiet_host_at_once_and_waits_for_a_busy_one() {
    let marks = |actions: &[Action]| -> Vec<String> {
        kinds(actions)
            .into_iter()
            .filter(|name| name == "Drain" || name == "ReleaseHost")
            .collect()
    };
    let (mut quiet, plan, _) = booting(settings("gpu"), "i-1");
    let mut announced = host(&plan.name, "gpu");
    announced.shape.cpu_millis = 7999;
    quiet.input.hosts = vec![announced.clone()];
    assert_eq!(marks(&quiet.tick(at(2 * SECOND))), ["Drain", "ReleaseHost"]);

    let (mut busy, plan, _) = booting(settings("gpu"), "i-1");
    announced.id = js_types::HostId::new(&plan.name).unwrap();
    announced.busy = true;
    busy.input.hosts = vec![announced];
    assert!(marks(&busy.tick(at(2 * SECOND))).is_empty());
    assert_eq!(busy.phase(0), Phase::Registered);
    busy.input.hosts[0].busy = false;
    assert_eq!(marks(&busy.tick(at(3 * SECOND))), ["Drain", "ReleaseHost"]);
}

/// Asserts that a registered or draining machine the registry no longer shows is revoked
/// and released, once the view is complete and it has been missing on two steps
/// `unknown_hold` apart, and counted throughout: the registry forgot the host, the cloud
/// still bills the machine.
#[test]
fn a_machine_the_registry_forgot_is_released() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts = Vec::new();
    rig.tick(at(3 * SECOND));
    let actions = rig.tick(at(3 * SECOND + 30 * MINUTE));
    assert_eq!(count(&actions, "Revoke"), 1);
    assert_eq!(count(&actions, "Release"), 1);
    assert_eq!(rig.phase(0), Phase::Releasing);

    let mut draining = Rig::new(settings("gpu"));
    let mut held = record(Phase::Draining, 60, 0);
    held.registered = true;
    draining.state.records.instances = vec![held];
    draining.tick(at(MINUTE));
    let actions = draining.tick(at(31 * MINUTE));
    assert_eq!(count(&actions, "Revoke"), 1);
    assert_eq!(count(&actions, "Release"), 1);
    assert_eq!(draining.phase(0), Phase::Releasing);
    assert!(draining.record(0).counts());
}

/// Asserts that a host the registry has marked releasing is released even when the
/// scaler had taken it back: the registry books nothing on it and nothing clears the mark.
#[test]
fn a_registered_host_marked_releasing_is_released() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].mark = Some(Mark::Releasing);
    let actions = rig.tick(at(3 * SECOND));
    assert_eq!(count(&actions, "Revoke"), 1);
    assert_eq!(count(&actions, "Release"), 1);
    assert_eq!(rig.phase(0), Phase::Releasing);
}

/// Asserts that a host the registry shows but does not hear from, lost or waiting to say
/// hello again, is left alone and counted, and that the state shows since when it has been
/// silent, until it is heard again.
#[test]
fn a_silent_host_is_held_and_its_age_is_shown() {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].state = HostState::Lost;
    assert!(rig.tick(at(3 * SECOND)).is_empty());
    assert_eq!(rig.state.silent[&plan.name], at(3 * SECOND));
    assert!(rig.tick(at(HOUR)).is_empty());
    assert_eq!(rig.state.silent[&plan.name], at(3 * SECOND));

    rig.input.hosts[0].state = HostState::Awaiting;
    assert!(rig.tick(at(2 * HOUR)).is_empty());
    assert_eq!(rig.state.silent[&plan.name], at(3 * SECOND));
    assert_eq!(rig.phase(0), Phase::Registered);

    rig.input.hosts[0].state = HostState::Live;
    rig.tick(at(3 * HOUR));
    assert!(rig.state.silent.is_empty());
}

/// Asserts that a machine the provider says it no longer has, for a host the registry
/// does not hear from or no longer shows, is revoked, forgotten and removed and ends
/// Gone, while the same answer for a host that is live is not believed.
#[test]
fn a_silent_host_the_provider_cannot_find_is_settled() {
    let silent = |change: fn(&mut Rig)| {
        let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
        change(&mut rig);
        rig.input.not_found = vec![plan.name.clone()];
        let actions = rig.tick(at(3 * SECOND));
        (rig, actions)
    };
    let changes: [fn(&mut Rig); 3] = [
        |rig| rig.input.hosts[0].state = HostState::Lost,
        |rig| rig.input.hosts[0].state = HostState::Awaiting,
        |rig| rig.input.hosts = Vec::new(),
    ];
    for change in changes {
        let (rig, actions) = silent(change);
        assert_eq!(count(&actions, "Revoke"), 1);
        assert_eq!(count(&actions, "RemoveHost"), 1);
        assert_eq!(count(&actions, "ForgetNetwork"), 1);
        assert_eq!(count(&actions, "Release"), 0);
        assert_eq!(rig.phase(0), Phase::Gone);
        assert_eq!(rig.record(0).gone_at, Some(at(3 * SECOND)));
    }
    let (rig, actions) = silent(|_| {});
    assert!(actions.is_empty());
    assert_eq!(rig.phase(0), Phase::Registered);
}

/// Asserts that a host the scaler took back, whose `Undrain` the registry never got, is
/// not stuck: it is asked to undrain again every step until the mark clears.
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

    rig.input.waiting = Vec::new();
    let again = rig.tick(at(now + 2 * SECOND));
    assert_eq!(count(&again, "Undrain"), 1);
    assert_eq!(rig.phase(0), Phase::Registered);

    rig.input.hosts[0].mark = None;
    assert!(rig.tick(at(now + 3 * SECOND)).is_empty());
}

/// Asserts that a request takes back a registered host the registry still shows draining
/// (an earlier undrain was lost) instead of launching, and the host is asked to undrain
/// once.
#[test]
fn a_request_takes_back_a_registered_host_still_shown_draining() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].mark = Some(Mark::Draining);
    rig.input.waiting = vec![request("r2")];
    let actions = rig.tick(at(3 * SECOND));
    assert_eq!(kinds(&actions), ["Undrain"]);
    assert_eq!(rig.phase(0), Phase::Registered);
}

/// A rig as `registered` leaves it, whose registry then stops showing the host.
fn forgotten() -> Rig {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts = Vec::new();
    rig
}

/// Asserts that a view the shell cannot vouch for releases nothing: the machine is held
/// and counted however long the registry shows nothing, and a step on an incomplete
/// view starts the count of missing steps over.
#[test]
fn an_incomplete_registry_view_releases_nothing() {
    let mut rig = forgotten();
    rig.input.registry_complete = false;
    for later in [3 * SECOND, HOUR, 2 * HOUR] {
        let actions = rig.tick(at(later));
        assert_eq!(count(&actions, "Release") + count(&actions, "Revoke"), 0);
        assert_eq!(rig.phase(0), Phase::Registered);
        assert!(rig.record(0).counts());
    }

    let mut reset = forgotten();
    reset.tick(at(3 * SECOND));
    reset.input.registry_complete = false;
    reset.tick(at(10 * MINUTE));
    reset.input.registry_complete = true;
    let actions = reset.tick(at(3 * SECOND + 30 * MINUTE));
    assert_eq!(count(&actions, "Release"), 0);
    assert_eq!(reset.phase(0), Phase::Registered);
}

/// Asserts that a host missing on one step is not released, and that one missing on two
/// complete steps `unknown_hold` apart is, to the millisecond.
#[test]
fn a_host_missing_on_two_complete_steps_is_released() {
    let mut rig = forgotten();
    assert_eq!(count(&rig.tick(at(3 * SECOND)), "Release"), 0);
    assert_eq!(rig.phase(0), Phase::Registered);
    assert_eq!(
        count(&rig.tick(at(3 * SECOND + 30 * MINUTE - 1)), "Release"),
        0
    );
    let actions = rig.tick(at(3 * SECOND + 30 * MINUTE));
    assert_eq!(count(&actions, "Revoke"), 1);
    assert_eq!(count(&actions, "Release"), 1);
    assert_eq!(rig.phase(0), Phase::Releasing);
}

/// Asserts that a host that reappears between the two steps starts the count over.
#[test]
fn a_host_that_reappears_resets_the_count() {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts = Vec::new();
    rig.tick(at(3 * SECOND));
    rig.input.hosts = vec![host(&plan.name, "gpu")];
    rig.tick(at(10 * MINUTE));
    rig.input.hosts = Vec::new();
    assert_eq!(count(&rig.tick(at(3 * SECOND + 30 * MINUTE)), "Release"), 0);
    assert_eq!(rig.phase(0), Phase::Registered);
    let actions = rig.tick(at(3 * SECOND + 60 * MINUTE));
    assert_eq!(count(&actions, "Release"), 1);
}

/// A rig as `registered` leaves it, whose host the registry then hears nothing from.
fn silent_for_a_while(leases_given_up: bool) -> (Rig, String) {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].state = HostState::Lost;
    rig.input.hosts[0].lease_given_up = leases_given_up;
    (rig, plan.name)
}

/// Asserts that a silent elastic host whose leases the registry still holds is not
/// released, however long it is silent.
#[test]
fn a_silent_host_with_leases_still_held_is_not_released() {
    let (mut rig, _) = silent_for_a_while(false);
    for later in [3 * SECOND, 30 * MINUTE, 2 * HOUR, 2 * DAY] {
        let actions = rig.tick(at(later));
        assert_eq!(count(&actions, "Release") + count(&actions, "Revoke"), 0);
        assert_eq!(rig.phase(0), Phase::Registered);
    }
    assert!(rig.state.silent_released.is_empty());
}

/// Asserts that a silent elastic host whose leases were given up is released once it has
/// been silent `unknown_hold`, to the millisecond, the release is named in the state for
/// the shell to log, and the usual read-back then ends it Gone.
#[test]
fn a_silent_host_with_its_leases_given_up_is_released_after_the_hold() {
    let (mut rig, name) = silent_for_a_while(true);
    assert!(rig.tick(at(3 * SECOND)).is_empty());
    assert!(rig.tick(at(3 * SECOND + 30 * MINUTE - 1)).is_empty());
    assert!(rig.state.silent_released.is_empty());
    let actions = rig.tick(at(3 * SECOND + 30 * MINUTE));
    assert_eq!(count(&actions, "Revoke"), 1);
    assert_eq!(count(&actions, "Release"), 1);
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert_eq!(rig.state.silent_released, vec![name.clone()]);

    rig.input.released = vec![js_scaler::ReleaseResult {
        host: name,
        answer: js_elastic::Released::Gone,
    }];
    let actions = rig.tick(at(3 * SECOND + 31 * MINUTE));
    assert_eq!(count(&actions, "RemoveHost"), 1);
    assert_eq!(rig.phase(0), Phase::Gone);
    assert!(rig.state.silent_released.is_empty());
}

/// Asserts that a host the scaler did not launch (a static pool's) is never released,
/// silent or not, whatever the registry says of its leases: it is shown, nothing more.
#[test]
fn a_static_host_is_never_released() {
    let mut rig = Rig::new(settings("gpu"));
    let mut fixed = host("static-1", "gpu");
    fixed.state = HostState::Lost;
    fixed.lease_given_up = true;
    rig.input.hosts = vec![fixed];
    for later in [SECOND, HOUR, 2 * DAY] {
        assert!(rig.tick(at(later)).is_empty());
    }
    assert!(rig.state.records.instances.is_empty());
}
