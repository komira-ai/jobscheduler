//! What the scaler does when a provider refuses an acquire, driven through `step` with the
//! answers handed in by hand (and, for lost answers, against the elastic fake): a refusal
//! for capacity is failed over within the request's attempt, zone by zone and then offer by
//! offer; a refusal for quota passes the instance type over for `quota_backoff`; a request
//! every offer was refused for waits `acquire_retry`, or a catalog refresh; an answer that
//! is no answer never launches a second machine. Time is a `Millis` the tests name; nothing
//! sleeps or reads a clock.

mod cloud_rig;
mod common;

use std::time::Duration;

use cloud_rig::Sim;
use common::{
    MINUTE, Rig, SECOND, accepted, acquires, answered, at, host, listed, minutes, offer, request,
    settings,
};
use js_elastic::{Acquired, HostStatus, Instant, Offer, Reason, Refusal, RefusalCode};
use js_elastic_fake::{Billing, Call, ElasticFake, Step};
use js_scaler::{
    AcquirePlan, AcquireResult, Blocked, Cooldown, CooldownReason, Millis, Phase, Settings, State,
    UNNAMED_PROVIDER,
};
use js_sched::failover::{PassedOver, Refused};

/// A refusal with `code`.
fn refusal(code: RefusalCode) -> Acquired {
    Acquired::Refused(Refusal {
        code,
        reason: Reason::DryRun,
    })
}

/// The provider's answer to `plan`: no room.
fn capacity(plan: &AcquirePlan) -> AcquireResult {
    answered(plan, refusal(RefusalCode::Capacity))
}

/// The offer `id` in `zone` at `price` cents an hour.
fn zoned(id: &str, zone: &str, price: u64) -> Offer {
    Offer {
        zone: Some(zone.to_owned()),
        ..offer(id, price)
    }
}

/// One step at `now` that reads `answers` first. Returns the acquires it makes.
fn step(rig: &mut Rig, now: Millis, answers: Vec<AcquireResult>) -> Vec<AcquirePlan> {
    rig.input.acquired = answers;
    acquires(&rig.tick(now))
}

/// The offer and zone `plan` asks for, as `id/zone`.
fn label(plan: &AcquirePlan) -> String {
    let zone = plan.offer.zone.as_deref().unwrap_or("-");
    format!("{}/{zone}", plan.offer.id)
}

/// The labels of `plans`, in order.
fn labels(plans: &[AcquirePlan]) -> Vec<String> {
    plans.iter().map(label).collect()
}

/// The one acquire among `plans`.
fn only(mut plans: Vec<AcquirePlan>) -> AcquirePlan {
    assert_eq!(plans.len(), 1, "{:?}", labels(&plans));
    plans.remove(0)
}

/// Asserts that the pool shows no block, holds no cooldown and has counted no failure.
fn calm(rig: &Rig) {
    assert!(rig.state.blocked.is_empty(), "{:?}", rig.state.blocked);
    assert!(rig.state.records.cooldowns.is_empty());
    assert!(rig.state.records.brake.is_empty());
}

/// The phases of the records launched for the offer `id`.
fn kept(rig: &Rig, id: &str) -> Vec<Phase> {
    let records = rig.state.records.instances.iter();
    records
        .filter(|record| record.offer.id == id)
        .map(|record| record.phase)
        .collect()
}

/// The refusal for capacity that `id` in `zone` is logged as.
fn capacity_of(id: &str, zone: &str) -> Refused {
    Refused {
        provider: UNNAMED_PROVIDER,
        offer: id.to_owned(),
        zone: Some(zone.to_owned()),
        code: RefusalCode::Capacity,
        passed_over: None,
    }
}

/// A rig whose pool lists `offers`, with the request `r1` waiting.
fn rig_with(limits: Settings, offers: Vec<Offer>) -> Rig {
    let mut rig = Rig::new(limits);
    rig.pool().offers = offers;
    rig.input.waiting = vec![request("r1")];
    rig
}

