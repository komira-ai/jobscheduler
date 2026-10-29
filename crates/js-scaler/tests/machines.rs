//! The machines and ids the scaler keeps: demand across pools, acquire ids and join
//! tokens, orphans, the extras of an accepted launch, what a step saves, and the instant
//! type.

mod common;

use std::time::Duration;

use common::*;
use js_elastic::{
    Acquired, Extra, Held, HostRef, HostStatus, Offer, Reason, Refusal, RefusalCode, Released,
};
use js_scaler::{Action, Blocked, Millis, Phase, Settings, State, step};

fn launched(limits: Settings) -> (Rig, js_scaler::AcquirePlan) {
    let mut rig = Rig::new(limits);
    rig.input.waiting = vec![request("r1")];
    let plan = acquires(&rig.tick(at(0))).remove(0);
    (rig, plan)
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
