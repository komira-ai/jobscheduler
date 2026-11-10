//! A pool with more than one machine provider, driven through `step` with the providers'
//! answers handed in by hand: pass 2 walks the providers in the pool's order (design 5.6,
//! step 5), each inside its own `provider_limits` and share; a refusal is failed over to the
//! next provider and charged to the provider that gave it; a request every provider refused
//! waits `acquire_retry` and then asks the first provider again; each record names the
//! provider that launched it, and a retry, a hello's verdict and a kind's total go by that
//! provider. Time is a `Millis` the tests name; nothing sleeps or reads a clock.

mod common;

use std::time::Duration;

use common::{
    MINUTE, Rig, SECOND, accepted, acquires, answered, at, demand, host, listed, offer, record,
    request, settings, shape,
};
use js_elastic::{Acquired, HostStatus, Instant, Offer, Reason, Refusal, RefusalCode};
use js_scaler::{
    AcquirePlan, AcquireResult, Blocked, Budgets, Millis, Phase, ProviderSettings, Records,
    Settings, Totals, launchable,
};
use js_sched::failover::{PassedOver, Refused};
use js_types::{ProviderId, RequestId};

/// The pool's first provider.
const FIRST: ProviderId = ProviderId::known("first");

/// The provider after it.
const NEXT: ProviderId = ProviderId::known("next");

/// The settings of the pool "gpu" whose providers are `first` (kind `x`) and then `next`
/// (kind `y`), with no shares and no limits.
fn two() -> Settings {
    Settings {
        provider: FIRST,
        kind: "x".to_owned(),
        later: vec![ProviderSettings::new(NEXT, "y")],
        ..settings("gpu")
    }
}

/// A rig of the pool `limits`, whose first provider offers `first` and whose next offers
/// `next`, with the request `r1` waiting.
fn rig_of(limits: Settings, first: Vec<Offer>, next: Vec<Offer>) -> Rig {
    let mut rig = Rig::new(limits);
    rig.pool().offers = first;
    rig.pool().later_offers = [(NEXT, next)].into();
    rig.input.waiting = vec![request("r1")];
    rig
}

/// A refusal with `code`.
fn refusal(code: RefusalCode) -> Acquired {
    Acquired::Refused(Refusal {
        code,
        reason: Reason::DryRun,
    })
}

/// One step at `now` that reads `answers` first. Returns the acquires it makes.
fn step(rig: &mut Rig, now: Millis, answers: Vec<AcquireResult>) -> Vec<AcquirePlan> {
    rig.input.acquired = answers;
    acquires(&rig.tick(now))
}

/// The one acquire among `plans`, as (provider, offer id).
fn only(mut plans: Vec<AcquirePlan>) -> (ProviderId, String, AcquirePlan) {
    assert_eq!(plans.len(), 1, "{plans:?}");
    let plan = plans.remove(0);
    (plan.provider.clone(), plan.offer.id.clone(), plan)
}

/// The refusal for capacity of `offer` (in zone z1) by `provider`, for `r1`.
fn declined(provider: ProviderId, offer: &str) -> Refused {
    Refused {
        provider,
        offer: offer.to_owned(),
        zone: Some("z1".to_owned()),
        code: RefusalCode::Capacity,
        passed_over: None,
        request: RequestId::new("r1").ok(),
    }
}

/// The providers the records name, in order.
fn named(rig: &Rig) -> Vec<Option<String>> {
    let records = rig.state.records.instances.iter();
    records.map(|record| record.provider.clone()).collect()
}