/// Asserts the walk of a request through a pool whose offers all refuse for capacity: X in
/// zone z1 (60 cents), X in z2 (80) and Y in z1 (70). The first ask is X/z1; after its
/// refusal the other zone of the same offer is asked before the cheaper Y; after that
/// refusal Y/z1 is asked; X/z1 is not asked again in the attempt. The refusal that ends the
/// attempt asks nothing, the pool shows no block, no cooldown and no failure at any step,
/// each step's `refusals` holds the refusal it read, and the next attempt begins after
/// `acquire_retry` with X/z1 again and ends in a machine. Catches a refusal that is
/// forgotten after the step that read it (the walk would return to X/z1 on the third ask),
/// one that cools the pool, and a walk that ignores the zones of the refused offer.
#[test]
fn capacity_falls_through_zone_then_offer() {
    let offers = vec![
        zoned("X", "z1", 60),
        zoned("X", "z2", 80),
        zoned("Y", "z1", 70),
    ];
    let mut rig = rig_with(settings("gpu"), offers);

    let first = only(step(&mut rig, at(0), vec![]));
    assert_eq!(label(&first), "X/z1");
    assert!(rig.state.refusals.is_empty());

    let second = only(step(&mut rig, at(SECOND), vec![capacity(&first)]));
    assert_eq!(label(&second), "X/z2");
    assert_eq!(rig.state.refusals, [capacity_of("X", "z1")]);
    calm(&rig);

    let third = only(step(&mut rig, at(2 * SECOND), vec![capacity(&second)]));
    assert_eq!(label(&third), "Y/z1");
    assert_eq!(rig.state.refusals, [capacity_of("X", "z2")]);
    calm(&rig);

    let ended = step(&mut rig, at(3 * SECOND), vec![capacity(&third)]);
    assert!(ended.is_empty(), "{:?}", labels(&ended));
    assert_eq!(rig.state.refusals, [capacity_of("Y", "z1")]);
    assert!(rig.state.records.instances.is_empty());
    calm(&rig);

    let early = step(&mut rig, at(63 * SECOND - 1), vec![]);
    assert!(early.is_empty(), "{:?}", labels(&early));
    assert!(rig.state.refusals.is_empty());
    let again = only(step(&mut rig, at(63 * SECOND), vec![]));
    assert_eq!(label(&again), "X/z1");

    rig.pool().held = Some(vec![listed(&again, "i-1", HostStatus::Booting)]);
    assert!(step(&mut rig, at(64 * SECOND), vec![accepted(&again, "i-1")]).is_empty());
    assert_eq!(rig.phase(0), Phase::Booting);
    assert!(rig.state.refusals.is_empty());
    calm(&rig);
}

/// Asserts that a refusal for quota passes the instance type over and nothing else. Type a
/// (60 cents) is refused for quota: the same step asks type b, the log line names what is
/// passed over and until when (`quota_backoff`, 30 minutes, from the step that read the
/// answer), the pool shows no block and no failure is counted. A different request that
/// arrives then is asked b, never a, and so is the first request when its attempt begins
/// again, until the last second of the backoff; at the instant the backoff ends a is asked
/// again. Catches a quota refusal that cools the pool or strikes the brake, a pass-over
/// that is forgotten with the request it was made for, one that ends early or late, and one
/// that never ends.
#[test]
fn quota_passes_over_that_type_for_quota_backoff() {
    let limits = Settings {
        quota_backoff: minutes(30),
        ..settings("gpu")
    };
    let mut rig = rig_with(limits, vec![offer("a", 60), offer("b", 90)]);

    let a = only(step(&mut rig, at(0), vec![]));
    assert_eq!(label(&a), "a/z1");

    let quota = answered(&a, refusal(RefusalCode::Quota));
    let b1 = only(step(&mut rig, at(SECOND), vec![quota]));
    assert_eq!(label(&b1), "b/z1");
    let until = Instant::from_millis(at(SECOND).as_millis() + 30 * MINUTE);
    let passed = PassedOver {
        provider: UNNAMED_PROVIDER,
        instance_type: "a".to_owned(),
        region: Some("z1".to_owned()),
    };
    let logged = Refused {
        provider: UNNAMED_PROVIDER,
        offer: "a".to_owned(),
        zone: Some("z1".to_owned()),
        code: RefusalCode::Quota,
        passed_over: Some((passed, until)),
    };
    assert_eq!(rig.state.refusals, [logged]);
    calm(&rig);

    // Another request: the pass-over is not the first request's alone.
    rig.input.waiting = vec![request("r2")];
    let b2 = only(step(&mut rig, at(2 * SECOND), vec![capacity(&b1)]));
    assert_eq!(label(&b2), "b/z1");

    let ended = step(&mut rig, at(3 * SECOND), vec![capacity(&b2)]);
    assert!(ended.is_empty(), "{:?}", labels(&ended));
    calm(&rig);

    let b3 = only(step(&mut rig, at(63 * SECOND), vec![]));
    assert_eq!(label(&b3), "b/z1");
    let ended = step(&mut rig, at(64 * SECOND), vec![capacity(&b3)]);
    assert!(ended.is_empty(), "{:?}", labels(&ended));

    let b4 = only(step(&mut rig, at(30 * MINUTE), vec![]));
    assert_eq!(label(&b4), "b/z1");

    let last = only(step(
        &mut rig,
        at(30 * MINUTE + SECOND),
        vec![capacity(&b4)],
    ));
    assert_eq!(label(&last), "a/z1");
    calm(&rig);
}

