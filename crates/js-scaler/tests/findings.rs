//! What a launch costs while it waits, how a long-running machine is billed, what is done
//! with a host the registry does not hear from or no longer shows, and how orphans, ids,
//! tokens and the boot clock behave. Each test drives the pure `step` with a hand-built
//! input and a time it names.

mod common;

use std::time::Duration;

use common::*;
use js_elastic::{Acquired, Extra, Held, HostRef, HostStatus, Offer, Reason, Refusal};
use js_elastic::{RefusalCode, Released};
use js_scaler::{
    Action, Blocked, HostState, LaunchMode, Mark, Millis, Phase, Record, Settings, State,
    launchable, step,
};

fn launched(limits: Settings) -> (Rig, js_scaler::AcquirePlan) {
    let mut rig = Rig::new(limits);
    rig.input.waiting = vec![request("r1")];
    let plan = acquires(&rig.tick(at(0))).remove(0);
    (rig, plan)
}

fn deferred(seconds: u64) -> Acquired {
    Acquired::Deferred {
        wait: Duration::from_secs(seconds),
    }
}

fn stray(id: &str) -> Held {
    Held {
        id: HostRef(id.to_owned()),
        acquire: "stray-1".to_owned(),
        name: None,
        offer: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        status: HostStatus::Active,
    }
}

/// A host too small to cover the waiting request, so it only counts.
fn small_host(name: &str) -> js_scaler::HostFacts {
    let mut small = host(name, "gpu");
    small.shape.cpu_millis = 1000;
    small
}

// ---- what a launch costs while it waits, and how long a machine is billed --------------

/// Asserts that a launch the provider told to wait is not sent when its pool is in plan
/// mode: the operator's go is the `live` line, and nothing is launched without it.
#[test]
fn a_planned_retry_is_not_sent_in_plan_mode() {
    let mut rig = Rig::new(settings("gpu"));
    rig.input.waiting = vec![request("r1")];
    let first = acquires(&rig.tick(at(0))).remove(0);
    rig.input.acquired = vec![answered(&first, deferred(10))];
    rig.tick(at(SECOND));
    assert_eq!(rig.phase(0), Phase::Planned);
    rig.pool().settings.launch = LaunchMode::Plan;
    let actions = rig.tick(at(20 * SECOND));
    assert_eq!(acquires(&actions).len(), 0);
    assert_eq!(count(&actions, "Mint"), 0);
    assert_eq!(rig.phase(0), Phase::Planned);
}

/// Asserts that a launch the provider told to wait keeps counting against the day cap at
/// its least cost, so a second launch is not admitted in the room it holds: two machines
/// at 15 cents each do not fit a day cap of 20.
#[test]
fn a_deferred_launch_still_counts_against_the_day_cap() {
    let mut limits = settings("gpu");
    limits.max_spend_per_day = 20;
    limits.max_step = 5;
    let (mut rig, first) = launched(limits);
    rig.input.acquired = vec![answered(&first, deferred(600))];
    rig.input.waiting = vec![request("r1"), request("r2")];
    let second = acquires(&rig.tick(at(5 * SECOND)));
    let retry = acquires(&rig.tick(at(5 * SECOND + 600 * SECOND)));
    let mut ids: Vec<_> = second
        .iter()
        .chain(&retry)
        .map(|p| p.acquire.clone())
        .collect();
    ids.push(first.acquire.clone());
    ids.sort_by_key(|id| id.as_str().to_owned());
    ids.dedup();
    assert_eq!(ids.len(), 1, "machines acquired against a cap of one");
}

/// Asserts that a launch waiting for its time counts at its least cost in the answer to
/// the registry: with a day cap of 20 one more machine could be launched without it and
/// could not with it.
#[test]
fn a_planned_launch_counts_at_its_least_cost() {
    let mut limits = settings("gpu");
    limits.max_spend_per_day = 20;
    let mut rig = Rig::new(limits);
    assert!(launchable(&rig.state, &rig.input, at(0), &demand()));
    let mut waiting = record(Phase::Planned, 60, 0);
    waiting.retry_at = Some(at(HOUR));
    rig.state.records.instances = vec![waiting];
    assert!(!launchable(&rig.state, &rig.input, at(0), &demand()));
}

