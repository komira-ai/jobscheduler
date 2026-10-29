//! What the shell's duties lean on: a booting machine is not given up on at the first
//! complete view after a restart, a stale run does not keep a given-up host from being
//! released, a host with no known shape is not judged, a price of zero is no price, and the
//! silent and missing times survive a restart.

mod common;

use common::*;
use js_elastic::{Held, HostRef, HostStatus, Offer};
use js_scaler::{
    Blocked, CooldownReason, HostFacts, HostState, Mark, Phase, Records, State, launchable,
};

/// The name of the machine `record(Phase::Booting, 60, 0)` describes.
const BOOTER: &str = "js-made-0-60";

/// The boot hold of the default settings: a third of `boot_timeout`.
const BOOT_HOLD: u64 = 5 * MINUTE;

/// A scaler that has just started with a booting machine launched at the start of the
/// tests on its records and nothing shown by the registry.
fn restarted_booting() -> Rig {
    let mut rig = Rig::new(settings("gpu"));
    rig.state = State::new(Records {
        instances: vec![record(Phase::Booting, 60, 0)],
        ..Records::default()
    });
    rig
}

/// A scaler started from the records file that `rig` has written, given the input `rig`
/// was last given.
fn restart(rig: &Rig) -> Rig {
    let text = rig.state.records.encode().unwrap();
    let mut fresh = Rig::new(settings("gpu"));
    fresh.input = rig.input.clone();
    fresh.state = State::new(Records::decode(&text).unwrap());
    fresh
}

/// Asserts that the step to `at` revokes and releases nothing and starts no cooldown.
fn assert_kept(rig: &mut Rig, now: u64) {
    let actions = rig.tick(at(now));
    assert!(actions.is_empty(), "{:?}", kinds(&actions));
    assert_eq!(rig.phase(0), Phase::Booting);
    assert!(rig.state.records.cooldowns.is_empty());
}

/// Asserts that a machine launched long ago and still booting is not given up on at the
/// first complete view after a restart: its host may be seconds from saying hello. It is
/// given up on once the hosts have been missing from complete views for the boot hold, to
/// the millisecond, and the pool then waits out the cooldown.
#[test]
fn a_booting_machine_is_not_given_up_on_at_the_first_view_after_a_restart() {
    let mut rig = restarted_booting();
    assert_kept(&mut rig, 20 * MINUTE);
    assert_kept(&mut rig, 20 * MINUTE + BOOT_HOLD - 1);
    let actions = rig.tick(at(20 * MINUTE + BOOT_HOLD));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert_eq!(rig.state.blocked["gpu"], Blocked::BootFailed);
    assert_eq!(
        rig.state.records.cooldowns["gpu"].reason,
        CooldownReason::BootFailed
    );
}

/// Asserts that a hello inside the hold keeps the machine: it is registered, nothing is
/// revoked or released, and the pool does not cool off.
#[test]
fn a_hello_inside_the_boot_hold_keeps_the_machine() {
    let mut rig = restarted_booting();
    assert_kept(&mut rig, 20 * MINUTE);
    rig.input.hosts = vec![host(BOOTER, "gpu")];
    let actions = rig.tick(at(20 * MINUTE + 2 * MINUTE));
    assert_eq!(count(&actions, "Revoke") + count(&actions, "Release"), 0);
    assert_eq!(rig.phase(0), Phase::Registered);
    assert!(rig.state.records.cooldowns.is_empty());

    rig.input.hosts = Vec::new();
    let actions = rig.tick(at(20 * MINUTE + 3 * MINUTE));
    assert_eq!(count(&actions, "Release"), 0);
}

/// Asserts that the hold starts at the first complete view and not at an incomplete one,
/// and that an incomplete view inside the hold neither releases nor starts the count over.
#[test]
fn the_boot_hold_counts_complete_views_and_an_incomplete_one_does_not_reset_it() {
    let mut rig = restarted_booting();
    rig.input.registry_complete = false;
    assert_kept(&mut rig, 20 * MINUTE);
    assert_kept(&mut rig, 30 * MINUTE);

    rig.input.registry_complete = true;
    assert_kept(&mut rig, 31 * MINUTE);
    rig.input.registry_complete = false;
    assert_kept(&mut rig, 33 * MINUTE);
    rig.input.registry_complete = true;
    assert_kept(&mut rig, 31 * MINUTE + BOOT_HOLD - 1);
    let actions = rig.tick(at(31 * MINUTE + BOOT_HOLD));
    assert_eq!(count(&actions, "Release"), 1);
    assert_eq!(rig.phase(0), Phase::Releasing);
}