/// Asserts that an offer the catalog lists as available and the cloud refuses costs one
/// refused acquire and no placement. S is the cheapest and listed available; the cloud
/// refuses it for capacity. The cloud is asked about S exactly once, no record of S is ever
/// Booting or Registered, the request is placed on the other offer O in the very step that
/// read the refusal, and once the refreshed catalog marks S unavailable (with
/// `catalog_refreshed` set) a second request is asked O and S is not asked again. Catches a
/// refusal that leaves a record of the refused offer, one that is followed by a second ask
/// of the same offer, and a refresh that does not keep a refused offer out.
#[test]
fn a_stale_offer_costs_one_refused_acquire_not_a_wrong_placement() {
    let mut rig = rig_with(settings("gpu"), vec![offer("S", 60), offer("O", 90)]);
    let mut log: Vec<String> = Vec::new();

    let plans = step(&mut rig, at(0), vec![]);
    log.extend(labels(&plans));
    let s = only(plans);
    assert_eq!(label(&s), "S/z1");
    assert_eq!(kept(&rig, "S"), [Phase::Launching]);

    let plans = step(&mut rig, at(SECOND), vec![capacity(&s)]);
    log.extend(labels(&plans));
    let o = only(plans);
    assert_eq!(label(&o), "O/z1");
    assert!(kept(&rig, "S").is_empty());
    assert_eq!(rig.state.refusals, [capacity_of("S", "z1")]);
    calm(&rig);

    // The refreshed catalog: S is no longer available.
    rig.pool().offers = vec![
        Offer {
            available: false,
            ..offer("S", 60)
        },
        offer("O", 90),
    ];
    rig.pool().catalog_refreshed = true;
    rig.pool().held = Some(vec![listed(&o, "i-1", HostStatus::Booting)]);
    rig.input.waiting = vec![request("r1"), request("r2")];
    let plans = step(&mut rig, at(2 * SECOND), vec![accepted(&o, "i-1")]);
    log.extend(labels(&plans));
    assert_eq!(labels(&plans), ["O/z1"]);
    assert!(kept(&rig, "S").is_empty());
    assert!(rig.state.refusals.is_empty());
    calm(&rig);

    let asked_s = log.iter().filter(|each| each.starts_with("S/")).count();
    assert_eq!(asked_s, 1, "{log:?}");
}