/// Asserts that a machine still running counts the part of its run inside the last 24
/// hours: two machines launched 25 and 23 hours ago and still held have spent 24 and 23
/// hours of the window, 2820 cents, over a day cap of 2000.
#[test]
fn a_long_running_machine_counts_for_its_day_inside_the_window() {
    let mut limits = settings("gpu");
    limits.max_spend_per_day = 2000;
    let mut rig = Rig::new(limits);
    let mut a = record(Phase::Registered, 60, 0);
    let mut b = record(Phase::Registered, 60, 2 * HOUR);
    for each in [&mut a, &mut b] {
        each.registered = true;
        each.offer.cpu_millis = 1000;
    }
    rig.input.hosts = vec![small_host(&a.name), small_host(&b.name)];
    rig.state.records.instances = vec![a, b];
    rig.input.waiting = vec![request("r1")];
    assert_eq!(acquires(&rig.tick(at(25 * HOUR))).len(), 0);
    assert_eq!(rig.state.blocked["gpu"], Blocked::MaxSpendDay);
}

/// Asserts the same for an orphan, which is billed from the step that found it and keeps
/// accruing while it is listed: found 25 hours ago, it has spent 24 hours of the window,
/// 1440 cents, over a day cap of 1000.
#[test]
fn an_orphan_counts_for_its_day_inside_the_window() {
    let mut limits = settings("gpu");
    limits.max_spend_per_day = 1000;
    let mut rig = Rig::new(limits);
    let found = record(Phase::Orphan, 60, 0);
    rig.pool().held = Some(vec![stray(found.cloud_id.as_deref().unwrap())]);
    rig.state.records.instances = vec![found];
    rig.input.waiting = vec![request("r1")];
    assert_eq!(acquires(&rig.tick(at(25 * HOUR))).len(), 0);
    assert_eq!(rig.state.blocked["gpu"], Blocked::MaxSpendDay);
}

/// Asserts the edge of the 24 hour window for a machine that ended: one gone exactly a
/// day ago has left it, one gone a millisecond later still counts that millisecond, which
/// is billed as a minute (one cent).
#[test]
fn a_record_leaves_the_window_when_it_ended_a_day_ago() {
    for (gone, expected) in [(DAY, 1), (DAY + 1, 0)] {
        let mut limits = settings("gpu");
        limits.max_spend_per_day = 15;
        let mut old = record(Phase::Gone, 60, 0);
        old.launched_at = at(DAY - HOUR);
        old.registered = true;
        old.gone_at = Some(at(gone));
        let mut rig = Rig::new(limits);
        rig.state.records.instances = vec![old];
        rig.input.waiting = vec![request("r1")];
        assert_eq!(
            acquires(&rig.tick(at(2 * DAY))).len(),
            expected,
            "gone {gone}"
        );
    }
}

// ---- a retry is a launch -----------------------------------------------------------------

/// Asserts that a launch told to wait is checked against every cap again when its time
/// comes: it stays planned, sends nothing and shows the cap, and goes out once the cap
/// has room.
#[test]
fn a_planned_retry_is_checked_against_the_caps_again() {
    let cases = [
        (
            Settings {
                max_instances: 1,
                ..settings("gpu")
            },
            record(Phase::Booting, 60, MINUTE),
            5 * MINUTE,
            Blocked::MaxInstances,
        ),
        (
            Settings {
                max_spend_per_hour: 159,
                ..settings("gpu")
            },
            record(Phase::Booting, 100, MINUTE),
            5 * MINUTE,
            Blocked::MaxSpend,
        ),
        (
            Settings {
                max_spend_per_day: 74,
                ..settings("gpu")
            },
            Record {
                registered: true,
                gone_at: Some(at(MINUTE + HOUR)),
                ..record(Phase::Gone, 60, MINUTE)
            },
            2 * HOUR,
            Blocked::MaxSpendDay,
        ),
    ];
    for (limits, other, now, expected) in cases {
        let mut planned = record(Phase::Planned, 60, 0);
        planned.retry_at = Some(at(0));
        let mut rig = Rig::new(limits);
        rig.state.records.instances = vec![planned, other];
        let actions = rig.tick(at(now));
        assert_eq!(acquires(&actions).len(), 0, "{expected:?}");
        assert_eq!(count(&actions, "Mint"), 0, "{expected:?}");
        assert_eq!(rig.phase(0), Phase::Planned, "{expected:?}");
        assert_eq!(rig.state.blocked["gpu"], expected);

        *rig.pool() = pool(settings("gpu"));
        let actions = rig.tick(at(now + SECOND));
        assert_eq!(acquires(&actions).len(), 1, "{expected:?}");
        assert_eq!(rig.phase(0), Phase::Launching);
    }
}

