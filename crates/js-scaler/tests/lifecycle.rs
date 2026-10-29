//! A machine from hello to gone: idle, drain, release, the read-back of a release, a hello
//! below the offer, a boot that does not finish, and a machine the provider lost.

mod common;

use common::*;
use js_elastic::{Claim, GpuClass, HostRef, HostStatus, Released};
use js_scaler::{Action, Blocked, CooldownReason, Health, HostFacts, Mark, Phase, ReleaseResult};

/// A rig as `registered` leaves it, driven on to the drain: the host has been quiet for
/// `idle`, the scaler asked to drain it and the registry shows it `Draining`. Returns the
/// rig, the plan and the time of the drain.
fn draining() -> (Rig, js_scaler::AcquirePlan, u64) {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    let now = 2 * SECOND + 10 * MINUTE;
    rig.tick(at(now));
    rig.input.hosts[0].mark = Some(Mark::Draining);
    (rig, plan, now)
}

fn release_answer(plan: &js_scaler::AcquirePlan, answer: Released) -> Vec<ReleaseResult> {
    vec![ReleaseResult {
        host: plan.name.clone(),
        answer,
    }]
}

/// Asserts the whole way down, in order, across steps: a host quiet for exactly `idle` is
/// drained (a millisecond short is not); while the registry shows it draining the scaler
/// asks for the release every step; once the registry shows it releasing the token is
/// revoked and the machine released, and released again every step whatever the read-back
/// says (terminating, unknown, refused); only `Gone` removes the host and forgets its
/// network key, and the token was revoked once.
#[test]
fn a_quiet_host_is_drained_marked_revoked_and_released_until_gone() {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    let quiet = 2 * SECOND;
    let mut all: Vec<Action> = Vec::new();

    assert!(rig.tick(at(quiet + 10 * MINUTE - 1)).is_empty());
    let actions = rig.tick(at(quiet + 10 * MINUTE));
    assert_eq!(actions[1], Action::Drain(plan.name.clone()));
    assert_eq!(rig.phase(0), Phase::Draining);
    all.extend(actions);

    rig.input.hosts[0].mark = Some(Mark::Draining);
    let after = quiet + 11 * MINUTE;
    all.extend(rig.tick(at(after)));
    all.extend(rig.tick(at(after + SECOND)));

    rig.input.hosts[0].mark = Some(Mark::Releasing);
    let actions = rig.tick(at(after + 2 * SECOND));
    assert_eq!(
        actions[2],
        Action::Release {
            host: plan.name.clone(),
            id: HostRef("i-1".to_owned())
        }
    );
    assert_eq!(rig.phase(0), Phase::Releasing);
    all.extend(actions);

    let answers = [
        Released::Terminating(HostStatus::Terminating),
        Released::Unknown,
        Released::Refused(Claim::Foreign),
    ];
    for (n, answer) in answers.into_iter().enumerate() {
        rig.input.released = release_answer(&plan, answer);
        let step_at = after + (3 + n as u64) * SECOND;
        all.extend(rig.tick(at(step_at)));
        assert_eq!(rig.phase(0), Phase::Releasing);
    }
    rig.input.released = release_answer(&plan, Released::Gone);
    all.extend(rig.tick(at(after + 6 * SECOND)));
    assert_eq!(rig.phase(0), Phase::Gone);
    assert_eq!(rig.record(0).gone_at, Some(at(after + 6 * SECOND)));
    assert!(rig.tick(at(after + 7 * SECOND)).is_empty());

    assert_eq!(
        kinds(&all),
        [
            "Save",
            "Drain",
            "ReleaseHost",
            "ReleaseHost",
            "Save",
            "Revoke",
            "Release",
            "Release",
            "Release",
            "Release",
            "Save",
            "RemoveHost",
            "ForgetNetwork",
        ]
    );
}

/// Asserts that a host that is never quiet is never drained, however long it runs, and
/// that once it goes quiet the idle time counts from then.
#[test]
fn a_host_that_never_goes_idle_is_never_drained() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].busy = true;
    for n in 1..=36 {
        assert!(rig.tick(at(2 * SECOND + n * 5 * MINUTE)).is_empty());
        assert_eq!(rig.phase(0), Phase::Registered);
    }
    let quiet = 4 * HOUR;
    rig.input.hosts[0].busy = false;
    assert!(rig.tick(at(quiet)).is_empty());
    assert!(rig.tick(at(quiet + 10 * MINUTE - 1)).is_empty());
    let actions = rig.tick(at(quiet + 10 * MINUTE));
    assert_eq!(kinds(&actions), ["Save", "Drain"]);
}

