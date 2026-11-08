//! Failover of pass 2 (`js_sched::failover::AcquireMemory`): what the walk asks next after a
//! provider refuses an acquire, driven by a loop that plays the scaler's pass 2 over
//! scripted answers and an injected clock.

use std::time::Duration;

use js_elastic::{
    AcquireId, Acquired, Demand, Held, HostRef, HostStatus, Instant, Offer, PriceUnit, Reason,
    Refusal, RefusalCode,
};
use js_sched::acquire::{AcquireInput, ProviderView, Skip, SkipReason, Waiting};
use js_sched::failover::{AcquireMemory, Next, PassedOver, Refused};
use js_types::{ExecMode, ProviderId, RequestId, Resources};

const GIB: u64 = 1 << 30;
const BACKOFF: Duration = Duration::from_secs(3600);
const RETRY: Duration = Duration::from_secs(60);

/// A demand for four cores and 16 GiB, with no accelerator.
fn demand() -> Demand {
    Demand {
        resources: Resources::new(4000, 16 * GIB, 0, None).unwrap(),
        disk_bytes: None,
        os: None,
        arch: None,
        isa_level: None,
        exec: None,
        accelerator: None,
        network: Default::default(),
        host_ports: Vec::new(),
    }
}

/// An available Linux offer `id` in `zone` (none when `zone` is `None`) of eight cores and
/// 32 GiB, at `price` cents an hour.
fn offer(id: &str, zone: Option<&str>, price: Option<u64>) -> Offer {
    Offer {
        id: id.to_owned(),
        zone: zone.map(str::to_owned),
        cpu_millis: 8000,
        memory_bytes: 32 * GIB,
        disk_bytes: 100 * GIB,
        accelerator: None,
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: price,
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    }
}

fn id(name: &str) -> ProviderId {
    ProviderId::new(name).unwrap()
}

fn request(name: &str) -> RequestId {
    RequestId::new(name).unwrap()
}

fn acquire_id(n: u32) -> AcquireId {
    AcquireId::new(&format!("acq-{n}")).unwrap()
}

fn ms(millis: u64) -> Instant {
    Instant::from_millis(millis)
}

/// The provider `name` with `offers`, enabled and holding nothing.
fn provider<'a>(name: &str, offers: &'a [Offer]) -> ProviderView<'a> {
    ProviderView {
        id: id(name),
        enabled: true,
        cooling_down: false,
        braked: false,
        max_machines: None,
        machines: 0,
        purchase: &[],
        offers,
    }
}

fn input<'a>(job: &'a Demand, providers: &'a [ProviderView<'a>]) -> AcquireInput<'a> {
    AcquireInput {
        demand: job,
        allowed_instance_types: None,
        max_price_cents_per_hour: None,
        max_machines: None,
        machines: 0,
        providers,
    }
}

fn refuse(code: RefusalCode) -> Acquired {
    Acquired::Refused(Refusal {
        code,
        reason: Reason::Api("scripted".to_owned()),
    })
}

fn capacity() -> Acquired {
    refuse(RefusalCode::Capacity)
}

fn quota() -> Acquired {
    refuse(RefusalCode::Quota)
}

fn accepted() -> Acquired {
    Acquired::Accepted {
        held: Held {
            id: HostRef("i-1".to_owned()),
            acquire: "acq".to_owned(),
            name: None,
            offer: "x".to_owned(),
            zone: None,
            status: HostStatus::Booting,
            ends_unix_seconds: None,
        },
        extras: Vec::new(),
    }
}

/// One offer asked: provider id, offer id, zone.
type Asked = (String, String, Option<String>);

fn asked(provider: &str, offer: &str, zone: Option<&str>) -> Asked {
    (
        provider.to_owned(),
        offer.to_owned(),
        zone.map(str::to_owned),
    )
}

/// What a driven request asked, what was refused, and the `Next` that ended it (`None`
/// when an acquire was accepted).
struct Run {
    asked: Vec<Asked>,
    refused: Vec<Refused>,
    end: Option<Next>,
}