/// Asserts the retry of a request every offer refused. Two offers refuse for capacity: the
/// first attempt asks each once and ends at the second refusal; nothing is asked until
/// `acquire_retry` (a minute) after it, not a millisecond before; the attempt then asks
/// each once again, and another follows a minute after it ends. A catalog refresh
/// (`catalog_refreshed`) ends a wait at once. Once the cloud accepts, the next attempt
/// launches a machine. The request waits throughout, the pool shows no block and holds no
/// cooldown or failure. Catches a request that is failed or dropped when every offer
/// refused, one asked again in the tick after the refusal, one asked every tick, a refresh
/// that does not end the wait, and a wait that never ends.
#[test]
fn every_offer_refused_keeps_the_job_queued_and_retries_on_the_interval() {
    let mut rig = rig_with(settings("gpu"), vec![offer("a", 60), offer("b", 90)]);

    let one = only(step(&mut rig, at(0), vec![]));
    assert_eq!(label(&one), "a/z1");
    let two = only(step(&mut rig, at(SECOND), vec![capacity(&one)]));
    assert_eq!(label(&two), "b/z1");
    let ended = step(&mut rig, at(2 * SECOND), vec![capacity(&two)]);
    assert!(ended.is_empty(), "{:?}", labels(&ended));
    calm(&rig);
    assert!(step(&mut rig, at(3 * SECOND), vec![]).is_empty());
    assert!(step(&mut rig, at(62 * SECOND - 1), vec![]).is_empty());

    let three = only(step(&mut rig, at(62 * SECOND), vec![]));
    assert_eq!(label(&three), "a/z1");
    let four = only(step(&mut rig, at(63 * SECOND), vec![capacity(&three)]));
    assert_eq!(label(&four), "b/z1");
    assert!(step(&mut rig, at(64 * SECOND), vec![capacity(&four)]).is_empty());
    assert!(step(&mut rig, at(124 * SECOND - 1), vec![]).is_empty());

    let five = only(step(&mut rig, at(124 * SECOND), vec![]));
    assert_eq!(label(&five), "a/z1");
    let six = only(step(&mut rig, at(125 * SECOND), vec![capacity(&five)]));
    assert_eq!(label(&six), "b/z1");
    assert!(step(&mut rig, at(126 * SECOND), vec![capacity(&six)]).is_empty());
    assert!(step(&mut rig, at(127 * SECOND - 1), vec![]).is_empty());

    // A refreshed catalog ends the wait that would run to 186 seconds.
    rig.pool().catalog_refreshed = true;
    let seven = only(step(&mut rig, at(127 * SECOND), vec![]));
    assert_eq!(label(&seven), "a/z1");
    rig.pool().catalog_refreshed = false;
    let eight = only(step(&mut rig, at(128 * SECOND), vec![capacity(&seven)]));
    assert_eq!(label(&eight), "b/z1");
    assert!(step(&mut rig, at(129 * SECOND), vec![capacity(&eight)]).is_empty());
    assert!(step(&mut rig, at(189 * SECOND - 1), vec![]).is_empty());
    calm(&rig);
    assert_eq!(rig.input.waiting.len(), 1);

    // The cloud has room again: the next attempt launches.
    let nine = only(step(&mut rig, at(189 * SECOND), vec![]));
    assert_eq!(label(&nine), "a/z1");
    rig.pool().held = Some(vec![listed(&nine, "i-1", HostStatus::Booting)]);
    let none = step(&mut rig, at(190 * SECOND), vec![accepted(&nine, "i-1")]);
    assert!(none.is_empty());
    assert_eq!(rig.phase(0), Phase::Booting);
    calm(&rig);
}

/// Asserts that an answer that is no answer launches nothing else. A lost answer for a
/// launch that landed (the fake cloud lists two offers and its list lags ten minutes): no
/// acquire for any offer follows while the record is Unresolved, the cloud makes one machine
/// of the first offer only, and when the list shows it the record is that machine, with no
/// second acquire. A deferral (429): the pool acquires nothing, for this request or another,
/// until the wait has run, and the retry carries the same acquire id and offer. Catches an
/// unknown answer taken for a refusal that fails over to the next offer (a second machine
/// for one request), and a deferral that lets another offer be asked.
#[test]
fn an_unknown_answer_never_launches_a_second_instance() {
    let both = vec![offer("a10", 60), offer("b", 90)];
    let mut sim = Sim::new(settings("gpu"), Billing::Lambda);
    sim.fake = ElasticFake::new(sim.clock.clone(), Billing::Lambda, both.clone());
    sim.rig.pool().offers = both;
    let mut knobs = sim.fake.knobs();
    knobs.list_lag = minutes(10);
    sim.fake.set_knobs(knobs);
    sim.fake
        .script(Call::Acquire, &[Step::Lost { landed: true }]);
    sim.waiting = 1;

    sim.run(2);
    assert_eq!(sim.phases(), [Phase::Unresolved]);
    sim.run(5);
    assert_eq!(sim.phases(), [Phase::Unresolved]);
    assert_eq!(sim.acquires(), 1);
    assert_eq!(sim.fake.launched(), 1);

    sim.run(30);
    assert_eq!(sim.phases(), [Phase::Registered]);
    assert_eq!(sim.acquires(), 1);
    assert_eq!(sim.fake.launched(), 1);
    assert_eq!(sim.answers, [Acquired::Unknown]);
    assert!(sim.fake.truth().iter().all(|held| held.offer == "a10"));

    let mut rig = rig_with(settings("gpu"), vec![offer("a10", 60), offer("b", 90)]);
    let plan = only(step(&mut rig, at(0), vec![]));
    assert_eq!(label(&plan), "a10/z1");
    rig.input.waiting = vec![request("r1"), request("r2")];
    let wait = Acquired::Deferred {
        wait: Duration::from_secs(12),
    };
    let during = step(&mut rig, at(SECOND), vec![answered(&plan, wait)]);
    assert!(during.is_empty(), "{:?}", labels(&during));
    assert_eq!(rig.phase(0), Phase::Planned);
    assert_eq!(rig.state.blocked["gpu"], Blocked::Deferred);
    assert!(rig.state.refusals.is_empty());
    let early = step(&mut rig, at(13 * SECOND - 1), vec![]);
    assert!(early.is_empty(), "{:?}", labels(&early));

    let retry = only(step(&mut rig, at(13 * SECOND), vec![]));
    assert_eq!(retry.acquire, plan.acquire);
    assert_eq!(label(&retry), "a10/z1");
    assert_eq!(rig.state.records.instances.len(), 1);
}