/// Asserts that an answer replayed for an earlier attempt of an acquire does nothing: a
/// `Deferred` for the first call, arriving after the retry went out, does not plan the
/// launch again or mint a second token, and a `Deferred` for the retry itself does.
#[test]
fn a_replayed_deferral_for_an_earlier_attempt_is_ignored() {
    let (mut rig, plan) = launched(settings("gpu"));
    assert_eq!(plan.attempt, 0);
    rig.input.acquired = vec![answered(&plan, deferred(12))];
    rig.tick(at(SECOND));
    assert_eq!(rig.phase(0), Phase::Planned);

    let retry = acquires(&rig.tick(at(13 * SECOND))).remove(0);
    assert_eq!(retry.acquire, plan.acquire);
    assert_eq!(retry.attempt, 1);
    assert_eq!(rig.record(0).attempt, 1);

    rig.input.acquired = vec![answered(&plan, deferred(12))];
    let actions = rig.tick(at(14 * SECOND));
    assert!(actions.is_empty(), "{:?}", kinds(&actions));
    assert_eq!(rig.phase(0), Phase::Launching);
    assert_eq!(rig.state.refused, 0);

    rig.input.acquired = vec![answered(&retry, deferred(12))];
    rig.tick(at(15 * SECOND));
    assert_eq!(rig.phase(0), Phase::Planned);
    assert_eq!(rig.record(0).retry_at, Some(at(27 * SECOND)));
}

/// Asserts that a pool with a launch waiting for its time launches nothing else until
/// that time has passed, says why, and then launches for what is left.
#[test]
fn a_pool_with_a_launch_waiting_for_its_time_launches_nothing_else() {
    let mut limits = settings("gpu");
    limits.max_step = 5;
    let (mut rig, plan) = launched(limits);
    rig.input.acquired = vec![answered(&plan, deferred(600))];
    rig.input.waiting = vec![request("r1"), request("r2")];
    assert!(acquires(&rig.tick(at(5 * SECOND))).is_empty());
    assert_eq!(rig.state.blocked["gpu"], Blocked::Deferred);

    let actions = rig.tick(at(605 * SECOND));
    assert_eq!(acquires(&actions).len(), 2);
    assert!(rig.state.blocked.is_empty());
}

// ---- demand across pools -----------------------------------------------------------------

/// Asserts that a request a machine on its way, or a free host, of a later pool covers
/// gets no machine from an earlier pool, and that no `no_offer` is shown for a request
/// that is covered.
#[test]
fn a_request_covered_by_any_pool_launches_nothing() {
    let mut coming = Rig::new(settings("first"));
    coming.input.pools.push(pool(settings("second")));
    let mut on_its_way = record(Phase::Booting, 60, 0);
    on_its_way.pool = "second".to_owned();
    coming.state.records.instances = vec![on_its_way];
    coming.input.waiting = vec![request("r1")];
    assert!(acquires(&coming.tick(at(SECOND))).is_empty());
    assert!(coming.state.blocked.is_empty());

    let mut free = Rig::new(settings("first"));
    free.input.pools.push(pool(settings("second")));
    free.input.hosts = vec![host("h1", "second")];
    free.input.waiting = vec![request("r1")];
    assert!(acquires(&free.tick(at(SECOND))).is_empty());

    let mut other = Rig::new(settings("first"));
    other.input.pools.push(pool(settings("second")));
    other.input.waiting = vec![request("r1")];
    assert_eq!(acquires(&other.tick(at(SECOND)))[0].pool, "first");
}

/// Asserts that `no_offer` names only requests that nothing covers: a request a free host
/// serves has none, though no offer of the pool fits it.
#[test]
fn a_covered_request_is_not_shown_as_having_no_offer() {
    let mut rig = Rig::new(settings("gpu"));
    rig.pool().offers = vec![Offer {
        os: "macos".to_owned(),
        ..offer("m", 60)
    }];
    rig.input.hosts = vec![host("h1", "gpu")];
    rig.input.waiting = vec![request("r1")];
    rig.tick(at(0));
    assert!(rig.state.no_offer.is_empty());
}

// ---- ids, tokens and the boot clock ---------------------------------------------------------

