//! A restart between a record and the call it describes, acquires that get no answer or a
//! refusal or a delay, and machines the records do not know.

mod common;

use common::*;
use js_elastic::{Acquired, Held, HostRef, HostStatus, Reason, Refusal, RefusalCode, Released};
use js_scaler::{
    AcquirePlan, Action, Blocked, Health, Phase, Records, ReleaseResult, Settings, State,
};

fn refusal(code: RefusalCode) -> Acquired {
    Acquired::Refused(Refusal {
        code,
        reason: Reason::DryRun,
    })
}

/// A rig with one request waiting whose launch is made at the start: returns the rig and
/// the plan of the acquire.
fn launched(limits: Settings) -> (Rig, AcquirePlan) {
    let mut rig = Rig::new(limits);
    rig.input.waiting = vec![request("r1")];
    let plan = acquires(&rig.tick(at(0))).remove(0);
    (rig, plan)
}

fn one_machine(mut limits: Settings) -> Settings {
    limits.max_instances = 1;
    limits
}

/// Asserts that a scaler restarted from the records file it wrote before the call neither
/// acquires nor mints again, and that when the list then shows the machine the record
/// moves on to booting with the provider's id, still without either.
#[test]
fn a_restart_between_the_record_and_the_call_launches_nothing_twice() {
    let (rig, plan) = launched(settings("gpu"));
    let text = rig.state.records.encode().unwrap();
    let mut fresh = Rig::new(settings("gpu"));
    fresh.input = rig.input.clone();
    fresh.state = State::new(Records::decode(&text).unwrap());
    assert!(fresh.tick(at(5 * SECOND)).is_empty());

    fresh.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Booting)]);
    let actions = fresh.tick(at(10 * SECOND));
    assert_eq!(kinds(&actions), ["Save"]);
    assert_eq!(fresh.phase(0), Phase::Booting);
    assert_eq!(fresh.record(0).cloud_id.as_deref(), Some("i-1"));
}

/// Asserts that an acquire that got no answer is not repeated while the request waits,
/// and that a list showing the machine moves it to booting with the id the list gives.
#[test]
fn an_unknown_answer_then_a_list_showing_the_machine_makes_it_booting() {
    let (mut rig, plan) = launched(settings("gpu"));
    rig.input.acquired = vec![answered(&plan, Acquired::Unknown)];
    assert_eq!(kinds(&rig.tick(at(SECOND))), ["Save"]);
    assert_eq!(rig.phase(0), Phase::Unresolved);
    assert!(rig.tick(at(2 * SECOND)).is_empty());

    rig.pool().held = Some(vec![listed(&plan, "i-9", HostStatus::Booting)]);
    let actions = rig.tick(at(3 * SECOND));
    assert_eq!(kinds(&actions), ["Save"]);
    assert_eq!(rig.phase(0), Phase::Booting);
    assert_eq!(rig.record(0).cloud_id.as_deref(), Some("i-9"));
}

/// Asserts the hold on an acquire whose machine is not listed: nothing happens a
/// millisecond before `unknown_hold`; at it the record fails, its token is revoked and
/// its network key forgotten, and it still counts against a cap of one machine; the next
/// clean list a millisecond short of `unknown_hold` after the first changes nothing; the
/// one at `unknown_hold` settles it, and only then may the request have a machine.
#[test]
fn a_machine_never_listed_fails_after_the_hold_and_counts_until_two_clean_lists() {
    let (mut rig, _) = launched(one_machine(settings("gpu")));
    assert!(rig.tick(at(30 * MINUTE - 1)).is_empty());
    assert_eq!(rig.phase(0), Phase::Launching);

    let actions = rig.tick(at(30 * MINUTE));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "ForgetNetwork"]);
    assert_eq!(rig.phase(0), Phase::Failed);
    assert_eq!(rig.record(0).clean_lists, 1);
    assert_eq!(rig.state.blocked["gpu"], Blocked::MaxInstances);

    assert!(rig.tick(at(60 * MINUTE - 1)).is_empty());
    assert_eq!(rig.record(0).clean_lists, 1);

    let actions = rig.tick(at(60 * MINUTE));
    assert_eq!(kinds(&actions), ["Save", "Mint", "Acquire"]);
    assert_eq!(rig.record(0).clean_lists, 2);
    assert_eq!(rig.state.records.instances.len(), 2);

    assert!(rig.tick(at(61 * MINUTE)).is_empty());
}