/// Asserts the walk across providers: the first provider is asked first although the next
/// offers a cheaper machine (provider order beats price); its `capacity` refusal falls
/// through to the next provider within the same attempt, charged to the provider that gave
/// it; the next provider's refusal ends the attempt, which asks nothing more; no step cools
/// the pool, strikes its brake or shows a block; and once `acquire_retry` has passed from
/// the last refusal, and not a millisecond before, the first provider is asked again and
/// its machine is launched, the record naming it. Catches a walk that stops at the first
/// provider, one that orders the providers by price, a refusal charged to the pool's first
/// provider whatever gave it, and a provider that is never asked again after a refusal.
#[test]
fn a_capacity_refusal_falls_through_to_the_next_provider_and_the_first_is_asked_again() {
    let mut rig = rig_of(two(), vec![offer("a", 90)], vec![offer("b", 40)]);
    let (provider, id, first) = only(step(&mut rig, at(0), vec![]));
    assert_eq!((provider, id.as_str()), (FIRST, "a"));

    let refused = answered(&first, refusal(RefusalCode::Capacity));
    let (provider, id, next) = only(step(&mut rig, at(SECOND), vec![refused]));
    assert_eq!((provider, id.as_str()), (NEXT, "b"));
    assert_eq!(rig.state.refusals, [declined(FIRST, "a")]);
    assert!(rig.state.blocked.is_empty(), "{:?}", rig.state.blocked);
    assert!(rig.state.records.cooldowns.is_empty());
    assert!(rig.state.records.brake.is_empty());

    let refused = answered(&next, refusal(RefusalCode::Capacity));
    assert!(step(&mut rig, at(2 * SECOND), vec![refused]).is_empty());
    assert_eq!(rig.state.refusals, [declined(NEXT, "b")]);
    assert!(rig.state.records.cooldowns.is_empty());
    assert!(rig.state.records.brake.is_empty());

    assert!(step(&mut rig, at(62 * SECOND - 1), vec![]).is_empty());
    let (provider, id, again) = only(step(&mut rig, at(62 * SECOND), vec![]));
    assert_eq!((provider, id.as_str()), (FIRST, "a"));
    step(&mut rig, at(63 * SECOND), vec![accepted(&again, "i-1")]);
    let booting = rig.state.records.instances.iter();
    let booting: Vec<_> = booting.filter(|r| r.phase == Phase::Booting).collect();
    assert_eq!(booting.len(), 1);
    assert_eq!(booting[0].provider.as_deref(), Some("first"));
}

/// Asserts that a provider holding its share is passed over for the next one: with the
/// first provider's share at one, the second request is launched by the next provider, each
/// record names its provider, and once the next provider holds its share of one too the
/// third request is launched by neither and the pool shows `MaxInstances`. A record that
/// names no provider (a file written before the pool had several) is the first provider's,
/// so it fills the first's share. Catches a share counted over the whole pool, and a
/// record of no provider counted against no provider's share.
#[test]
fn a_provider_holding_its_share_is_passed_over_for_the_next() {
    let shared = Settings {
        provider_share: Some(1),
        later: vec![ProviderSettings {
            share: Some(1),
            ..ProviderSettings::new(NEXT, "y")
        }],
        ..two()
    };
    let mut rig = rig_of(shared.clone(), vec![offer("a", 60)], vec![offer("b", 60)]);
    rig.input.waiting = vec![request("r1"), request("r2"), request("r3")];
    rig.pool().settings.max_step = 3;
    let plans = step(&mut rig, at(0), vec![]);
    let asked: Vec<_> = plans.iter().map(|plan| plan.provider.clone()).collect();
    assert_eq!(asked, [FIRST, NEXT]);
    assert_eq!(named(&rig), [Some("first".into()), Some("next".into())]);
    assert_eq!(rig.state.blocked["gpu"], Blocked::MaxInstances);

    let mut old = rig_of(shared, vec![offer("a", 60)], vec![offer("b", 60)]);
    old.state.records.instances = vec![record(Phase::Booting, 60, 0)];
    old.input.waiting = vec![request("r1"), request("r2")];
    let (provider, id, _) = only(step(&mut old, at(SECOND), vec![]));
    assert_eq!((provider, id.as_str()), (NEXT, "b"));
}

