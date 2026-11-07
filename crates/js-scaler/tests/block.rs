//! A machine in a reserved block of time (`Held::ends_unix_seconds`): until the block ends
//! it is neither released as idle nor retired at `max_age`, and when it ends the machine's
//! going is expected and counts as no loss. Each test drives the pure `step` on a time it
//! names; none reads a clock.

mod common;

use common::*;
use js_elastic::{Acquired, Held, HostRef, HostStatus};
use js_scaler::{AcquirePlan, AcquireResult, Action, Millis, Phase, Records, Settings, State};

/// The seconds since the Unix epoch of the instant `millis` after the start.
fn secs(millis: u64) -> u64 {
    (START + millis) / SECOND
}

/// The listing of `plan`'s machine `i-1` in `status`, in a block that ends `end` milliseconds
/// after the start.
fn in_block(plan: &AcquirePlan, status: HostStatus, end: u64) -> Held {
    Held {
        ends_unix_seconds: Some(secs(end)),
        ..listed(plan, "i-1", status)
    }
}

/// A rig whose machine `i-1` was accepted at one second in a block that ends `end`
/// milliseconds after the start, and is still Booting.
fn booting_in_block(settings: Settings, end: u64) -> (Rig, AcquirePlan) {
    let mut rig = Rig::new(settings);
    rig.input.waiting = vec![request("r1")];
    let actions = rig.tick(at(0));
    let plan = acquires(&actions).remove(0);
    rig.input.acquired = vec![AcquireResult {
        acquire: plan.acquire.clone(),
        attempt: plan.attempt,
        answer: Acquired::Accepted {
            held: in_block(&plan, HostStatus::Booting, end),
            extras: Vec::new(),
        },
    }];
    rig.pool().held = Some(vec![in_block(&plan, HostStatus::Booting, end)]);
    rig.input.waiting = Vec::new();
    rig.tick(at(SECOND));
    (rig, plan)
}

/// A rig as `booting_in_block` leaves it, then the agent's hello at two seconds: the machine
/// is Registered and quiet, and the list shows it active.
fn registered_in_block(settings: Settings, end: u64) -> (Rig, AcquirePlan) {
    let (mut rig, plan) = booting_in_block(settings, end);
    let pool = rig.input.pools[0].settings.pool.clone();
    rig.input.hosts = vec![host(&plan.name, &pool)];
    rig.pool().held = Some(vec![in_block(&plan, HostStatus::Active, end)]);
    rig.tick(at(2 * SECOND));
    (rig, plan)
}

/// The count of the pool's failure brake, zero when it has none.
fn failures(rig: &Rig) -> u32 {
    rig.state.records.brake.get("gpu").map_or(0, |b| b.failures)
}

/// Asserts that the end the provider stated with its acceptance is kept in the record, in
/// milliseconds, and that a listing which states a later end moves it.
#[test]
fn the_end_of_the_block_is_taken_from_the_acceptance_and_from_each_listing() {
    let (mut rig, plan) = booting_in_block(settings("gpu"), HOUR);
    assert_eq!(rig.record(0).ends_at, Some(at(HOUR)));
    rig.pool().held = Some(vec![in_block(&plan, HostStatus::Active, 2 * HOUR)]);
    rig.tick(at(2 * SECOND));
    assert_eq!(rig.record(0).ends_at, Some(at(2 * HOUR)));
}

/// Asserts that a machine the provider states no end for has none, and that an end too large
/// to hold in milliseconds saturates rather than wrapping.
#[test]
fn a_machine_in_no_block_has_no_end_and_a_huge_end_saturates() {
    let (mut rig, plan, _) = booting(settings("gpu"), "i-1");
    assert_eq!(rig.record(0).ends_at, None);
    let mut far = listed(&plan, "i-1", HostStatus::Active);
    far.ends_unix_seconds = Some(u64::MAX);
    rig.pool().held = Some(vec![far]);
    rig.tick(at(2 * SECOND));
    assert_eq!(rig.record(0).ends_at, Some(Millis::new(u64::MAX)));
}