/// Asserts that an acquire that got `Unknown` is given up on `unknown_hold` after the
/// answer, and not after the launch.
#[test]
fn an_unresolved_acquire_fails_after_the_hold_counted_from_the_answer() {
    let (mut rig, plan) = launched(settings("gpu"));
    rig.input.acquired = vec![answered(&plan, Acquired::Unknown)];
    rig.tick(at(MINUTE));
    rig.tick(at(MINUTE + 30 * MINUTE - 1));
    assert_eq!(rig.phase(0), Phase::Unresolved);
    rig.tick(at(MINUTE + 30 * MINUTE));
    assert_eq!(rig.phase(0), Phase::Failed);
}

/// Asserts that a machine listed after its acquire failed is released and never
/// uncounted until a read by id says it is gone: its token is revoked on the way into
/// release too (a restart may have lost the revoke made when it failed; a second revoke
/// of a missing token is a no-op), it holds the cap of one machine throughout, and the
/// request gets a machine only once it is confirmed gone.
#[test]
fn a_machine_listed_after_failing_is_released_and_stays_counted() {
    let (mut rig, plan) = launched(one_machine(settings("gpu")));
    rig.tick(at(30 * MINUTE));
    assert_eq!(rig.phase(0), Phase::Failed);

    rig.pool().held = Some(vec![listed(&plan, "i-late", HostStatus::Active)]);
    let actions = rig.tick(at(31 * MINUTE));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(
        actions[2],
        Action::Release {
            host: plan.name.clone(),
            id: HostRef("i-late".to_owned())
        }
    );
    assert_eq!(rig.phase(0), Phase::Releasing);

    rig.pool().held = Some(Vec::new());
    assert_eq!(count(&rig.tick(at(32 * MINUTE)), "Acquire"), 0);
    assert_eq!(rig.state.blocked["gpu"], Blocked::MaxInstances);

    rig.input.released = vec![ReleaseResult {
        host: plan.name.clone(),
        answer: Released::Gone,
    }];
    let actions = rig.tick(at(33 * MINUTE));
    assert_eq!(count(&actions, "RemoveHost"), 1);
    assert_eq!(count(&actions, "Acquire"), 1);
}

/// Asserts that an answer that arrives for an acquire already failed is an instance
/// found late: it is released, not booted.
#[test]
fn a_late_acceptance_of_a_failed_acquire_is_released() {
    let (mut rig, plan) = launched(settings("gpu"));
    rig.tick(at(30 * MINUTE));
    rig.input.acquired = vec![accepted(&plan, "i-late")];
    let actions = rig.tick(at(31 * MINUTE));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert_eq!(rig.record(0).cloud_id.as_deref(), Some("i-late"));
}

/// Asserts that `Capacity` refuses the offer for this step: the request is launched at
/// the next cheapest offer in the same step, the refused record is gone with the step, and
/// its token and network key are cleaned up.
#[test]
fn a_capacity_refusal_tries_the_next_offer() {
    let (mut rig, plan) = launched(settings("gpu"));
    rig.pool().offers = vec![offer("a10", 60), offer("b", 90)];
    rig.input.acquired = vec![answered(&plan, refusal(RefusalCode::Capacity))];
    let actions = rig.tick(at(SECOND));
    assert_eq!(
        kinds(&actions),
        ["Save", "Revoke", "ForgetNetwork", "Mint", "Acquire"]
    );
    assert_eq!(rig.state.records.instances.len(), 1);
    assert_eq!(rig.phase(0), Phase::Launching);
    assert_eq!(acquires(&actions)[0].offer.id, "b");
    assert!(rig.state.blocked.is_empty());
}

/// Asserts that a pool whose only offer is refused for capacity over and over keeps no
/// refused record and mints no token in the step that learns of the refusal: the records
/// hold the one launch in flight and no more, after a thousand steps.
#[test]
fn a_pool_refused_for_capacity_over_and_over_keeps_its_records_bounded() {
    let (mut rig, mut plan) = launched(settings("gpu"));
    let mut launches = 1;
    for round in 1..=1000 {
        rig.input.acquired = vec![answered(&plan, refusal(RefusalCode::Capacity))];
        let refusal_step = rig.tick(at((2 * round - 1) * SECOND));
        assert_eq!(kinds(&refusal_step), ["Save", "Revoke", "ForgetNetwork"]);
        assert!(rig.state.records.instances.is_empty());

        let retry = rig.tick(at(2 * round * SECOND));
        plan = acquires(&retry).remove(0);
        launches += 1;
        assert_eq!(rig.state.records.instances.len(), 1);
    }
    assert_eq!(launches, 1001);
}