/// A rig whose registered host is silent with `busy` set, as a host that went lost in the
/// middle of a job is shown, in `state`.
fn lost_mid_job(state: HostState, lease_given_up: bool) -> (Rig, String) {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    let host = &mut rig.input.hosts[0];
    host.state = state;
    host.busy = true;
    host.lease_given_up = lease_given_up;
    (rig, plan.name)
}

/// Asserts that a silent host that shows runs but whose lease the registry gave up is
/// released like an idle one, once silent for `unknown_hold`, to the millisecond: drained
/// and marked, then revoked and released when the mark is read back. The runs it lists are
/// those of a job that can no longer report.
#[test]
fn a_silent_host_with_stale_runs_is_released_once_its_lease_is_given_up() {
    for state in [HostState::Lost, HostState::Awaiting] {
        let (mut rig, name) = lost_mid_job(state, true);
        assert_eq!(kinds(&rig.tick(at(3 * SECOND))), ["Save"]);
        assert!(only_saves(&rig.tick(at(3 * SECOND + 30 * MINUTE - 1))));
        let actions = rig.tick(at(3 * SECOND + 30 * MINUTE));
        assert_eq!(kinds(&actions), ["Save", "Drain", "ReleaseHost"]);
        assert_eq!(rig.phase(0), Phase::Draining);

        rig.input.hosts[0].mark = Some(Mark::Releasing);
        let actions = rig.tick(at(3 * SECOND + 31 * MINUTE));
        assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
        assert_eq!(rig.state.silent_released, vec![name]);
    }
}

/// Asserts that the same host while the registry still holds its lease is held however
/// long it is silent, and that a `Held` or `Reserved` mark holds a host whose lease was
/// given up.
#[test]
fn a_silent_host_that_still_books_a_lease_or_is_set_aside_is_held() {
    let cases: [fn(&mut HostFacts); 3] = [
        |host| host.lease_given_up = false,
        |host| host.mark = Some(Mark::Held),
        |host| host.mark = Some(Mark::Reserved),
    ];
    for change in cases {
        let (mut rig, _) = lost_mid_job(HostState::Lost, true);
        change(&mut rig.input.hosts[0]);
        assert_eq!(kinds(&rig.tick(at(3 * SECOND))), ["Save"]);
        for later in [3 * SECOND + 30 * MINUTE, 2 * HOUR, 2 * DAY] {
            let actions = rig.tick(at(later));
            assert!(only_saves(&actions), "{:?}", kinds(&actions));
            assert_eq!(rig.phase(0), Phase::Registered);
        }
    }
}

/// Asserts that a host that hello'd before a restart and has not announced a shape since is
/// not judged below its offer, whatever state the registry shows it in: it is registered,
/// not condemned, and the pool gets no cooldown.
#[test]
fn a_host_with_no_known_shape_is_not_condemned() {
    for state in [HostState::Awaiting, HostState::Live] {
        let (mut rig, plan, booted) = booting(settings("gpu"), "i-1");
        let mut unknown = host(&plan.name, "gpu");
        unknown.state = state;
        unknown.shape = None;
        unknown.busy = true;
        rig.input.hosts = vec![unknown];
        let actions = rig.tick(at(booted + SECOND));
        assert_eq!(count(&actions, "Drain") + count(&actions, "ReleaseHost"), 0);
        assert_eq!(rig.phase(0), Phase::Registered);
        assert!(rig.record(0).registered);
        assert!(!rig.record(0).condemned);
        assert!(rig.state.records.cooldowns.is_empty());
        assert!(rig.state.blocked.is_empty());
    }
}

/// Asserts that a live host with no known shape covers no request: a machine is launched.
#[test]
fn a_host_with_no_known_shape_covers_nothing() {
    let mut rig = Rig::new(settings("gpu"));
    let mut unknown = host("h1", "gpu");
    unknown.shape = None;
    rig.input.hosts = vec![unknown];
    rig.input.waiting = vec![request("r1")];
    assert_eq!(acquires(&rig.tick(at(0))).len(), 1);
}