/// Asserts that a host a waiting request fits is not drained when it has been idle long
/// enough, that no machine is launched for the request, and that the idle time then
/// counts again from the next step.
#[test]
fn an_idle_host_that_a_request_fits_is_kept() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    let ripe = 2 * SECOND + 10 * MINUTE;
    assert!(rig.tick(at(ripe - 1)).is_empty());
    rig.input.waiting = vec![request("r2")];
    assert!(rig.tick(at(ripe)).is_empty());
    assert_eq!(rig.phase(0), Phase::Registered);

    rig.input.waiting = Vec::new();
    assert!(rig.tick(at(ripe + SECOND)).is_empty());
    assert!(rig.tick(at(ripe + SECOND + 10 * MINUTE - 1)).is_empty());
    assert_eq!(
        count(&rig.tick(at(ripe + SECOND + 10 * MINUTE)), "Drain"),
        1
    );
}

/// Asserts that a request that arrives while the host is draining is not booked on it
/// and gets the host back (`Undrain`) and no machine, that a host that took work after
/// the drain is taken back too, and that a drain the registry refused is read back as a
/// registered host.
#[test]
fn a_request_or_work_arriving_while_draining_takes_the_host_back() {
    let (mut rig, _, now) = draining();
    rig.input.waiting = vec![request("r2")];
    let actions = rig.tick(at(now + SECOND));
    assert_eq!(kinds(&actions), ["Save", "Undrain"]);
    assert_eq!(rig.phase(0), Phase::Registered);

    let (mut busy, _, now) = draining();
    busy.input.hosts[0].busy = true;
    assert_eq!(kinds(&busy.tick(at(now + SECOND))), ["Save", "Undrain"]);
    assert_eq!(busy.phase(0), Phase::Registered);

    let (mut refused, _, now) = draining();
    refused.input.hosts[0].mark = None;
    assert_eq!(kinds(&refused.tick(at(now + SECOND))), ["Save"]);
    assert_eq!(refused.phase(0), Phase::Registered);
    assert!(refused.tick(at(now + 2 * SECOND)).is_empty());
}

/// Asserts that a draining host the registry shows but does not hear from is left alone:
/// nothing is released on its silence, and it stays draining.
#[test]
fn a_draining_host_the_registry_hears_nothing_from_is_left_alone() {
    let (mut silent, _, now) = draining();
    silent.input.hosts[0].state = js_scaler::HostState::Lost;
    assert!(silent.tick(at(now + SECOND)).is_empty());
    assert_eq!(silent.phase(0), Phase::Draining);
}

fn short(change: fn(&mut HostFacts)) -> (Rig, js_scaler::AcquirePlan) {
    let (mut rig, plan, _) = booting(settings("gpu"), "i-1");
    let mut announced = host(&plan.name, "gpu");
    change(&mut announced);
    rig.input.hosts = vec![announced];
    rig.tick(at(2 * SECOND));
    (rig, plan)
}

/// Asserts that a hello below the offer in cpu, memory, gpus or gpu class (or with no
/// class) condemns the host: it is drained, and marked for release, as soon as it is quiet,
/// the pool waits out
/// `boot_fail_cooldown`, and a hello that meets or exceeds the offer does neither.
#[test]
fn a_hello_below_its_offer_condemns_the_host_and_starts_the_cooldown() {
    let below: [fn(&mut HostFacts); 5] = [
        |h| h.shape.cpu_millis = 7999,
        |h| h.shape.memory_bytes = (64 << 30) - 1,
        |h| h.shape.gpus = 0,
        |h| h.shape.gpu_class = Some(GpuClass::parse("nvidia-h100").unwrap()),
        |h| h.shape.gpu_class = None,
    ];
    for change in below {
        let (rig, _) = short(change);
        assert_eq!(rig.phase(0), Phase::Draining);
        assert!(rig.record(0).condemned);
        assert!(rig.record(0).registered);
        let cooldown = rig.state.records.cooldowns["gpu"];
        assert_eq!(cooldown.reason, CooldownReason::BootFailed);
        assert_eq!(cooldown.until, at(2 * SECOND + 10 * MINUTE));
        assert_eq!(rig.state.blocked["gpu"], Blocked::BootFailed);
    }
    let enough: [fn(&mut HostFacts); 2] = [|h| h.busy = false, |h| h.shape.cpu_millis = 16_000];
    for change in enough {
        let (rig, _) = short(change);
        assert_eq!(rig.phase(0), Phase::Registered);
        assert!(!rig.record(0).condemned);
        assert!(rig.state.records.cooldowns.is_empty());
    }
}