/// Asserts that a quota, a denial and a shape refusal each block the pool, with their own
/// reason, until `boot_fail_cooldown` has run, and that the pool launches again at the
/// instant it ends and not a millisecond before.
#[test]
fn quota_denied_and_shape_block_the_pool_for_the_cooldown() {
    let cases = [
        (RefusalCode::Quota, Blocked::Quota),
        (RefusalCode::Denied, Blocked::Denied),
        (RefusalCode::Shape, Blocked::Shape),
    ];
    for (code, expected) in cases {
        let (mut rig, plan) = launched(settings("gpu"));
        rig.input.acquired = vec![answered(&plan, refusal(code))];
        let first = 5 * SECOND;
        let actions = rig.tick(at(first));
        assert_eq!(kinds(&actions), ["Save", "Revoke", "ForgetNetwork"]);
        assert_eq!(rig.state.blocked["gpu"], expected);

        let actions = rig.tick(at(first + 10 * MINUTE - 1));
        assert_eq!(count(&actions, "Acquire"), 0);
        assert_eq!(rig.state.blocked["gpu"], expected);

        let actions = rig.tick(at(first + 10 * MINUTE));
        assert_eq!(count(&actions, "Acquire"), 1);
        assert!(rig.state.blocked.is_empty());
    }
}

/// Asserts that a launch the provider told to wait stays one machine of the request, is
/// not sent before its time, and then goes out with a new token for the same acquire id,
/// at the instant its time comes.
#[test]
fn a_deferred_acquire_is_retried_with_the_same_id_when_its_time_comes() {
    let (mut rig, plan) = launched(settings("gpu"));
    rig.input.acquired = vec![answered(
        &plan,
        Acquired::Deferred {
            wait: std::time::Duration::from_secs(12),
        },
    )];
    assert_eq!(kinds(&rig.tick(at(SECOND))), ["Save"]);
    assert_eq!(rig.phase(0), Phase::Planned);
    assert_eq!(rig.record(0).retry_at, Some(at(13 * SECOND)));

    assert!(rig.tick(at(13 * SECOND - 1)).is_empty());
    let actions = rig.tick(at(13 * SECOND));
    assert_eq!(kinds(&actions), ["Save", "Mint", "Acquire"]);
    assert_eq!(acquires(&actions)[0].acquire, plan.acquire);
    assert_eq!(acquires(&actions)[0].name, plan.name);
    assert_eq!(rig.phase(0), Phase::Launching);
    assert_eq!(rig.record(0).launched_at, at(13 * SECOND));
    assert_eq!(rig.record(0).retry_at, None);
    assert_eq!(rig.state.records.instances.len(), 1);
}

/// Asserts that a retry takes the step's one launch, so a second request waits for the
/// next step; and that a pool whose checks fail does not retry.
#[test]
fn a_retry_uses_the_step_and_waits_for_a_healthy_pool() {
    let (mut rig, plan) = launched(settings("gpu"));
    rig.input.acquired = vec![answered(
        &plan,
        Acquired::Deferred {
            wait: std::time::Duration::from_secs(12),
        },
    )];
    rig.tick(at(SECOND));

    rig.pool().health = Health::Failing;
    assert!(rig.tick(at(13 * SECOND)).is_empty());
    assert_eq!(rig.phase(0), Phase::Planned);

    rig.pool().health = Health::Available;
    rig.input.waiting = vec![request("r1"), request("r2")];
    let actions = rig.tick(at(14 * SECOND));
    assert_eq!(acquires(&actions).len(), 1);
    assert_eq!(acquires(&actions)[0].acquire, plan.acquire);
    assert_eq!(acquires(&rig.tick(at(15 * SECOND))).len(), 1);
    assert_eq!(rig.state.records.instances.len(), 2);
}