/// Asserts that each provider launches only inside its own `provider_limits`: the first may
/// launch only the type `a` and offers only `big`, so it is passed over; the next may pay at
/// most 50 cents and offers `b` at 60 and `c` at 50, so `c` is launched although `b` is the
/// first provider's allowed price. A next provider whose only serving offer has no price
/// launches nothing and the pool shows `Unpriced`. Catches the first provider's limits
/// applied to every provider, and a provider's limits that drop an unpriced offer before the
/// pool can show it.
#[test]
fn each_provider_launches_only_inside_its_own_limits() {
    let limited = Settings {
        allowed_instance_types: Some(vec!["a".to_owned()]),
        later: vec![ProviderSettings {
            max_price_cents_per_hour: Some(50),
            ..ProviderSettings::new(NEXT, "y")
        }],
        ..two()
    };
    let first = vec![offer("big", 10)];
    let mut rig = rig_of(
        limited.clone(),
        first.clone(),
        vec![offer("b", 60), offer("c", 50)],
    );
    let (provider, id, _) = only(step(&mut rig, at(0), vec![]));
    assert_eq!((provider, id.as_str()), (NEXT, "c"));

    let mut unpriced = rig_of(limited, first, vec![offer("b", 60), offer("free", 0)]);
    assert!(step(&mut unpriced, at(0), vec![]).is_empty());
    assert_eq!(unpriced.state.blocked["gpu"], Blocked::Unpriced);
}

/// Asserts that a record names its provider only when its pool has several, and that the
/// records file holds the name then and has no `provider` key otherwise: a pool of one
/// provider writes the records the release before this one reads, and a record naming its
/// provider reads back whole. Catches a provider written into the file of every pool, which
/// that release would refuse, and a name lost on the way back from the file.
#[test]
fn a_record_names_its_provider_only_when_its_pool_has_several() {
    let mut one = Rig::new(settings("gpu"));
    one.input.waiting = vec![request("r1")];
    let (provider, _, _) = only(step(&mut one, at(0), vec![]));
    assert_eq!(provider, js_scaler::UNNAMED_PROVIDER);
    assert_eq!(named(&one), [None]);
    let text = one.state.records.encode().unwrap();
    assert!(!text.contains("\"provider\""), "{text}");

    let mut several = rig_of(two(), Vec::new(), vec![offer("b", 60)]);
    step(&mut several, at(0), vec![]);
    assert_eq!(named(&several), [Some("next".into())]);
    let text = several.state.records.encode().unwrap();
    assert!(text.contains("\"provider\": \"next\""), "{text}");
    assert_eq!(Records::decode(&text).unwrap(), several.state.records);
}

/// Asserts that a `quota` refusal passes over the instance type of the provider that gave
/// it, for that provider's own `quota_backoff` (ten minutes, beside the first provider's
/// hour), and that the walk goes on to the first provider's other offer meanwhile. Catches
/// a pass-over keyed to the pool's first provider and one timed by the first provider's
/// backoff.
#[test]
fn a_quota_refusal_passes_over_the_type_of_the_provider_that_gave_it() {
    let limits = Settings {
        later: vec![ProviderSettings {
            quota_backoff: Duration::from_secs(600),
            ..ProviderSettings::new(NEXT, "y")
        }],
        ..two()
    };
    let mut rig = rig_of(limits, Vec::new(), vec![offer("b", 60), offer("c", 70)]);
    let (provider, id, plan) = only(step(&mut rig, at(0), vec![]));
    assert_eq!((provider, id.as_str()), (NEXT, "b"));
    let refused = answered(&plan, refusal(RefusalCode::Quota));
    let (_, id, _) = only(step(&mut rig, at(SECOND), vec![refused]));
    assert_eq!(id, "c");
    let over = PassedOver {
        provider: NEXT,
        instance_type: "b".to_owned(),
        region: Some("z1".to_owned()),
    };
    let until = Instant::from_millis(at(SECOND + 10 * MINUTE).as_millis());
    assert_eq!(rig.state.refusals[0].passed_over, Some((over, until)));
}