/// Asserts that a quiet host in a block is kept past `idle` until the block ends, and is
/// drained and its release asked on the step at the end itself, as any idle host is. It
/// catches an idle release of a machine that is paid for to the end of its block.
#[test]
fn an_idle_host_in_a_block_is_kept_until_the_block_ends() {
    let (mut rig, plan) = registered_in_block(settings("gpu"), HOUR);
    assert!(rig.tick(at(2 * SECOND + 10 * MINUTE)).is_empty());
    assert!(rig.tick(at(30 * MINUTE)).is_empty());
    assert!(rig.tick(at(HOUR - 1)).is_empty());
    assert_eq!(rig.phase(0), Phase::Registered);
    let actions = rig.tick(at(HOUR));
    assert_eq!(kinds(&actions), ["Save", "Drain"]);
    assert_eq!(actions[1], Action::Drain(plan.name.clone()));
    assert_eq!(rig.phase(0), Phase::Draining);
}

/// Asserts that the same quiet host with no block is drained at `idle`, so the block is what
/// keeps the host above.
#[test]
fn an_idle_host_in_no_block_is_drained_at_idle() {
    let (mut rig, plan, _) = booting(settings("gpu"), "i-1");
    let pool = rig.input.pools[0].settings.pool.clone();
    rig.input.hosts = vec![host(&plan.name, &pool)];
    rig.tick(at(2 * SECOND));
    let actions = rig.tick(at(2 * SECOND + 10 * MINUTE));
    assert_eq!(kinds(&actions), ["Save", "Drain"]);
}

/// Asserts that a host in a block that outlasts `max_age` is not retired at `max_age`, busy
/// or not, and is retired on the step at the end of the block. It catches a retire of a
/// machine that is paid for to the end of its block.
#[test]
fn a_host_in_a_block_is_not_retired_at_max_age_before_the_block_ends() {
    let bounded = Settings {
        max_age: Some(minutes(60)),
        ..settings("gpu")
    };
    let (mut rig, plan) = registered_in_block(bounded, 2 * HOUR);
    rig.input.hosts[0].busy = true;
    assert!(rig.tick(at(HOUR)).is_empty());
    assert!(rig.tick(at(2 * HOUR - 1)).is_empty());
    let actions = rig.tick(at(2 * HOUR));
    assert_eq!(actions[1], Action::Retire(plan.name.clone()));
    assert_eq!(rig.phase(0), Phase::Draining);
}