/// Asserts that an answer the state machine has no transition for is refused and counted
/// and changes nothing: a repeated acceptance, an unknown, a deferral and a refusal for
/// a booting machine, a gone release for a machine not being released, and a refusal
/// for an unresolved one. Answers that match no record are ignored and not counted.
#[test]
fn an_answer_with_no_transition_is_counted_and_changes_nothing() {
    let (mut rig, plan, later) = booting(settings("gpu"), "i-1");
    let before = rig.state.records.clone();
    rig.input.acquired = vec![
        accepted(&plan, "i-1"),
        answered(&plan, Acquired::Unknown),
        answered(
            &plan,
            Acquired::Deferred {
                wait: std::time::Duration::from_secs(1),
            },
        ),
        answered(&plan, refusal(RefusalCode::Capacity)),
        answered(
            &AcquirePlan {
                acquire: stranger(),
                ..plan.clone()
            },
            Acquired::Unknown,
        ),
    ];
    rig.input.released = vec![
        ReleaseResult {
            host: plan.name.clone(),
            answer: Released::Gone,
        },
        ReleaseResult {
            host: "nobody".to_owned(),
            answer: Released::Gone,
        },
    ];
    assert!(rig.tick(at(later + SECOND)).is_empty());
    assert_eq!(rig.state.refused, 5);
    assert_eq!(rig.state.records, before);

    let (mut other, plan) = launched(settings("gpu"));
    other.input.acquired = vec![answered(&plan, Acquired::Unknown)];
    other.tick(at(SECOND));
    other.input.acquired = vec![answered(&plan, refusal(RefusalCode::Quota))];
    other.tick(at(2 * SECOND));
    assert_eq!(other.state.refused, 1);
    assert_eq!(other.phase(0), Phase::Unresolved);
}

fn stray(id: &str, acquire: &str, status: HostStatus) -> Held {
    Held {
        id: HostRef(id.to_owned()),
        acquire: acquire.to_owned(),
        name: Some(format!("stray-{id}")),
        offer: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        status,
    }
}

/// Asserts that machines in the list that no record knows become orphan records, with an
/// acquire id only when their marks carry a usable one; that an orphan is reported and
/// never released; that a list that stops showing it does not make it gone, and a machine
/// listed terminated does; and that a listed machine already terminated is not adopted.
#[test]
fn unknown_machines_are_adopted_as_orphans_and_never_released() {
    let mut rig = Rig::new(settings("gpu"));
    rig.pool().held = Some(vec![
        stray("i-a", "", HostStatus::Active),
        stray("i-b", "old-acquire-1", HostStatus::Unhealthy),
        stray("i-dead", "", HostStatus::Terminated),
    ]);
    let actions = rig.tick(at(0));
    assert_eq!(kinds(&actions), ["Save"]);
    assert_eq!(rig.state.records.instances.len(), 2);
    assert_eq!(rig.phase(0), Phase::Orphan);
    assert_eq!(rig.record(0).acquire, None);
    assert_eq!(rig.record(0).price_cents_per_hour, 60);
    assert_eq!(rig.record(0).cloud_id.as_deref(), Some("i-a"));
    assert_eq!(
        rig.record(1).acquire.as_ref().map(|id| id.as_str()),
        Some("old-acquire-1")
    );

    for step_at in [5, 10, 15] {
        assert!(rig.tick(at(step_at * SECOND)).is_empty());
    }

    rig.pool().held = Some(vec![stray("i-b", "old-acquire-1", HostStatus::Terminated)]);
    let actions = rig.tick(at(20 * SECOND));
    assert_eq!(kinds(&actions), ["Save"]);
    assert_eq!(rig.phase(0), Phase::Orphan);
    assert_eq!(rig.record(0).clean_lists, 1);
    assert!(rig.record(0).counts());
    assert_eq!(rig.phase(1), Phase::Gone);
    assert_eq!(rig.record(1).gone_at, Some(at(20 * SECOND)));
    assert_eq!(count(&actions, "Release"), 0);
}

/// Asserts that a failed acquire whose machine shows up after a restart from the records
/// (by a list, or by a late acceptance) has its join token revoked when it goes into
/// release, before the release, so the revoke made when it failed cannot have been lost.
#[test]
fn a_failed_record_restarted_from_records_is_revoked_then_released() {
    let (mut rig, plan) = launched(one_machine(settings("gpu")));
    rig.tick(at(30 * MINUTE));
    assert_eq!(rig.phase(0), Phase::Failed);
    let text = rig.state.records.encode().unwrap();

    for late in [false, true] {
        let mut fresh = Rig::new(one_machine(settings("gpu")));
        fresh.state = State::new(Records::decode(&text).unwrap());
        if late {
            fresh.input.acquired = vec![accepted(&plan, "i-late")];
        } else {
            fresh.pool().held = Some(vec![listed(&plan, "i-late", HostStatus::Active)]);
        }
        let actions = fresh.tick(at(31 * MINUTE));
        assert_eq!(
            kinds(&actions),
            ["Save", "Revoke", "Release"],
            "late {late}"
        );
        assert_eq!(fresh.phase(0), Phase::Releasing);
        assert_eq!(fresh.state.refused, 0);
    }
}