/// Asserts that the hello of a machine the next provider launched is judged with that
/// provider's `size_tolerance_percent` (none, so a host one millicore short of the offer
/// does not match where the first provider's ten percent would let it) and that its offer is
/// cooled down under that provider for that provider's `offer_cooldown` (two hours). Catches
/// a verdict and a cool-down that take the pool's first provider's settings.
#[test]
fn a_mismatched_hello_cools_the_offer_of_the_provider_that_launched_it() {
    let limits = Settings {
        later: vec![ProviderSettings {
            size_tolerance_percent: 0,
            offer_cooldown: Duration::from_secs(7200),
            ..ProviderSettings::new(NEXT, "y")
        }],
        ..two()
    };
    let mut rig = rig_of(limits, Vec::new(), vec![offer("a10", 60)]);
    let (_, _, plan) = only(step(&mut rig, at(0), vec![]));
    rig.pool().held = Some(vec![listed(&plan, "i-1", HostStatus::Booting)]);
    rig.input.waiting = Vec::new();
    step(&mut rig, at(SECOND), vec![accepted(&plan, "i-1")]);
    let mut announced = host(&plan.name, "gpu");
    shape(&mut announced).cpu_millis = 7999;
    rig.input.hosts = vec![announced];
    rig.tick(at(2 * SECOND));
    assert!(rig.record(0).condemned);
    let cooled = &rig.state.records.cooled_offers;
    assert_eq!(cooled.len(), 1);
    assert_eq!(cooled[0].provider, "next");
    assert_eq!(cooled[0].until, at(2 * SECOND + 2 * 60 * MINUTE));
}

/// Asserts that a launch the next provider told to wait is retried with the next provider
/// when its time comes, as the next call of the same acquire. Catches a retry sent to the
/// pool's first provider, which never heard of the acquire.
#[test]
fn a_launch_told_to_wait_is_retried_with_the_provider_its_record_names() {
    let mut rig = rig_of(two(), Vec::new(), vec![offer("b", 60)]);
    let (_, _, plan) = only(step(&mut rig, at(0), vec![]));
    let wait = Acquired::Deferred {
        wait: Duration::from_secs(1),
    };
    assert!(step(&mut rig, at(SECOND), vec![answered(&plan, wait)]).is_empty());
    let (provider, _, retry) = only(step(&mut rig, at(3 * SECOND), vec![]));
    assert_eq!(provider, NEXT);
    assert_eq!((retry.acquire, retry.attempt), (plan.acquire, 1));
}

/// Asserts the kind totals of a pool of two kinds: a launch by the next provider (kind `y`)
/// holds `y`'s total, which blocks it at 14 cents (a launch's least cost is 15) and not
/// at 15, while a total of 14 on `x`, the first provider's kind, does not block it; the
/// pool's machines count in the totals of both its kinds, so 15 cents spent by the first
/// provider block the next one's launch under `y`'s 29; and a pool of two kinds forgets the
/// kind it had in the records, so once it leaves the file its records count in every kind.
/// Catches a launch checked against the first provider's kind, a pool counted in one of its
/// kinds only, and a remembered kind that lets a removed pool's spend escape a total.
#[test]
fn a_launch_holds_the_total_of_its_providers_kind() {
    let month = |kind: &str, total| Budgets {
        kinds: [(
            kind.to_owned(),
            Totals {
                per_month: Some(total),
                ..Totals::default()
            },
        )]
        .into(),
        ..Budgets::default()
    };
    let cases = [("y", 14, 0), ("y", 15, 1), ("x", 14, 1)];
    for (kind, total, launched) in cases {
        let mut rig = rig_of(two(), Vec::new(), vec![offer("b", 60)]);
        rig.state.records.pool_kinds = [("gpu".to_owned(), "x".to_owned())].into();
        rig.input.budgets = month(kind, total);
        let plans = step(&mut rig, at(0), vec![]);
        assert_eq!(plans.len(), launched, "{kind} {total}");
        if launched == 1 {
            assert!(rig.state.records.pool_kinds.is_empty());
        }
    }

    let mut spent = rig_of(two(), vec![offer("a", 60)], vec![offer("b", 60)]);
    spent.input.budgets = month("y", 29);
    spent.input.waiting = vec![request("r1"), request("r2")];
    spent.pool().settings.provider_share = Some(1);
    spent.pool().settings.max_step = 2;
    let plans = step(&mut spent, at(0), vec![]);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].provider, FIRST);
    assert_eq!(spent.state.blocked["gpu"], Blocked::MaxSpendTotal);
}