/// Asserts that an acquire id is the pool and the 128 random bits the caller gave, in
/// hex, one value for each launch in order; and that a step given too few values, or one
/// twice, launches only what it has a fresh value for and says so.
#[test]
fn an_acquire_id_is_the_pool_and_the_random_bits_given() {
    let mut limits = settings("gpu");
    limits.max_step = 3;
    let mut rig = Rig::new(limits);
    rig.input.waiting = vec![request("r1"), request("r2"), request("r3")];
    rig.input.entropy = vec![draw(1), draw(2), draw(3)];
    let plans = acquires(&rig.tick(at(0)));
    let ids: Vec<String> = plans.iter().map(|p| p.acquire.to_string()).collect();
    assert_eq!(ids, [1, 2, 3].map(|n| format!("gpu-{}", draw_text(n))));

    let mut repeated = Rig::new(Settings {
        max_step: 3,
        ..settings("gpu")
    });
    repeated.input.waiting = vec![request("r1"), request("r2"), request("r3")];
    repeated.input.entropy = vec![draw(7), draw(7), draw(8)];
    let (state, actions) = step(State::default(), &repeated.input, at(0));
    assert_eq!(acquires(&actions).len(), 1);
    assert_eq!(state.blocked["gpu"], Blocked::Entropy);

    repeated.input.entropy = Vec::new();
    let (state, actions) = step(State::default(), &repeated.input, at(0));
    assert!(actions.is_empty());
    assert_eq!(state.blocked["gpu"], Blocked::Entropy);
}

/// Asserts that a join token lives `boot_timeout` plus the margin, 20 minutes by
/// default, in the mint and in the record, and that the margin is a setting.
#[test]
fn a_join_token_lives_the_boot_timeout_plus_its_margin() {
    for (margin, lives) in [(None, 20), (Some(7), 22)] {
        let mut limits = settings("gpu");
        if let Some(minutes_more) = margin {
            limits.token_margin = minutes(minutes_more);
        }
        let mut rig = Rig::new(limits);
        rig.input.waiting = vec![request("r1")];
        let actions = rig.tick(at(0));
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::Mint { valid_for, .. } if *valid_for == minutes(lives)
        )));
        assert_eq!(rig.record(0).token_expires, Some(at(lives * MINUTE)));
    }
}

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

// ---- hello below the offer ------------------------------------------------------------------

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

// ---- a host the registry does not hear from or no longer shows ---------------------------------

/// Asserts that a registered or draining machine the registry no longer shows is revoked
/// and released, and counted until the provider says it is gone: the registry forgot
/// the host, the cloud still bills the machine.
#[test]
fn a_machine_the_registry_forgot_is_released() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts = Vec::new();
    let actions = rig.tick(at(3 * SECOND));
    assert_eq!(count(&actions, "Revoke"), 1);
    assert_eq!(count(&actions, "Release"), 1);
    assert_eq!(rig.phase(0), Phase::Releasing);

    let mut draining = Rig::new(settings("gpu"));
    let mut held = record(Phase::Draining, 60, 0);
    held.registered = true;
    draining.state.records.instances = vec![held];
    let actions = draining.tick(at(MINUTE));
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

// ---- orphans and machines that were gone ----------------------------------------------------------

/// Asserts that an orphan missing from a list is still counted: it is gone after two
/// lists that lack it, `unknown_hold` apart, and a list that shows it again starts the
/// count over.
#[test]
fn an_orphan_is_gone_only_after_two_clean_lists() {
    let mut rig = Rig::new(settings("gpu"));
    rig.pool().held = Some(vec![stray("i-stray")]);
    rig.tick(at(0));
    assert_eq!(rig.phase(0), Phase::Orphan);

    rig.pool().held = Some(Vec::new());
    rig.tick(at(MINUTE));
    assert_eq!(rig.phase(0), Phase::Orphan);
    assert_eq!(rig.record(0).clean_lists, 1);
    assert!(rig.record(0).counts());

    rig.pool().held = Some(vec![stray("i-stray")]);
    rig.tick(at(2 * MINUTE));
    assert_eq!(rig.state.records.instances.len(), 1);
    assert_eq!(rig.record(0).clean_lists, 0);
    assert_eq!(rig.record(0).last_clean, None);

    rig.pool().held = Some(Vec::new());
    rig.tick(at(3 * MINUTE));
    assert_eq!(rig.record(0).clean_lists, 1);
    rig.tick(at(33 * MINUTE - 1));
    assert_eq!(rig.phase(0), Phase::Orphan);
    rig.tick(at(33 * MINUTE));
    assert_eq!(rig.phase(0), Phase::Gone);
    assert_eq!(rig.record(0).gone_at, Some(at(33 * MINUTE)));
}