/// Asserts that a condemned host is released without waiting for `idle` and is never
/// taken back for a request, though the request fits it.
#[test]
fn a_condemned_host_is_never_taken_back_for_a_request() {
    let (mut rig, plan) = short(|h| h.shape.cpu_millis = 7999);
    rig.input.hosts[0].mark = Some(Mark::Draining);
    rig.input.waiting = vec![request("r2")];
    let actions = rig.tick(at(3 * SECOND));
    assert_eq!(kinds(&actions), ["ReleaseHost"]);
    assert_eq!(actions[0], Action::ReleaseHost(plan.name.clone()));
    assert_eq!(rig.phase(0), Phase::Draining);
}

/// Asserts the boot timeout at its edge, counted from the launch as the join token is: a
/// machine with no hello is left alone a millisecond before `boot_timeout` and at it is
/// revoked and released and the pool waits out the cooldown; a hello that arrives at that
/// very step wins.
#[test]
fn a_boot_that_does_not_finish_is_released_and_starts_the_cooldown() {
    let (mut rig, _, _) = booting(settings("gpu"), "i-1");
    assert!(rig.tick(at(15 * MINUTE - 1)).is_empty());
    assert_eq!(rig.phase(0), Phase::Booting);
    assert!(rig.state.records.cooldowns.is_empty());

    let actions = rig.tick(at(15 * MINUTE));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert_eq!(rig.state.blocked["gpu"], Blocked::BootFailed);

    let until = 25 * MINUTE;
    rig.input.waiting = vec![request("r2")];
    assert_eq!(count(&rig.tick(at(until - 1)), "Acquire"), 0);
    assert_eq!(rig.state.blocked["gpu"], Blocked::BootFailed);
    assert_eq!(count(&rig.tick(at(until)), "Acquire"), 1);
    assert!(rig.state.blocked.is_empty());

    let (mut late, plan, _) = booting(settings("gpu"), "i-1");
    late.input.hosts = vec![host(&plan.name, "gpu")];
    late.tick(at(15 * MINUTE));
    assert_eq!(late.phase(0), Phase::Registered);
    assert!(late.state.records.cooldowns.is_empty());
}

/// Asserts that a machine the provider lists as terminated or preempted is revoked and
/// released whichever way it was going, that only a boot that did not finish starts the
/// cooldown, and that the read-back `Gone` then removes the host.
#[test]
fn a_machine_the_provider_lost_is_released_and_removed() {
    let (mut booting_rig, plan, booted) = booting(settings("gpu"), "i-1");
    booting_rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Terminated)]);
    let actions = booting_rig.tick(at(booted + SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(booting_rig.state.blocked["gpu"], Blocked::BootFailed);
    booting_rig.input.released = release_answer(&plan, Released::Gone);
    let actions = booting_rig.tick(at(booted + 2 * SECOND));
    assert_eq!(kinds(&actions), ["Save", "RemoveHost", "ForgetNetwork"]);
    assert_eq!(booting_rig.phase(0), Phase::Gone);

    let (mut registered_rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    registered_rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Preempted)]);
    let actions = registered_rig.tick(at(3 * SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(registered_rig.phase(0), Phase::Releasing);
    assert!(registered_rig.state.records.cooldowns.is_empty());
    assert!(registered_rig.state.blocked.is_empty());

    let (mut draining_rig, plan, now) = draining();
    draining_rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Terminated)]);
    let actions = draining_rig.tick(at(now + SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(draining_rig.phase(0), Phase::Releasing);
}

/// Asserts that failing or unfinished checks stop launches and not scale-down: a host
/// the registry has marked releasing is still released, and an idle host is still drained.
#[test]
fn failing_health_stops_launches_but_not_release() {
    let (mut rig, _, now) = draining();
    rig.pool().health = Health::Failing;
    rig.input.hosts[0].mark = Some(Mark::Releasing);
    rig.input.waiting = vec![request("r2")];
    let actions = rig.tick(at(now + SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(rig.state.blocked["gpu"], Blocked::Unavailable);

    let (mut checking, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    checking.pool().health = Health::Checking;
    let actions = checking.tick(at(2 * SECOND + 10 * MINUTE));
    assert_eq!(kinds(&actions), ["Save", "Drain"]);
    assert_eq!(checking.state.blocked["gpu"], Blocked::Checking);
}