/// Asserts that a request is launchable when only the next provider has an offer that
/// fits, and not once that provider holds its share. Catches a `launchable` answer that
/// asks the first provider alone.
#[test]
fn a_request_is_launchable_by_a_later_provider() {
    let mut rig = rig_of(two(), Vec::new(), vec![offer("b", 60)]);
    assert!(launchable(&rig.state, &rig.input, at(0), &demand()));
    rig.pool().settings.later[0].share = Some(1);
    let mut held = record(Phase::Booting, 60, 0);
    held.provider = Some("next".to_owned());
    rig.state.records.instances = vec![held];
    assert!(!launchable(&rig.state, &rig.input, at(0), &demand()));
}

/// Asserts that a machine the next provider launched from an offer that states an ISA level
/// still covers its request once the record is read back without the level (the records
/// file never keeps it): the level is taken from the next provider's catalog, so no second
/// machine is launched. Catches the level looked up in the first provider's offers, which do
/// not list the machine's offer, and a paid machine launched again after every restart.
#[test]
fn a_later_providers_machine_on_its_way_keeps_covering_its_request_after_a_restart() {
    let level: js_types::IsaLevel = "x86-64-v3".parse().unwrap();
    let stated = Offer {
        isa_level: Some(level),
        ..offer("b", 60)
    };
    let mut rig = rig_of(two(), Vec::new(), vec![stated]);
    let mut wanting = request("r1");
    wanting.demand.isa_level = Some(level);
    rig.input.waiting = vec![wanting];
    let (provider, _, _) = only(step(&mut rig, at(0), vec![]));
    assert_eq!(provider, NEXT);
    rig.state.records.instances[0].offer.isa_level = None;
    for later in [5, 10, 15] {
        assert!(step(&mut rig, at(later * SECOND), vec![]).is_empty());
    }
    assert_eq!(rig.state.records.instances.len(), 1);
}

/// Asserts that a machine of ours that the next provider lists and no record knows is an
/// orphan of the next provider: priced at the next provider's offer (40 cents, where the
/// first provider offers the same type at 90 and the fallback is the hourly cap), shaped
/// from it, and named on the record, so it fills the next provider's share and not the
/// first's. Catches an orphan priced and counted as the first provider's.
#[test]
fn an_orphan_a_later_provider_lists_is_that_providers() {
    let shared = Settings {
        provider_share: Some(1),
        later: vec![ProviderSettings {
            share: Some(1),
            ..ProviderSettings::new(NEXT, "y")
        }],
        ..two()
    };
    let mut rig = rig_of(shared, vec![offer("b", 90)], vec![offer("b", 40)]);
    let stray = js_elastic::Held {
        id: js_elastic::HostRef("i-stray".to_owned()),
        acquire: "someone-else".to_owned(),
        name: None,
        offer: "b".to_owned(),
        zone: Some("z1".to_owned()),
        status: HostStatus::Active,
        ends_unix_seconds: None,
    };
    rig.pool().held = Some(vec![stray]);
    rig.pool().listed_by = [("i-stray".to_owned(), NEXT)].into();
    let (provider, _, _) = only(step(&mut rig, at(0), vec![]));
    assert_eq!(provider, FIRST);
    let orphan = &rig.state.records.instances[0];
    assert_eq!(orphan.phase, Phase::Orphan);
    assert_eq!(orphan.provider.as_deref(), Some("next"));
    assert_eq!(orphan.price_cents_per_hour, 40);
    assert!(orphan.offer.is_recorded());
}

/// Asserts that the hint of what a request misses by (`no_offer`) reads the later
/// providers' offers too: with the first provider offering nothing and the next only a
/// machine of another operating system, the request is named as missing by its operating
/// system. Catches a hint over the first provider's offers alone, which would name no miss.
#[test]
fn the_nearest_miss_reads_the_later_providers_offers() {
    let other_os = Offer {
        os: "macos".to_owned(),
        ..offer("m", 60)
    };
    let mut rig = rig_of(two(), Vec::new(), vec![other_os]);
    assert!(step(&mut rig, at(0), vec![]).is_empty());
    let r1 = RequestId::new("r1").unwrap();
    assert_eq!(
        rig.state.no_offer.get(&r1).copied(),
        Some(js_elastic::Dimension::Os)
    );
}