/// Asserts that a machine whose hello does not match its offer is released at once though
/// its block has not ended: it cannot serve, so keeping it serves nothing. Catches a block's
/// running time that holds back the release of a machine that came up wrong.
#[test]
fn a_condemned_host_in_a_block_is_still_released() {
    let (mut rig, plan) = booting_in_block(settings("gpu"), HOUR);
    let mut small = host(&plan.name, "gpu");
    shape(&mut small).cpu_millis = 1000;
    rig.input.hosts = vec![small];
    rig.pool().held = Some(vec![in_block(&plan, HostStatus::Active, HOUR)]);
    let actions = rig.tick(at(2 * SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(actions[1], Action::Revoke(plan.name.clone()));
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert!(rig.record(0).condemned);
}

/// Asserts that a block host the list shows preempted at the end of its block is released
/// with no strike on the brake and no cooldown, and that the same listing one millisecond
/// before the end is a loss that strikes. It catches a block's planned end counted as a
/// preemption.
#[test]
fn a_block_host_listed_preempted_at_its_end_is_released_without_a_strike() {
    let (mut rig, plan) = registered_in_block(settings("gpu"), HOUR);
    rig.pool().held = Some(vec![in_block(&plan, HostStatus::Preempted, HOUR)]);
    let actions = rig.tick(at(HOUR));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert_eq!(failures(&rig), 0);
    assert!(rig.state.records.cooldowns.is_empty());
    assert!(rig.state.blocked.is_empty());

    let (mut early, plan) = registered_in_block(settings("gpu"), HOUR);
    early.pool().held = Some(vec![in_block(&plan, HostStatus::Preempted, HOUR)]);
    let actions = early.tick(at(HOUR - 1));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(failures(&early), 1);
    assert!(early.state.records.cooldowns.is_empty());
}

/// Asserts that a machine still booting when the list shows it terminated at the end of its
/// block is released and starts no cooldown and no strike, while one terminated before the
/// end starts the boot cooldown. It catches the end of a block counted as a failed boot.
#[test]
fn a_booting_block_host_listed_terminated_at_its_end_starts_no_cooldown() {
    let (mut rig, plan) = booting_in_block(settings("gpu"), 10 * SECOND);
    rig.pool().held = Some(vec![in_block(&plan, HostStatus::Terminated, 10 * SECOND)]);
    let actions = rig.tick(at(10 * SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert_eq!(failures(&rig), 0);
    assert!(rig.state.records.cooldowns.is_empty());

    let (mut early, plan) = booting_in_block(settings("gpu"), 10 * SECOND);
    early.pool().held = Some(vec![in_block(&plan, HostStatus::Terminated, 10 * SECOND)]);
    early.tick(at(10 * SECOND - 1));
    assert_eq!(early.phase(0), Phase::Releasing);
    assert_eq!(early.state.records.cooldowns.len(), 1);
}

/// Asserts that a reclaim notice for a block host at its end is released with no strike,
/// and one before its end strikes, as a notice for any host does.
#[test]
fn a_reclaim_notice_for_a_block_host_at_its_end_is_not_a_loss() {
    let (mut rig, _) = registered_in_block(settings("gpu"), HOUR);
    rig.input.reclaimed = vec!["i-1".to_owned()];
    let actions = rig.tick(at(HOUR));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(failures(&rig), 0);

    let (mut early, _) = registered_in_block(settings("gpu"), HOUR);
    early.input.reclaimed = vec!["i-1".to_owned()];
    early.tick(at(HOUR - 1));
    assert_eq!(failures(&early), 1);
}

/// Asserts that the end is not written to the records file, so a release before the field
/// reads the file, and that the first list after a restart states it again and the host is
/// kept to the end as before the restart.
#[test]
fn the_end_is_not_stored_and_the_next_list_restores_it_after_a_restart() {
    let (rig, _) = registered_in_block(settings("gpu"), HOUR);
    let text = rig.state.records.encode().unwrap();
    assert!(!text.contains("ends_at"), "{text}");

    let mut fresh = Rig::new(settings("gpu"));
    fresh.input = rig.input.clone();
    fresh.state = State::new(Records::decode(&text).unwrap());
    assert_eq!(fresh.record(0).ends_at, None);
    assert_eq!(count(&fresh.tick(at(30 * MINUTE)), "Drain"), 0);
    assert_eq!(fresh.record(0).ends_at, Some(at(HOUR)));
    assert!(fresh.tick(at(HOUR - 1)).is_empty());
    assert_eq!(kinds(&fresh.tick(at(HOUR))), ["Save", "Drain"]);
}

/// Asserts that a machine listed running that no record knows is adopted as an orphan with
/// the end of the block the list states.
#[test]
fn an_adopted_orphan_takes_the_end_of_its_block_from_the_list() {
    let mut rig = Rig::new(settings("gpu"));
    rig.pool().held = Some(vec![Held {
        id: HostRef("i-9".to_owned()),
        acquire: "stray-acquire".to_owned(),
        name: Some("js-stray".to_owned()),
        offer: "a10".to_owned(),
        zone: None,
        status: HostStatus::Active,
        ends_unix_seconds: Some(secs(HOUR)),
    }]);
    rig.tick(at(SECOND));
    assert_eq!(rig.phase(0), Phase::Orphan);
    assert_eq!(rig.record(0).ends_at, Some(at(HOUR)));
}