/// Asserts that an offer priced at zero is not launched, as an unpriced one is not: a
/// zero would pass both spend caps for any number of machines. A priced offer is still
/// chosen beside it, and `launchable` says no when only the free one fits.
#[test]
fn an_offer_priced_zero_is_not_launched() {
    let mut only = Rig::new(settings("gpu"));
    only.pool().offers = vec![offer("a10", 0)];
    only.input.waiting = vec![request("r1")];
    assert!(acquires(&only.tick(at(0))).is_empty());
    assert!(!launchable(&only.state, &only.input, at(0), &demand()));
    assert_eq!(only.state.blocked["gpu"], Blocked::Unpriced);

    let mut beside = Rig::new(settings("gpu"));
    beside.pool().offers = vec![offer("free", 0), offer("a10", 60)];
    beside.input.waiting = vec![request("r1")];
    let plans = acquires(&beside.tick(at(0)));
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].offer.id, "a10");
    assert_eq!(plans[0].offer.price_cents_per_hour, Some(60));
    assert!(beside.state.blocked.is_empty());
}

/// Asserts that a request which fits only an offer with no price, or with a price of zero,
/// shows the pool as blocked on it, and that a request nothing fits does not: that one is
/// named in `no_offer` instead.
#[test]
fn a_request_that_fits_only_an_unpriced_offer_shows_the_pool_blocked() {
    let mut rig = Rig::new(settings("gpu"));
    rig.pool().offers = vec![Offer {
        price_cents_per_hour: None,
        ..offer("a10", 0)
    }];
    rig.input.waiting = vec![request("r1")];
    assert!(acquires(&rig.tick(at(0))).is_empty());
    assert_eq!(rig.state.blocked["gpu"], Blocked::Unpriced);

    rig.pool().offers[0].available = false;
    rig.tick(at(SECOND));
    assert!(rig.state.blocked.is_empty());
}

/// Asserts that a machine found at the provider whose offer the catalog prices at zero is
/// counted at the whole hourly cap, as one the catalog does not price is.
#[test]
fn an_orphan_the_catalog_prices_at_zero_counts_at_the_hourly_cap() {
    let mut rig = Rig::new(settings("gpu"));
    rig.pool().offers = vec![offer("a10", 0)];
    rig.pool().held = Some(vec![Held {
        id: HostRef("i-stray".to_owned()),
        acquire: "stray-launch".to_owned(),
        name: None,
        offer: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        status: HostStatus::Active,
    }]);
    rig.tick(at(0));
    assert_eq!(rig.phase(0), Phase::Orphan);
    assert_eq!(rig.record(0).price_cents_per_hour, 1000);
}

/// A rig whose registered host is silent with its leases given up.
fn silent_registered() -> (Rig, String) {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].state = HostState::Lost;
    rig.input.hosts[0].lease_given_up = true;
    (rig, plan.name)
}

/// A rig whose registered host the registry then stops showing.
fn forgotten() -> (Rig, String) {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts = Vec::new();
    (rig, plan.name)
}

/// Asserts that the first time a host is seen silent is written with the records, when it
/// is first seen and when the host is heard again, and at no other step.
#[test]
fn the_silent_time_is_saved_when_it_starts_and_when_it_ends() {
    let (mut rig, name) = silent_registered();
    assert_eq!(kinds(&rig.tick(at(3 * SECOND))), ["Save"]);
    assert_eq!(rig.state.records.silent_since[&name], at(3 * SECOND));
    assert!(only_saves(&rig.tick(at(10 * MINUTE))));
    assert_eq!(rig.state.records.silent_since[&name], at(3 * SECOND));

    rig.input.hosts[0].state = HostState::Live;
    assert_eq!(kinds(&rig.tick(at(20 * MINUTE))), ["Save"]);
    assert!(rig.state.records.silent_since.is_empty());
}

/// Makes the provider list a machine nobody launched, so that the step at `now` changes the
/// records and writes them; the scaler's last write is then `now`. `n` tells the machines
/// apart.
fn write_at(rig: &mut Rig, n: u64, now: u64) {
    let held = Held {
        id: HostRef(format!("stray-{n}")),
        acquire: String::new(),
        name: None,
        offer: "a10".to_owned(),
        zone: None,
        status: HostStatus::Active,
    };
    rig.pool().held.as_mut().unwrap().push(held);
    assert_eq!(kinds(&rig.tick(at(now))), ["Save"]);
}