/// Plays pass 2 for `request` at `now` (`refreshed`: a catalog refresh completed): asks
/// `memory.next`, launches, and answers with `script(provider, offer id, zone)`, until the
/// request is accepted or the walk acquires nothing.
fn drive(
    memory: &mut AcquireMemory,
    request: &RequestId,
    providers: &[ProviderView<'_>],
    (now, refreshed): (Instant, bool),
    script: &dyn Fn(&str, &str, Option<&str>) -> Acquired,
) -> Run {
    let job = demand();
    let mut run = Run {
        asked: Vec::new(),
        refused: Vec::new(),
        end: None,
    };
    for n in 0..64 {
        let (next, _) = memory.next(request, &input(&job, providers), now, (RETRY, refreshed));
        let Next::Acquire { provider, offer } = next else {
            run.end = Some(next);
            return run;
        };
        run.asked
            .push(asked(provider.as_str(), &offer.id, offer.zone.as_deref()));
        let acquire = acquire_id(n);
        memory.launched(request, &acquire);
        let answer = script(provider.as_str(), &offer.id, offer.zone.as_deref());
        let done = matches!(answer, Acquired::Accepted { .. });
        run.refused
            .extend(memory.answered(&acquire, (&provider, &*offer), &answer, now, BACKOFF));
        if done {
            return run;
        }
    }
    panic!("the walk did not end");
}

/// The offer the next call asks for, by offer id and zone.
fn next_offer(
    memory: &mut AcquireMemory,
    request: &RequestId,
    providers: &[ProviderView<'_>],
    now: Instant,
) -> Option<(String, Option<String>)> {
    let job = demand();
    match memory
        .next(request, &input(&job, providers), now, (RETRY, false))
        .0
    {
        Next::Acquire { offer, .. } => Some((offer.id, offer.zone)),
        _ => None,
    }
}

/// With provider `alpha` listing X in z1 (100) and z2 (300) and Y in z1 (200), and `beta`
/// listing W (50, the cheapest of all), and a capacity answer for X/z1, X/z2 and Y/z1, the
/// walk asks X/z1, X/z2 (the other zone of the same offer before the cheaper Y/z1), Y/z1,
/// then W of the second provider, and nothing else; the accepted answer leaves no attempt.
/// Catches zones tried in plain price order, a later provider asked before an earlier
/// one's offers are refused, a refused offer asked twice, or an attempt kept after its end.
#[test]
fn capacity_falls_through_zone_then_offer_then_provider() {
    let alpha = [
        offer("X", Some("z1"), Some(100)),
        offer("X", Some("z2"), Some(300)),
        offer("Y", Some("z1"), Some(200)),
    ];
    let beta = [offer("W", Some("zb"), Some(50))];
    let providers = [provider("alpha", &alpha), provider("beta", &beta)];
    let mut memory = AcquireMemory::new();
    let run = drive(
        &mut memory,
        &request("r1"),
        &providers,
        (ms(1000), false),
        &|p, _, _| match p {
            "beta" => accepted(),
            _ => capacity(),
        },
    );
    assert_eq!(
        run.asked,
        vec![
            asked("alpha", "X", Some("z1")),
            asked("alpha", "X", Some("z2")),
            asked("alpha", "Y", Some("z1")),
            asked("beta", "W", Some("zb")),
        ]
    );
    assert_eq!(run.end, None);
    assert_eq!(run.refused.len(), 3);
    assert!(run.refused.iter().all(|r| r.code == RefusalCode::Capacity));
    assert_eq!(run.refused[1].zone.as_deref(), Some("z2"));
    assert_eq!(run.refused[0].passed_over, None);
    assert_eq!(memory.attempts_len(), 0);
}

/// A quota answer for type `a` in region `r` (zones r-a and r-b) passes over `a` in both
/// zones for the quota backoff, for every request: the first request goes on to the next
/// cheapest type, the second never asks `a` in region `r` and reaches `a` in region `s`
/// only when it is the cheapest left, and once the backoff has ended `a` in r-a is asked
/// first again. The refusal carries the pass-over key and its end, and type `b` is not
/// passed over. Catches a quota answer treated as one zone, one request, another type, or
/// as permanent, and a region confused with its neighbour.
#[test]
fn quota_passes_over_that_type_for_quota_backoff() {
    let cloud = id("cloud");
    let offers = [
        offer("a", Some("r-a"), Some(100)),
        offer("a", Some("r-b"), Some(110)),
        offer("a", Some("s-a"), Some(400)),
        offer("b", Some("r-a"), Some(200)),
    ];
    let providers = [provider("cloud", &offers)];
    let mut memory = AcquireMemory::new()
        .with_region(&cloud, "r-a", "r")
        .with_region(&cloud, "r-b", "r")
        .with_region(&cloud, "s-a", "s");

    let first = drive(
        &mut memory,
        &request("one"),
        &providers,
        (ms(1000), false),
        &|_, o, _| match o {
            "a" => quota(),
            _ => accepted(),
        },
    );
    assert_eq!(
        first.asked,
        vec![
            asked("cloud", "a", Some("r-a")),
            asked("cloud", "b", Some("r-a"))
        ]
    );
    let key = PassedOver {
        provider: cloud.clone(),
        instance_type: "a".to_owned(),
        region: Some("r".to_owned()),
    };
    let until = ms(1000).saturating_add(BACKOFF);
    assert_eq!(
        first.refused,
        vec![Refused {
            provider: cloud.clone(),
            offer: "a".to_owned(),
            zone: Some("r-a".to_owned()),
            code: RefusalCode::Quota,
            passed_over: Some((key, until)),
            request: Some(request("one")),
        }]
    );
    assert_eq!(memory.passed_over_len(), 1);
    assert!(memory.is_passed_over(&cloud, &offers[1], ms(2000)));
    assert!(!memory.is_passed_over(&cloud, &offers[2], ms(2000)));
    assert!(!memory.is_passed_over(&cloud, &offers[3], ms(2000)));

    let second = drive(
        &mut memory,
        &request("two"),
        &providers,
        (ms(2000), false),
        &|_, o, _| match o {
            "b" => capacity(),
            _ => accepted(),
        },
    );
    assert_eq!(
        second.asked,
        vec![
            asked("cloud", "b", Some("r-a")),
            asked("cloud", "a", Some("s-a"))
        ]
    );

    let after = ms(1000 + 3_600_000);
    assert_eq!(
        next_offer(&mut memory, &request("three"), &providers, after),
        Some(("a".to_owned(), Some("r-a".to_owned())))
    );
    assert_eq!(memory.passed_over_len(), 0);
}

/// When every offer is refused the walk ends with `Exhausted` at now plus the retry; a call
/// before that time is `Retrying` with the same time; a completed catalog refresh asks
/// again at once, with the refused offers forgotten (the first offer is asked first); and
/// a call at the time asks again; each repeat ends the same way. Catches a retry that
/// never waits, that waits past a refresh, that does not wait the full retry, or that
/// remembers the declined offers into the next attempt.
#[test]
fn every_offer_refused_ends_the_attempt_and_waits_acquire_retry() {
    let offers = [
        offer("P", Some("z1"), Some(100)),
        offer("Q", Some("z1"), Some(200)),
    ];
    let providers = [provider("cloud", &offers)];
    let mut memory = AcquireMemory::new();
    let req = request("r");
    let both = vec![
        asked("cloud", "P", Some("z1")),
        asked("cloud", "Q", Some("z1")),
    ];
    let all = |_: &str, _: &str, _: Option<&str>| capacity();

    let run = drive(&mut memory, &req, &providers, (ms(1000), false), &all);
    assert_eq!(run.asked, both);
    assert_eq!(run.end, Some(Next::Exhausted { at: ms(61_000) }));

    let job = demand();
    let (next, skips) = memory.next(&req, &input(&job, &providers), ms(60_999), (RETRY, false));
    assert_eq!(next, Next::Retrying { at: ms(61_000) });
    assert_eq!(skips, Vec::<Skip>::new());
    assert_eq!(memory.attempts_len(), 1);

    let run = drive(&mut memory, &req, &providers, (ms(60_999), true), &all);
    assert_eq!(run.asked, both);
    assert_eq!(run.end, Some(Next::Exhausted { at: ms(120_999) }));

    let run = drive(&mut memory, &req, &providers, (ms(120_999), false), &all);
    assert_eq!(run.asked, both);
    assert_eq!(run.end, Some(Next::Exhausted { at: ms(180_999) }));
}

/// An `Unknown` or `Deferred` answer returns nothing and records nothing: the next call
/// still offers the same first offer, and a later accepted answer under the same acquire id
/// clears the request's attempt. Catches an answer that is no answer treated as a refusal
/// and failed over, which could launch a second machine.
#[test]
fn an_unknown_or_deferred_answer_records_nothing() {
    let offers = [
        offer("P", Some("z1"), Some(100)),
        offer("Q", Some("z1"), Some(200)),
    ];
    let providers = [provider("cloud", &offers)];
    let cloud = id("cloud");
    let mut memory = AcquireMemory::new();
    let req = request("r");
    let acquire = acquire_id(1);
    let first = Some(("P".to_owned(), Some("z1".to_owned())));

    assert_eq!(next_offer(&mut memory, &req, &providers, ms(1000)), first);
    memory.launched(&req, &acquire);
    let unknown = memory.answered(
        &acquire,
        (&cloud, &offers[0]),
        &Acquired::Unknown,
        ms(1000),
        BACKOFF,
    );
    assert_eq!(unknown, None);
    assert_eq!(next_offer(&mut memory, &req, &providers, ms(1001)), first);

    let deferred = Acquired::Deferred {
        wait: Duration::from_secs(5),
    };
    let held = memory.answered(&acquire, (&cloud, &offers[0]), &deferred, ms(1002), BACKOFF);
    assert_eq!(held, None);
    assert_eq!(next_offer(&mut memory, &req, &providers, ms(1003)), first);
    assert_eq!(memory.passed_over_len(), 0);

    assert_eq!(memory.attempts_len(), 1);
    let done = memory.answered(
        &acquire,
        (&cloud, &offers[0]),
        &accepted(),
        ms(1004),
        BACKOFF,
    );
    assert_eq!(done, None);
    assert_eq!(memory.attempts_len(), 0);
}

/// A `shape` or `denied` answer returns nothing, and declines and passes over nothing: the
/// next call offers the same first offer. Catches those answers failed over here, where the
/// caller owns them.
#[test]
fn shape_and_denied_are_not_failed_over_here() {
    let offers = [
        offer("P", Some("z1"), Some(100)),
        offer("Q", Some("z1"), Some(200)),
    ];
    let providers = [provider("cloud", &offers)];
    let cloud = id("cloud");
    let mut memory = AcquireMemory::new();
    let req = request("r");
    let first = Some(("P".to_owned(), Some("z1".to_owned())));
    for (n, code) in [RefusalCode::Shape, RefusalCode::Denied]
        .into_iter()
        .enumerate()
    {
        assert_eq!(next_offer(&mut memory, &req, &providers, ms(1000)), first);
        let acquire = acquire_id(u32::try_from(n).unwrap());
        memory.launched(&req, &acquire);
        let out = memory.answered(
            &acquire,
            (&cloud, &offers[0]),
            &refuse(code),
            ms(1000),
            BACKOFF,
        );
        assert_eq!(out, None);
        assert_eq!(next_offer(&mut memory, &req, &providers, ms(1000)), first);
        assert_eq!(memory.passed_over_len(), 0);
    }
}

/// The memory is bounded: 500 distinct quota answers are 500 pass-overs; a call after the
/// backoff has ended leaves none; `retain` drops the attempts of requests no longer waiting
/// and the launches no longer pending (an answer to a dropped launch declines nothing) and
/// keeps those still held. Catches pass-overs, attempts or launches that grow without end.
#[test]
fn acquire_memory_is_bounded() {
    let offers = [
        offer("P", Some("z1"), Some(100)),
        offer("Q", Some("z1"), Some(200)),
    ];
    let providers = [provider("cloud", &offers)];
    let cloud = id("cloud");
    let mut memory = AcquireMemory::new();
    for n in 0..500 {
        let each = offer(&format!("type-{n}"), Some("z1"), Some(100));
        let out = memory.answered(&acquire_id(n), (&cloud, &each), &quota(), ms(0), BACKOFF);
        assert!(out.is_some());
    }
    assert_eq!(memory.passed_over_len(), 500);
    let req = request("r");
    assert_eq!(
        next_offer(&mut memory, &req, &providers, ms(3_599_999)),
        Some(("P".to_owned(), Some("z1".to_owned())))
    );
    assert_eq!(memory.passed_over_len(), 500);
    next_offer(&mut memory, &req, &providers, ms(3_600_000));
    assert_eq!(memory.passed_over_len(), 0);

    let kept = acquire_id(900);
    let dropped = acquire_id(901);
    memory.launched(&req, &kept);
    memory.launched(&req, &dropped);
    let capacity_for = |memory: &mut AcquireMemory, acquire: &AcquireId| {
        memory.answered(
            acquire,
            (&cloud, &offers[0]),
            &capacity(),
            ms(3_600_000),
            BACKOFF,
        )
    };
    memory.retain(|_| true, |acquire| *acquire == kept);
    assert!(capacity_for(&mut memory, &dropped).is_some());
    assert_eq!(
        next_offer(&mut memory, &req, &providers, ms(3_600_000))
            .unwrap()
            .0,
        "P"
    );
    assert!(capacity_for(&mut memory, &kept).is_some());
    assert_eq!(
        next_offer(&mut memory, &req, &providers, ms(3_600_000))
            .unwrap()
            .0,
        "Q"
    );

    assert_eq!(memory.attempts_len(), 1);
    memory.retain(|_| false, |_| false);
    assert_eq!(memory.attempts_len(), 0);
}

/// A pick that acquires nothing even with nothing left out stays the pick's wait: no offer
/// at all is `NoOffer` with the provider's skip, an unpriced offer is `Unpriced`, and a full
/// pool is `PoolFull`; none is `Exhausted`. Catches a wait that was
/// not caused by a refusal reported as an exhausted attempt.
#[test]
fn a_wait_that_is_not_a_refusal_stays_the_picks_wait() {
    let job = demand();
    let mut memory = AcquireMemory::new();
    let req = request("r");
    let now = ms(1000);

    let none: [Offer; 0] = [];
    let providers = [provider("cloud", &none)];
    let (next, skips) = memory.next(&req, &input(&job, &providers), now, (RETRY, false));
    assert_eq!(next, Next::Wait(Waiting::NoOffer));
    assert_eq!(
        skips,
        vec![Skip {
            provider: id("cloud"),
            why: SkipReason::NoUsableOffer
        }]
    );

    let unpriced = [offer("P", Some("z1"), None)];
    let providers = [provider("cloud", &unpriced)];
    let (next, _) = memory.next(&req, &input(&job, &providers), now, (RETRY, false));
    assert_eq!(next, Next::Wait(Waiting::Unpriced));

    let priced = [offer("P", Some("z1"), Some(100))];
    let providers = [provider("cloud", &priced)];
    let full = AcquireInput {
        max_machines: Some(0),
        ..input(&job, &providers)
    };
    let (next, _) = memory.next(&req, &full, now, (RETRY, false));
    assert_eq!(next, Next::Wait(Waiting::PoolFull));
}

/// `region` is the zone named by `with_region`, else the zone itself, per provider, and
/// `None` for no zone; the pass-over key of an offer with no zone has no region and so
/// covers the provider's whole type. Catches a region mapping shared across providers, a
/// zone-less offer given a region, or an unmapped zone lost.
#[test]
fn regions_name_zones_per_provider() {
    let alpha = id("alpha");
    let beta = id("beta");
    let memory = AcquireMemory::new().with_region(&alpha, "z-a", "z");
    assert_eq!(memory.region(&alpha, Some("z-a")), Some("z".to_owned()));
    assert_eq!(memory.region(&alpha, Some("y-a")), Some("y-a".to_owned()));
    assert_eq!(memory.region(&beta, Some("z-a")), Some("z-a".to_owned()));
    assert_eq!(memory.region(&alpha, None), None);
    let bare = offer("T", None, Some(100));
    assert_eq!(
        memory.passed_over_key(&alpha, &bare),
        PassedOver {
            provider: alpha,
            instance_type: "T".to_owned(),
            region: None,
        }
    );
}

/// A pass-over ends exactly at `now + backoff`: `forget_expired` drops it from then on and
/// not before, and a later, shorter quota answer does not shorten it. Catches an early or
/// late end, an expired entry kept, and a pass-over shortened by a newer answer.
#[test]
fn a_pass_over_ends_at_its_time_and_is_not_shortened() {
    let cloud = id("cloud");
    let one = offer("T", Some("z1"), Some(100));
    let mut memory = AcquireMemory::new();
    let ten = Duration::from_secs(10);
    let five = Duration::from_secs(5);
    memory.answered(&acquire_id(1), (&cloud, &one), &quota(), ms(0), ten);
    memory.answered(&acquire_id(2), (&cloud, &one), &quota(), ms(0), five);
    assert!(memory.is_passed_over(&cloud, &one, ms(9_999)));
    assert!(!memory.is_passed_over(&cloud, &one, ms(10_000)));
    memory.forget_expired(ms(9_999));
    assert_eq!(memory.passed_over_len(), 1);
    memory.forget_expired(ms(10_000));
    assert_eq!(memory.passed_over_len(), 0);
}

/// An answer for an acquire never launched through the memory still passes over on quota
/// and records the refusal on capacity, but declines nothing (no attempt), and an accepted
/// one changes nothing. Catches an answer without a known request that panics, is lost, or
/// declines the offer for some other request.
#[test]
fn an_answer_for_an_unknown_acquire_is_charged_to_no_request() {
    let offers = [
        offer("P", Some("z1"), Some(100)),
        offer("Q", Some("z1"), Some(200)),
    ];
    let providers = [provider("cloud", &offers)];
    let cloud = id("cloud");
    let mut memory = AcquireMemory::new();

    let out = memory.answered(
        &acquire_id(1),
        (&cloud, &offers[0]),
        &capacity(),
        ms(0),
        BACKOFF,
    );
    let out = out.expect("a refusal");
    assert_eq!(out.code, RefusalCode::Capacity);
    assert_eq!(out.passed_over, None);
    assert_eq!(memory.attempts_len(), 0);

    let out = memory.answered(
        &acquire_id(2),
        (&cloud, &offers[0]),
        &quota(),
        ms(0),
        BACKOFF,
    );
    assert!(out.expect("a refusal").passed_over.is_some());
    assert_eq!(memory.passed_over_len(), 1);

    let out = memory.answered(
        &acquire_id(3),
        (&cloud, &offers[1]),
        &accepted(),
        ms(0),
        BACKOFF,
    );
    assert_eq!(out, None);
    assert_eq!(memory.attempts_len(), 0);
    assert_eq!(
        next_offer(&mut memory, &request("r"), &providers, ms(1)),
        Some(("Q".to_owned(), Some("z1".to_owned())))
    );
}

/// When the offer whose other zones come next has left the provider's catalog, the walk
/// falls back to the full pick over what is left. Catches a walk that waits, or ends the
/// attempt, because the followed offer is gone.
#[test]
fn a_followed_offer_that_left_the_catalog_falls_back_to_the_full_pick() {
    let before = [
        offer("X", Some("z1"), Some(100)),
        offer("Y", Some("z1"), Some(200)),
    ];
    let after = [offer("Y", Some("z1"), Some(200))];
    let cloud = id("cloud");
    let mut memory = AcquireMemory::new();
    let req = request("r");
    assert_eq!(
        next_offer(&mut memory, &req, &[provider("cloud", &before)], ms(0))
            .unwrap()
            .0,
        "X"
    );
    memory.launched(&req, &acquire_id(1));
    memory.answered(
        &acquire_id(1),
        (&cloud, &before[0]),
        &capacity(),
        ms(0),
        BACKOFF,
    );
    assert_eq!(
        next_offer(&mut memory, &req, &[provider("cloud", &after)], ms(1)),
        Some(("Y".to_owned(), Some("z1".to_owned())))
    );
}

/// Asserts what the read accessors give: before any answer, nothing passed over and nothing
/// declined; after a capacity answer for X/z1 at 10 ms and a quota answer for Y/z2 at 20 ms
/// for request `r1`, the declined list holds X/z1 for `r1` at 10 ms and the passed-over list
/// holds Y in region z2 until 20 ms plus the backoff; at the pass-over's end it is left out
/// although not yet forgotten; and once the request's attempt ends (every offer refused) its
/// declines are gone. Catches a decline without its request or time, a pass-over shown past
/// its end, and declines of an ended attempt still shown.
#[test]
fn the_read_accessors_show_declines_and_pass_overs() {
    let offers = [
        offer("X", Some("z1"), Some(100)),
        offer("Y", Some("z2"), Some(200)),
    ];
    let providers = [provider("alpha", &offers)];
    let alpha = id("alpha");
    let r1 = request("r1");
    let mut memory = AcquireMemory::new();
    assert_eq!(memory.passed_over(ms(0)).count(), 0);
    assert_eq!(memory.declined().count(), 0);

    memory.launched(&r1, &acquire_id(1));
    memory.answered(
        &acquire_id(1),
        (&alpha, &offers[0]),
        &capacity(),
        ms(10),
        BACKOFF,
    );
    memory.launched(&r1, &acquire_id(2));
    memory.answered(
        &acquire_id(2),
        (&alpha, &offers[1]),
        &quota(),
        ms(20),
        BACKOFF,
    );

    let declined: Vec<_> = memory.declined().collect();
    let x_z1 = js_elastic::OfferKey::of(&alpha, &offers[0]);
    assert_eq!(declined, [(&r1, &x_z1, ms(10))]);
    let until = ms(20).saturating_add(BACKOFF);
    let y_z2 = PassedOver {
        provider: alpha.clone(),
        instance_type: "Y".to_owned(),
        region: Some("z2".to_owned()),
    };
    let passed: Vec<_> = memory.passed_over(ms(20)).collect();
    assert_eq!(passed, [(&y_z2, until)]);
    assert_eq!(memory.passed_over(until).count(), 0);
    assert_eq!(memory.passed_over_len(), 1);

    let next = next_offer(&mut memory, &r1, &providers, ms(30));
    assert_eq!(next, None);
    assert_eq!(memory.declined().count(), 0);
}
