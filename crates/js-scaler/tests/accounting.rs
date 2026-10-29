//! What a launch costs while it waits and how long a machine is billed against the day
//! cap, and when a launch the provider told to wait is sent: never in plan mode, not
//! before its time, not past a cap, and once for each call.

mod common;

use std::time::Duration;

use common::*;
use js_elastic::{Acquired, Held, HostRef, HostStatus};
use js_scaler::{Blocked, LaunchMode, Phase, Record, Settings, launchable};

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

/// Asserts that a machine launched 30 hours ago and still running costs only the 24 hours
/// of its run that lie in the window, 1440 cents at a cent a minute, and not its whole 30
/// hours, 1800 cents: with a launch costing 15 cents at least, a day cap of 1455 admits one
/// and a cap of 1454 does not, and a whole-time bill would refuse both.
#[test]
fn a_running_machine_counts_only_its_last_24_hours() {
    for (cap, expected) in [(1455, 1), (1454, 0)] {
        let mut limits = settings("gpu");
        limits.max_spend_per_day = cap;
        let mut rig = Rig::new(limits);
        let mut old = record(Phase::Registered, 60, 0);
        old.registered = true;
        old.offer.cpu_millis = 1000;
        rig.input.hosts = vec![small_host(&old.name)];
        rig.state.records.instances = vec![old];
        rig.input.waiting = vec![request("r1")];
        assert_eq!(acquires(&rig.tick(at(30 * HOUR))).len(), expected, "{cap}");
    }
}

/// Asserts that a launch the provider told to wait is forgotten when no request waits for it
/// any more, in a live pool and in a pool that only plans: its token is revoked, nothing is
/// retried when its time comes, and it no longer counts against the pool.
#[test]
fn a_planned_launch_no_request_waits_for_is_forgotten() {
    for mode in [LaunchMode::Live, LaunchMode::Plan] {
        let (mut rig, plan) = launched(settings("gpu"));
        rig.input.acquired = vec![answered(&plan, deferred(10))];
        rig.tick(at(SECOND));
        assert_eq!(rig.phase(0), Phase::Planned);
        rig.pool().settings.launch = mode;

        rig.input.waiting = Vec::new();
        let actions = rig.tick(at(2 * SECOND));
        assert_eq!(kinds(&actions), ["Save", "Revoke", "ForgetNetwork"]);
        assert!(rig.state.records.instances.is_empty());

        assert!(rig.tick(at(20 * SECOND)).is_empty());
    }
}

/// Asserts that a launch the provider told to wait is kept while a request that fits waits
/// for it, and forgotten once a free host covers that request instead.
#[test]
fn a_planned_launch_is_kept_for_the_request_that_waits_for_it() {
    let (mut rig, plan) = launched(settings("gpu"));
    rig.input.acquired = vec![answered(&plan, deferred(600))];
    rig.tick(at(SECOND));
    assert_eq!(rig.phase(0), Phase::Planned);
    assert!(rig.tick(at(2 * SECOND)).is_empty());

    rig.input.hosts = vec![host("h1", "gpu")];
    let actions = rig.tick(at(3 * SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "ForgetNetwork"]);
    assert!(rig.state.records.instances.is_empty());
}

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
        rig.input.waiting = vec![request("r1")];
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