/// Asserts that a free host that fits is taken over a request's retry. The only offer was
/// refused for capacity, so the request waits for its next attempt; a live free host that
/// fits then appears: neither the step in the wait nor the one at the instant the wait ends
/// acquires anything, because the host covers the request. When the host goes, the request
/// is asked again at once. Catches a retry timer that acquires a machine although a host
/// is free, and a wait that is lost while a host covers the request.
#[test]
fn a_free_host_beats_a_retry() {
    let mut rig = rig_with(settings("gpu"), vec![offer("a10", 60)]);
    let first = only(step(&mut rig, at(0), vec![]));
    assert!(step(&mut rig, at(SECOND), vec![capacity(&first)]).is_empty());

    rig.input.hosts = vec![host("static-1", "gpu")];
    assert!(step(&mut rig, at(2 * SECOND), vec![]).is_empty());
    assert!(step(&mut rig, at(61 * SECOND), vec![]).is_empty());
    assert!(step(&mut rig, at(62 * SECOND), vec![]).is_empty());

    rig.input.hosts.clear();
    let again = only(step(&mut rig, at(63 * SECOND), vec![]));
    assert_eq!(label(&again), "a10/z1");
}

/// Asserts that a shape refusal and a denial are not failed over here: the other offer is
/// not asked in their step, the pool is blocked with their own reason, one cooldown and one
/// failure of the brake are recorded, and nothing is logged as a failover refusal. Catches
/// a walk that treats them like capacity (a loop of asks against a refusal that will not
/// change), and a change that stops counting them.
#[test]
fn shape_and_denied_do_not_fail_over_here() {
    for (code, blocked) in [
        (RefusalCode::Denied, Blocked::Denied),
        (RefusalCode::Shape, Blocked::Shape),
    ] {
        let mut rig = rig_with(settings("gpu"), vec![offer("a", 60), offer("b", 90)]);
        let plan = only(step(&mut rig, at(0), vec![]));
        assert_eq!(label(&plan), "a/z1");
        let after = step(&mut rig, at(SECOND), vec![answered(&plan, refusal(code))]);
        assert!(after.is_empty(), "{code:?}: {:?}", labels(&after));
        assert_eq!(rig.state.blocked["gpu"], blocked, "{code:?}");
        assert_eq!(rig.state.records.cooldowns.len(), 1, "{code:?}");
        let failures = rig.state.records.brake.get("gpu").map(|each| each.failures);
        assert_eq!(failures, Some(1), "{code:?}");
        assert!(rig.state.refusals.is_empty(), "{code:?}");
    }
}

/// Asserts that a restart forgets what pass 2 remembered: a scaler rebuilt from the records
/// it wrote (`State::new`) asks type a again although a quota answer passed it over, where
/// the scaler that kept running asks nothing. Catches a memory that is written to the
/// records (it would outlive a restart), and a restart that carries a stale pass-over.
#[test]
fn a_restart_forgets_the_attempts() {
    let after_quota = || {
        let mut rig = rig_with(settings("gpu"), vec![offer("a", 60), offer("b", 90)]);
        let a = only(step(&mut rig, at(0), vec![]));
        let quota = answered(&a, refusal(RefusalCode::Quota));
        let b = only(step(&mut rig, at(SECOND), vec![quota]));
        assert_eq!(label(&b), "b/z1");
        (rig, b)
    };

    let (mut kept_running, b) = after_quota();
    let left = step(&mut kept_running, at(2 * SECOND), vec![capacity(&b)]);
    assert!(left.is_empty(), "{:?}", labels(&left));

    let (mut restarted, b) = after_quota();
    restarted.state = State::new(restarted.state.records.clone());
    let again = only(step(&mut restarted, at(2 * SECOND), vec![capacity(&b)]));
    assert_eq!(label(&again), "a/z1");
}