/// Asserts that the time a scaler observed a silent host before a restart and the time it
/// observes it after add up to the hold, to the millisecond: the hold is reached across a
/// restart, and the downtime between the two is not part of it.
#[test]
fn a_silent_host_is_released_when_the_time_seen_before_and_after_a_restart_adds_up() {
    let (mut first, name) = silent_registered();
    first.tick(at(3 * SECOND));
    write_at(&mut first, 1, 14 * MINUTE);

    let mut second = restart(&first);
    let back = HOUR;
    assert_eq!(count(&second.tick(at(back)), "Drain"), 0);
    let seen = 14 * MINUTE - 3 * SECOND;
    let last = back + 30 * MINUTE - seen;
    assert_eq!(second.state.silent[&name], at(back - seen));
    assert!(only_saves(&second.tick(at(last - 1))));
    let actions = second.tick(at(last));
    assert_eq!(kinds(&actions), ["Save", "Drain", "ReleaseHost"]);
}

/// Asserts the same for a machine missing from complete views, through the incomplete view
/// a restarted registry shows first.
#[test]
fn a_missing_host_is_released_when_the_time_seen_before_and_after_a_restart_adds_up() {
    let (mut first, name) = forgotten();
    assert_eq!(kinds(&first.tick(at(3 * SECOND))), ["Save"]);
    assert_eq!(first.state.records.missing_since[&name], at(3 * SECOND));
    write_at(&mut first, 1, 14 * MINUTE);

    let mut second = restart(&first);
    second.input.registry_complete = false;
    let back = HOUR;
    assert_eq!(count(&second.tick(at(back)), "Release"), 0);
    let seen = 14 * MINUTE - 3 * SECOND;
    assert_eq!(second.state.records.missing_since[&name], at(back - seen));
    second.input.registry_complete = true;
    let last = back + 30 * MINUTE - seen;
    assert_eq!(count(&second.tick(at(last - 1)), "Release"), 0);
    let actions = second.tick(at(last));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(second.phase(0), Phase::Releasing);
}

/// Asserts that a scaler restarted again and again reaches the hold after enough time it
/// observed, spread over three runs with hours of downtime between them, and not before.
#[test]
fn the_hold_is_reached_by_time_seen_across_several_restarts() {
    let (mut first, _) = silent_registered();
    first.tick(at(3 * SECOND));
    write_at(&mut first, 1, 10 * MINUTE);

    let mut second = restart(&first);
    assert_eq!(count(&second.tick(at(3 * HOUR)), "Drain"), 0);
    write_at(&mut second, 2, 3 * HOUR + 10 * MINUTE);

    let mut third = restart(&second);
    assert_eq!(count(&third.tick(at(9 * HOUR)), "Drain"), 0);
    let seen = 2 * (10 * MINUTE) - 3 * SECOND;
    let last = 9 * HOUR + 30 * MINUTE - seen;
    assert!(only_saves(&third.tick(at(last - 1))));
    let actions = third.tick(at(last));
    assert_eq!(kinds(&actions), ["Save", "Drain", "ReleaseHost"]);
}

/// Asserts that restarts alone never reach the hold, however long the scaler is down
/// between them.
#[test]
fn downtime_alone_never_reaches_the_hold() {
    let (mut rig, _) = silent_registered();
    rig.tick(at(3 * SECOND));
    for n in 1..=10 {
        rig = restart(&rig);
        let actions = rig.tick(at(3 * SECOND + n * 40 * MINUTE));
        assert_eq!(count(&actions, "Drain") + count(&actions, "ReleaseHost"), 0);
        assert_eq!(rig.phase(0), Phase::Registered);
    }
}

/// Asserts that a restarted scaler that was never written a time starts the count at its
/// first step: nothing is released before the hold, which is the safe side.
#[test]
fn a_host_first_seen_missing_after_a_restart_starts_its_count_there() {
    let (first, _) = forgotten();
    let mut second = restart(&first);
    assert_eq!(count(&second.tick(at(2 * HOUR)), "Release"), 0);
    let actions = second.tick(at(2 * HOUR + 30 * MINUTE));
    assert_eq!(count(&actions, "Release"), 1);
}