/// Asserts that a machine whose record is gone or was refused, but which a list shows
/// running, is counted again as an orphan, and the old record is left as it was.
#[test]
fn a_machine_listed_running_after_it_was_gone_is_counted_again() {
    for phase in [Phase::Gone, Phase::Refused] {
        let mut rig = Rig::new(settings("gpu"));
        let mut old = record(phase, 60, 0);
        old.gone_at = Some(at(MINUTE));
        rig.pool().held = Some(vec![Held {
            acquire: String::new(),
            ..stray(old.cloud_id.as_deref().unwrap())
        }]);
        rig.state.records.instances = vec![old.clone()];
        rig.tick(at(HOUR));
        assert_eq!(rig.state.records.instances.len(), 2, "{phase:?}");
        assert_eq!(rig.record(0), &old);
        assert_eq!(rig.phase(1), Phase::Orphan);
        assert!(rig.record(1).counts());
    }
}

/// Asserts that the machines an accepted launch named besides the one wanted are recorded
/// as the provider reported them: one it released is a gone record, billed; one it could
/// not confirm is an orphan that is counted until a list stops showing it.
#[test]
fn the_extras_of_an_accepted_launch_are_recorded() {
    let (mut rig, plan) = launched(settings("gpu"));
    rig.input.waiting = Vec::new();
    let mut answer = accepted(&plan, "i-1");
    if let Acquired::Accepted { extras, .. } = &mut answer.answer {
        *extras = vec![
            Extra {
                id: HostRef("i-x1".to_owned()),
                released: Released::Gone,
            },
            Extra {
                id: HostRef("i-x2".to_owned()),
                released: Released::Terminating(HostStatus::Terminating),
            },
        ];
    }
    rig.input.acquired = vec![answer];
    rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Booting)]);
    rig.tick(at(SECOND));
    assert_eq!(rig.state.records.instances.len(), 3);
    let gone = rig.record(1);
    assert_eq!(gone.phase, Phase::Gone);
    assert_eq!(gone.cloud_id.as_deref(), Some("i-x1"));
    assert_eq!(gone.gone_at, Some(at(SECOND)));
    assert!(!gone.counts());
    let unsure = rig.record(2);
    assert_eq!(unsure.phase, Phase::Orphan);
    assert_eq!(unsure.cloud_id.as_deref(), Some("i-x2"));
    assert!(unsure.counts());
}

// ---- what the records and the actions carry ---------------------------------------------------------

/// Asserts that the first action of a step that changed the records is all of them, the
/// cooldowns with them, and that a step that changes nothing writes nothing.
#[test]
fn the_whole_records_are_written_first() {
    let (mut rig, plan) = launched(settings("gpu"));
    rig.input.acquired = vec![answered(
        &plan,
        Acquired::Refused(Refusal {
            code: RefusalCode::Quota,
            reason: Reason::DryRun,
        }),
    )];
    let actions = rig.tick(at(5 * SECOND));
    assert_eq!(rig.state.records.cooldowns.len(), 1);
    assert_eq!(
        actions[0],
        Action::Save(Box::new(rig.state.records.clone()))
    );
    assert!(rig.tick(at(6 * SECOND)).is_empty());
}

/// Asserts that a machine being released with no provider id to release it by is refused
/// and counted, and no release is sent for an empty id.
#[test]
fn a_release_with_no_provider_id_is_refused_and_counted() {
    let mut rig = Rig::new(settings("gpu"));
    let mut stuck = record(Phase::Releasing, 60, 0);
    stuck.cloud_id = None;
    rig.state.records.instances = vec![stuck];
    let actions = rig.tick(at(MINUTE));
    assert_eq!(count(&actions, "Release"), 0);
    assert_eq!(rig.state.refused, 1);
}

/// Asserts that an instant past the largest the lease clock holds saturates there, in
/// `new` and in `plus`.
#[test]
fn a_time_past_the_largest_instant_saturates() {
    let largest = u64::try_from(i64::MAX).unwrap();
    assert_eq!(Millis::new(u64::MAX).as_millis(), largest);
    assert_eq!(
        Millis::new(largest)
            .plus(Duration::from_secs(1))
            .as_millis(),
        largest
    );
}