/// Asserts that an accepted launch is not logged as a refusal and ends the walk: after a
/// capacity refusal moved the request to the next offer, the acceptance makes the record
/// Booting, asks nothing more and logs nothing. Catches an acceptance that is taken in as a
/// refusal, and a covered request that is asked again.
#[test]
fn an_accepted_answer_records_no_refusal() {
    let mut rig = rig_with(settings("gpu"), vec![offer("a", 60), offer("b", 90)]);
    let a = only(step(&mut rig, at(0), vec![]));
    let b = only(step(&mut rig, at(SECOND), vec![capacity(&a)]));
    assert_eq!(label(&b), "b/z1");
    assert_eq!(rig.state.refusals, [capacity_of("a", "z1")]);

    rig.pool().held = Some(vec![listed(&b, "i-1", HostStatus::Booting)]);
    let after = step(&mut rig, at(2 * SECOND), vec![accepted(&b, "i-1")]);
    assert!(after.is_empty(), "{:?}", labels(&after));
    assert_eq!(rig.phase(0), Phase::Booting);
    assert!(rig.state.refusals.is_empty());
    calm(&rig);
}

/// Asserts that a records file an earlier release wrote, holding a pool cooldown for a quota
/// refusal (this release passes the instance type over instead and writes none), is still
/// read and still holds the pool: it shows `Quota` and launches nothing until the cooldown's
/// end, then launches. Catches a quota cooldown read back from an older file that is dropped
/// or shown as another reason.
#[test]
fn a_quota_cooldown_an_earlier_release_wrote_still_holds_the_pool() {
    let mut rig = rig_with(settings("gpu"), vec![offer("a", 60)]);
    let mut records = rig.state.records.clone();
    records.cooldowns.insert(
        "gpu".to_owned(),
        Cooldown {
            until: at(10 * MINUTE),
            reason: CooldownReason::Quota,
        },
    );
    rig.state = State::new(records);

    let held = step(&mut rig, at(0), vec![]);
    assert!(held.is_empty(), "{:?}", labels(&held));
    assert_eq!(rig.state.blocked.get("gpu"), Some(&Blocked::Quota));

    let launched = only(step(&mut rig, at(10 * MINUTE), vec![]));
    assert_eq!(label(&launched), "a/z1");
}

/// Asserts that the state's failover memory, as a reader outside the step sees it, holds
/// what the steps took in: after a capacity answer for X/z1 the request `r1` has X/z1
/// declined at the step that read the answer, and after a quota answer for X/z2 the instance
/// type X is passed over in region z2 until that step plus `quota_backoff`. Catches a
/// reader handed a copy that the step does not update, and a decline timed at the acquire
/// rather than at the answer.
#[test]
fn the_failover_memory_is_readable_from_the_state() {
    let limits = Settings {
        quota_backoff: minutes(30),
        ..settings("gpu")
    };
    let mut rig = rig_with(limits, vec![zoned("X", "z1", 60), zoned("X", "z2", 80)]);
    assert_eq!(rig.state.failover().declined().count(), 0);

    let first = only(step(&mut rig, at(0), vec![]));
    let second = only(step(&mut rig, at(SECOND), vec![capacity(&first)]));
    let declined: Vec<(String, String, Option<String>, u64)> = rig
        .state
        .failover()
        .declined()
        .map(|(request, key, when)| {
            let (offer, zone) = (key.offer.clone(), key.zone.clone());
            (request.to_string(), offer, zone, when.millis())
        })
        .collect();
    let z1 = Some("z1".to_owned());
    let declined_at = at(SECOND).as_millis();
    assert_eq!(
        declined,
        [("r1".to_owned(), "X".to_owned(), z1, declined_at)]
    );

    let quota = answered(&second, refusal(RefusalCode::Quota));
    step(&mut rig, at(2 * SECOND), vec![quota]);
    let now = Instant::from_millis(at(2 * SECOND).as_millis());
    let passed: Vec<(String, Option<String>, u64)> = rig
        .state
        .failover()
        .passed_over(now)
        .map(|(key, until)| {
            (
                key.instance_type.clone(),
                key.region.clone(),
                until.millis(),
            )
        })
        .collect();
    let until = at(2 * SECOND).as_millis() + 30 * MINUTE;
    assert_eq!(passed, [("X".to_owned(), Some("z2".to_owned()), until)]);
}
