//! What the fake cloud does, call by call: its bill at each provider's granularity, the
//! refusals, the lost answers, the 429s, the lists that fail or lag, the releases that are
//! refused, and preemption. These are the behaviours the scaler's tests rely on, so each is
//! pinned here against numbers worked out by hand.

mod common;

use std::time::Duration;

use common::{PRICE, START, cloud, launch, mins, offer, request, request_at, secs};
use js_elastic::{
    AcquireId, Acquired, Claim, ElasticProvider, HostRef, HostStatus, Reason, Refusal, RefusalCode,
    Released,
};
use js_elastic_fake::{
    Bill, Billing, Call, ElasticFake, FakeClock, Knobs, PAYLOAD_LIMIT, Step, THROTTLE_WAIT,
};

/// The bill for `billed_ms` milliseconds of the one offer.
fn owed(billed_ms: u64) -> Bill {
    Bill(u128::from(PRICE) * u128::from(billed_ms))
}

fn refused(answer: &Acquired) -> RefusalCode {
    match answer {
        Acquired::Refused(Refusal { code, .. }) => *code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Asserts the rounding of each provider with the numbers worked out by hand: Lambda bills
/// every started minute and at least one, AWS every started second and at least sixty of
/// them, and the two Google products every started second (their real terms are
/// UNVERIFIED, which the fake says). It catches a granularity that is off by a unit, a
/// minimum that is missing or applied to the wrong provider, and a round-down.
#[test]
fn each_provider_rounds_the_time_a_machine_ran_as_its_terms_say() {
    let table = [
        (Billing::Lambda, 0, 60_000),
        (Billing::Lambda, 1, 60_000),
        (Billing::Lambda, 60_000, 60_000),
        (Billing::Lambda, 60_001, 120_000),
        (Billing::Lambda, 125_000, 180_000),
        (Billing::Aws, 0, 60_000),
        (Billing::Aws, 59_999, 60_000),
        (Billing::Aws, 61_000, 61_000),
        (Billing::Aws, 61_001, 62_000),
        (Billing::Gce, 0, 0),
        (Billing::Gce, 1, 1_000),
        (Billing::Gce, 1_500, 2_000),
        (Billing::Gce, 61_001, 62_000),
        (Billing::Tpu, 1_500, 2_000),
    ];
    for (billing, run, billed) in table {
        assert_eq!(billing.billed_ms(run), billed, "{billing:?} ran {run} ms");
    }
}

/// Asserts the unit of a bill: five cents is five hours of a cent-an-hour price in
/// milliseconds, one unit over rounds up to the next cent, nothing is zero cents, and a
/// bill too large for a u64 of cents saturates and does not wrap. It catches a unit that
/// drifts from the price per hour and a conversion that rounds down.
#[test]
fn a_bill_counts_in_cent_milliseconds_and_rounds_up_to_cents() {
    assert_eq!(Bill::of_cents(5), Bill(5 * 3_600_000));
    assert_eq!(Bill::of_cents(5).cents_up(), 5);
    assert_eq!(Bill(5 * 3_600_000 + 1).cents_up(), 6);
    assert_eq!(Bill::default().cents_up(), 0);
    assert!(Bill(1) < Bill(2));
    assert_eq!(Bill(u128::MAX).cents_up(), u64::MAX);
}

/// Asserts that the cloud bills a machine that ran for a stated time at the granularity of
/// the provider it stands for: the same launch and release, three providers, three
/// different bills, each equal to the price times the hand-computed billed time. It
/// catches a bill that uses one rounding for every provider.
#[test]
fn the_cloud_bills_the_time_between_launch_and_release_at_its_providers_granularity() {
    let table = [
        (Billing::Lambda, 61_500, 120_000),
        (Billing::Lambda, 10_000, 60_000),
        (Billing::Aws, 61_500, 62_000),
        (Billing::Aws, 10_000, 60_000),
        (Billing::Gce, 61_500, 62_000),
        (Billing::Gce, 10_000, 10_000),
        (Billing::Tpu, 10_000, 10_000),
    ];
    for (billing, run, billed) in table {
        let cloud = cloud(billing);
        let id = launch(&cloud, "a1");
        cloud.clock().advance(Duration::from_millis(run));
        cloud.release(&id);
        assert_eq!(cloud.bill(), owed(billed), "{billing:?} ran {run} ms");
        assert_eq!(cloud.bill_of(&id), owed(billed));
        assert_eq!(
            cloud.bill_of(&HostRef("nobody".to_owned())),
            Bill::default()
        );
    }
}

/// Asserts that a machine still running is billed as if it ended now, including the
/// increment it has started, that the bill grows as the clock moves, and that a release
/// stops it for good. It catches a bill that leaves a running machine out, one that does
/// not grow, and one that keeps growing after the release.
#[test]
fn a_running_machine_is_billed_to_now_and_a_release_stops_the_bill() {
    let cloud = cloud(Billing::Lambda);
    assert_eq!(cloud.bill(), Bill::default());
    let id = launch(&cloud, "a1");
    cloud.clock().advance(secs(30));
    assert_eq!(cloud.bill(), owed(60_000));
    cloud.clock().advance(secs(70));
    assert_eq!(cloud.bill(), owed(120_000));
    cloud.release(&id);
    cloud.clock().advance(mins(60));
    assert_eq!(cloud.bill(), owed(120_000));
}

/// Asserts that the bill of a window counts only the billed time inside it: a machine that
/// ran 150 s on Lambda is billed from its launch to 180 s, so the window of its second
/// minute holds 60 s, a window that opens at 170 s holds the last 10 s, a window before the
/// launch holds nothing, and the window of everything holds all of it. It catches a window
/// that counts a machine whole, and one that ignores the rounded-up tail.
#[test]
fn the_bill_of_a_window_counts_the_billed_time_inside_it() {
    let cloud = cloud(Billing::Lambda);
    let id = launch(&cloud, "a1");
    cloud.clock().advance(secs(150));
    cloud.release(&id);
    cloud.clock().advance(mins(10));
    assert_eq!(
        cloud.bill_between(START + 60_000, START + 120_000),
        owed(60_000)
    );
    assert_eq!(
        cloud.bill_between(START + 170_000, START + 500_000),
        owed(10_000)
    );
    assert_eq!(cloud.bill_between(0, START), Bill::default());
    assert_eq!(cloud.bill_between(0, u64::MAX), owed(180_000));
    assert_eq!(cloud.bill(), owed(180_000));
}

/// Asserts that the cloud bills at the price in its own catalog and not at the price the
/// caller believes: an acquire that carries a price of 9999 cents an hour, or none, is
/// billed at the catalog's. It catches a bill read from the request, which is where the
/// scaler's own number would leak in.
#[test]
fn a_machine_is_billed_at_the_catalog_price_not_the_one_the_caller_believes() {
    for believed in [Some(9_999), None] {
        let cloud = cloud(Billing::Lambda);
        let held = common::accepted(cloud.acquire(&request_at("a1", believed)));
        cloud.clock().advance(mins(1));
        cloud.release(&held.id);
        assert_eq!(cloud.bill(), owed(60_000), "believed {believed:?}");
    }
}

/// Asserts that a second acquire of the same id makes no second machine and answers the
/// first one, that a machine already released does not count (the same id then makes a
/// new one), and that the machine carries the name, the offer and the zone it was asked
/// for and the user data it was given. It catches a double launch from a retry.
#[test]
fn an_acquire_of_the_same_id_finds_its_machine_and_launches_nothing() {
    let cloud = cloud(Billing::Lambda);
    let first = common::accepted(cloud.acquire(&request("a1")));
    let again = common::accepted(cloud.acquire(&request("a1")));
    assert_eq!(first.id, again.id);
    assert_eq!(cloud.launched(), 1);
    assert_eq!(first.acquire, "a1");
    assert_eq!(first.offer, "a10");
    assert_eq!(first.zone.as_deref(), Some("z1"));
    assert_eq!(first.status, HostStatus::Booting);
    assert_eq!(first.name.as_deref(), Some(request("a1").name.as_str()));
    assert_eq!(cloud.user_data(0), "user-data-MARKER");
    cloud.release(&first.id);
    let new = common::accepted(cloud.acquire(&request("a1")));
    assert_ne!(new.id, first.id);
    assert_eq!(cloud.launched(), 2);
}

/// Asserts a capacity refusal followed by a success: the scripted refusal makes nothing and
/// reads as `Capacity`, and the same acquire sent again launches the machine. It catches a
/// refusal that leaves a machine behind, and a refusal that sticks.
#[test]
fn a_capacity_refusal_makes_nothing_and_the_next_acquire_succeeds() {
    let cloud = cloud(Billing::Lambda);
    cloud.script(Call::Acquire, &[Step::Refuse(RefusalCode::Capacity)]);
    let answer = cloud.acquire(&request("a1"));
    assert_eq!(refused(&answer), RefusalCode::Capacity);
    assert_eq!(
        answer,
        Acquired::Refused(Refusal {
            code: RefusalCode::Capacity,
            reason: Reason::Api("scripted".to_owned()),
        })
    );
    assert_eq!(cloud.launched(), 0);
    assert_eq!(cloud.bill(), Bill::default());
    common::accepted(cloud.acquire(&request("a1")));
    assert_eq!(cloud.launched(), 1);
}

/// Asserts that every refusal the cloud can script comes back as its code and makes
/// nothing: quota, denial and a request that does not fit. It catches a code mapped to
/// another.
#[test]
fn a_scripted_quota_denial_or_shape_refusal_reads_as_its_code_and_makes_nothing() {
    for code in [
        RefusalCode::Quota,
        RefusalCode::Denied,
        RefusalCode::Shape,
        RefusalCode::Capacity,
    ] {
        let cloud = cloud(Billing::Lambda);
        cloud.script(Call::Acquire, &[Step::Refuse(code)]);
        assert_eq!(refused(&cloud.acquire(&request("a1"))), code);
        assert_eq!(cloud.launched(), 0);
    }
}

/// Asserts that an offer the catalog does not list is refused as a request that does not fit
/// (`Shape`, not allowed) with nothing made, and that the same acquire of a listed offer
/// goes through. It catches a cloud that launches what it does not sell.
#[test]
fn an_offer_the_catalog_does_not_list_is_refused_as_shape() {
    let cloud = cloud(Billing::Lambda);
    let mut other = request("a1");
    other.offer.id = "h100".to_owned();
    assert_eq!(
        cloud.acquire(&other),
        Acquired::Refused(Refusal {
            code: RefusalCode::Shape,
            reason: Reason::NotAllowed,
        })
    );
    let mut elsewhere = request("a1");
    elsewhere.offer.zone = Some("z9".to_owned());
    assert_eq!(refused(&cloud.acquire(&elsewhere)), RefusalCode::Shape);
    assert_eq!(cloud.launched(), 0);
    assert_eq!(cloud.catalog().unwrap(), vec![offer(Some(PRICE))]);
    common::accepted(cloud.acquire(&request("a1")));
}

/// Asserts the two limits of the cloud: a quota of one machine refuses the second as
/// `Quota` while the first runs and allows it once the first is released, and room for one
/// refuses the second as `Capacity` the same way. It catches a limit that counts released
/// machines and one that does not count running ones.
#[test]
fn a_quota_and_the_cloud_s_room_count_running_machines_only() {
    for (quota, capacity, code) in [
        (Some(1), None, RefusalCode::Quota),
        (None, Some(1), RefusalCode::Capacity),
    ] {
        let cloud = cloud(Billing::Lambda);
        let mut knobs = cloud.knobs();
        knobs.quota = quota;
        knobs.capacity = capacity;
        cloud.set_knobs(knobs);
        let first = launch(&cloud, "a1");
        assert_eq!(refused(&cloud.acquire(&request("a2"))), code);
        assert_eq!(cloud.launched(), 1);
        cloud.release(&first);
        common::accepted(cloud.acquire(&request("a2")));
        assert_eq!(cloud.launched(), 2);
    }
}

/// Asserts a lost answer that landed: the caller reads `Unknown`, the machine exists, the
/// acquire id is unresolved, a retry is deferred for the rest of the hold and launches
/// nothing, a list that shows the machine ends the hold, and the retry then finds the
/// machine. It catches an Unknown that is read as nothing happening, and a retry that
/// launches twice.
#[test]
fn a_lost_answer_that_landed_leaves_a_machine_and_an_unresolved_id_until_a_list_shows_it() {
    let cloud = cloud(Billing::Lambda);
    cloud.script(Call::Acquire, &[Step::Lost { landed: true }]);
    assert_eq!(cloud.acquire(&request("a1")), Acquired::Unknown);
    assert_eq!(cloud.launched(), 1);
    let id = AcquireId::new("a1").unwrap();
    assert_eq!(cloud.unresolved(), std::slice::from_ref(&id));
    assert_eq!(
        cloud.acquire(&request("a1")),
        Acquired::Deferred { wait: mins(30) }
    );
    cloud.clock().advance(mins(10));
    assert_eq!(
        cloud.acquire(&request("a1")),
        Acquired::Deferred { wait: mins(20) }
    );
    assert_eq!(cloud.launched(), 1);
    let listed = cloud.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert!(cloud.unresolved().is_empty());
    let found = common::accepted(cloud.acquire(&request("a1")));
    assert_eq!(found.id, listed[0].id);
    assert_eq!(cloud.launched(), 1);
}

/// Asserts a lost answer that did not land: `Unknown`, nothing exists, the id is unresolved
/// until the hold runs out (a list that shows nothing does not end it), and then the same
/// acquire launches. It catches a hold that never ends and one that ends on an empty list.
#[test]
fn a_lost_answer_that_did_not_land_holds_the_id_until_the_hold_runs_out() {
    let cloud = cloud(Billing::Lambda);
    cloud.script(Call::Acquire, &[Step::Lost { landed: false }]);
    assert_eq!(cloud.acquire(&request("a1")), Acquired::Unknown);
    assert_eq!(cloud.launched(), 0);
    assert!(cloud.list().unwrap().is_empty());
    assert_eq!(cloud.unresolved().len(), 1);
    cloud.clock().advance(mins(29));
    assert_eq!(cloud.unresolved().len(), 1);
    cloud.clock().advance(mins(2));
    assert!(cloud.unresolved().is_empty());
    common::accepted(cloud.acquire(&request("a1")));
    assert_eq!(cloud.launched(), 1);
}

/// Asserts that a program that restarts forgets the acquires it has no answer for and the
/// machines stay: after the restart nothing is unresolved and the landed machine is still
/// running and still billing. It catches a restart that loses machines.
#[test]
fn a_restart_of_the_caller_forgets_unresolved_ids_and_not_machines() {
    let cloud = cloud(Billing::Lambda);
    cloud.script(Call::Acquire, &[Step::Lost { landed: true }]);
    cloud.acquire(&request("a1"));
    assert_eq!(cloud.unresolved().len(), 1);
    cloud.forget_calls_in_flight();
    assert!(cloud.unresolved().is_empty());
    assert_eq!(cloud.running().len(), 1);
}

/// Asserts the 429 answers: an acquire is deferred by the fixed wait and makes nothing, a
/// list fails, and a release answers Unknown and ends nothing, so the machine keeps
/// billing. Then the storm ends and the same calls work. It catches a 429 that launches
/// or releases, and one that is read as an answer.
#[test]
fn a_429_storm_makes_no_machine_ends_none_and_passes_when_it_ends() {
    let cloud = cloud(Billing::Lambda);
    cloud.script_forever(Call::Acquire, Step::Throttled);
    for _ in 0..3 {
        assert_eq!(
            cloud.acquire(&request("a1")),
            Acquired::Deferred {
                wait: THROTTLE_WAIT
            }
        );
    }
    assert_eq!(cloud.launched(), 0);
    assert!(cloud.unresolved().is_empty());
    cloud.script_forever(Call::Acquire, Step::Pass);
    let id = launch(&cloud, "a1");

    cloud.script_forever(Call::List, Step::Throttled);
    cloud.script_forever(Call::Release, Step::Throttled);
    let error = cloud.list().unwrap_err();
    assert!(error.to_string().contains("429"), "{error}");
    cloud.clock().advance(mins(3));
    assert_eq!(cloud.release(&id), Released::Unknown);
    assert_eq!(cloud.running(), std::slice::from_ref(&id));
    assert_eq!(cloud.bill(), owed(180_000));

    cloud.script_forever(Call::List, Step::Pass);
    cloud.script_forever(Call::Release, Step::Pass);
    assert_eq!(cloud.list().unwrap().len(), 1);
    assert!(matches!(cloud.release(&id), Released::Terminating(_)));
    assert!(cloud.running().is_empty());
}

/// Asserts the three ways a list fails, each with its own words and none as an empty list,
/// that a queued step is used once and the repeated step after it, and that the calls are
/// all counted. It catches a failed list that reads as empty.
#[test]
fn a_list_that_fails_says_so_and_is_never_an_empty_list() {
    let cloud = cloud(Billing::Lambda);
    launch(&cloud, "a1");
    cloud.script(
        Call::List,
        &[
            Step::Refuse(RefusalCode::Denied),
            Step::Lost { landed: false },
            Step::Throttled,
        ],
    );
    let words: Vec<String> = (0..3)
        .map(|_| cloud.list().unwrap_err().to_string())
        .collect();
    assert_eq!(
        words,
        [
            "the list was refused",
            "the list timed out",
            "the list was throttled (429)"
        ]
    );
    assert_eq!(cloud.list().unwrap().len(), 1);
    cloud.script_forever(Call::List, Step::Lost { landed: false });
    assert!(cloud.list().is_err());
    assert!(cloud.list().is_err());
    assert_eq!(cloud.count(Call::List), 6);
}

/// Asserts a release that works: the first answer is `Terminating` (the machine reads
/// `terminating` in a list), a later one after the removal delay is `Gone`, the machine is
/// then out of the list, a read by id says gone, and nothing runs. It catches a release
/// that reads gone at once and one that never does.
#[test]
fn a_release_reads_terminating_and_then_gone_after_the_removal_delay() {
    let cloud = cloud(Billing::Lambda);
    let id = launch(&cloud, "a1");
    cloud.clock().advance(mins(5));
    assert!(!cloud.is_gone(&id));
    assert_eq!(
        cloud.release(&id),
        Released::Terminating(HostStatus::Terminating)
    );
    assert_eq!(cloud.list().unwrap()[0].status, HostStatus::Terminating);
    assert!(cloud.running().is_empty());
    assert!(!cloud.is_gone(&id));
    cloud.clock().advance(secs(20));
    assert!(cloud.is_gone(&id));
    assert_eq!(cloud.release(&id), Released::Gone);
    assert!(cloud.list().unwrap().is_empty());
}

/// Asserts that a second release does not move the end of billing: a machine released at
/// 90 s and released again an hour later is billed two minutes. It catches a release that
/// restarts the clock of the bill.
#[test]
fn releasing_twice_bills_to_the_first_release() {
    let cloud = cloud(Billing::Lambda);
    let id = launch(&cloud, "a1");
    cloud.clock().advance(secs(90));
    cloud.release(&id);
    cloud.clock().advance(mins(60));
    cloud.release(&id);
    assert_eq!(cloud.bill(), owed(120_000));
}

/// Asserts the release that is not the cloud's to make: an id it never issued is refused
/// as foreign and is never `Gone`. It catches a foreign id read as a machine that is gone.
#[test]
fn a_release_of_an_id_the_cloud_never_issued_is_refused_as_foreign() {
    let cloud = cloud(Billing::Lambda);
    launch(&cloud, "a1");
    let stranger = HostRef("i-not-ours".to_owned());
    assert_eq!(cloud.release(&stranger), Released::Refused(Claim::Foreign));
    assert_eq!(cloud.running().len(), 1);
    assert!(
        cloud.is_gone(&stranger),
        "a read by id of a stranger is a 404"
    );
}

/// Asserts a release refused forever: every release answers `Refused`, the machine stays
/// running and billing for as long as the refusal lasts, and it is never `Gone`. It
/// catches a refusal that ends the machine, and one that reads as gone.
#[test]
fn a_release_that_is_refused_forever_ends_nothing_and_the_bill_keeps_running() {
    let cloud = cloud(Billing::Lambda);
    let id = launch(&cloud, "a1");
    cloud.script_forever(Call::Release, Step::Refuse(RefusalCode::Denied));
    for hour in 1..=3 {
        cloud.clock().advance(mins(60));
        assert_eq!(cloud.release(&id), Released::Refused(Claim::Suspect));
        assert_eq!(cloud.running(), std::slice::from_ref(&id));
        assert!(!cloud.is_gone(&id));
        assert_eq!(cloud.bill(), owed(hour * 3_600_000));
    }
}

/// Asserts a release whose answer is lost: `Unknown` either way, and the effect is the
/// script's. One that landed has ended the machine (it stops billing and no longer runs),
/// one that did not leaves it running. The caller cannot tell the two apart from the
/// answer, which is the point. It catches an Unknown read as gone, and a lost release that
/// always lands.
#[test]
fn a_lost_release_answer_is_unknown_whether_or_not_it_landed() {
    for landed in [true, false] {
        let cloud = cloud(Billing::Lambda);
        let id = launch(&cloud, "a1");
        cloud.script(Call::Release, &[Step::Lost { landed }]);
        cloud.clock().advance(secs(30));
        assert_eq!(cloud.release(&id), Released::Unknown);
        assert_eq!(cloud.running().is_empty(), landed);
        cloud.clock().advance(mins(10));
        let expected = if landed { 60_000 } else { 11 * 60_000 };
        assert_eq!(cloud.bill(), owed(expected), "landed {landed}");
    }
}

/// Asserts preemption: the machine reads `preempted` in a list, stops running and billing
/// at the moment it was reclaimed (not later), still needs a release to be gone, and a
/// release afterwards does not move the end of the bill. Preempting an id the cloud does
/// not hold changes nothing, and preempting twice keeps the first time. It catches a
/// preempted machine that bills on, and one that vanishes without a release.
#[test]
fn a_preempted_machine_stops_billing_when_reclaimed_and_stays_until_released() {
    let cloud = cloud(Billing::Lambda);
    let id = launch(&cloud, "a1");
    cloud.clock().advance(mins(5));
    assert_eq!(cloud.list().unwrap()[0].status, HostStatus::Active);
    cloud.preempt(&HostRef("nobody".to_owned()));
    cloud.preempt(&id);
    cloud.clock().advance(mins(1));
    cloud.preempt(&id);
    assert_eq!(cloud.list().unwrap()[0].status, HostStatus::Preempted);
    assert!(cloud.running().is_empty());
    cloud.clock().advance(mins(60));
    assert_eq!(cloud.bill(), owed(5 * 60_000));
    assert!(!cloud.is_gone(&id));
    assert!(matches!(cloud.release(&id), Released::Terminating(_)));
    assert_eq!(cloud.bill(), owed(5 * 60_000));
    cloud.clock().advance(secs(20));
    assert_eq!(cloud.release(&id), Released::Gone);
}

/// Asserts that a machine reads `booting` for the boot time and `active` after, and that
/// the cloud's truth and an unlagged list agree. It catches a machine that is active
/// before it has booted.
#[test]
fn a_machine_boots_for_the_boot_time_and_then_reads_active() {
    let cloud = cloud(Billing::Lambda);
    launch(&cloud, "a1");
    cloud.clock().advance(secs(119));
    assert_eq!(cloud.truth()[0].status, HostStatus::Booting);
    cloud.clock().advance(secs(1));
    assert_eq!(cloud.truth()[0].status, HostStatus::Active);
    assert_eq!(cloud.truth(), cloud.list().unwrap());
}

/// Asserts a list that lags the truth by 90 s: a machine launched now is not in the list
/// yet though the cloud holds it, it appears once the lag has passed, showing the status it
/// had 90 s ago, and a released machine goes on showing as it was until the lag has passed
/// both its release and its removal. It catches a lag that hides nothing, one that shows
/// the truth, and one that never lets a release show.
#[test]
fn a_list_with_lag_shows_the_cloud_as_it_was_that_long_ago() {
    let cloud = cloud(Billing::Lambda);
    let mut knobs = cloud.knobs();
    knobs.list_lag = secs(90);
    cloud.set_knobs(knobs);
    let id = launch(&cloud, "a1");
    assert!(cloud.list().unwrap().is_empty());
    assert_eq!(cloud.truth().len(), 1);
    cloud.clock().advance(secs(90));
    assert_eq!(cloud.list().unwrap()[0].status, HostStatus::Booting);
    cloud.clock().advance(secs(210));
    assert_eq!(cloud.list().unwrap()[0].status, HostStatus::Active);
    cloud.release(&id);
    cloud.clock().advance(secs(30));
    assert!(cloud.truth().is_empty(), "removed in the truth");
    assert_eq!(cloud.list().unwrap()[0].status, HostStatus::Active);
    cloud.clock().advance(secs(70));
    assert_eq!(cloud.list().unwrap()[0].status, HostStatus::Terminating);
    cloud.clock().advance(secs(20));
    assert!(cloud.list().unwrap().is_empty());
}

/// Asserts the record of calls: every call of every kind is counted, in order, whether it
/// worked or not, which is what lets a test prove that a call was never made. It catches a
/// call that is not recorded.
#[test]
fn every_call_is_recorded_in_order() {
    let cloud = cloud(Billing::Lambda);
    let id = launch(&cloud, "a1");
    cloud.catalog().unwrap();
    assert!(cloud.checks().is_empty());
    cloud.list().unwrap();
    cloud.unresolved();
    cloud.is_gone(&id);
    cloud.release(&id);
    assert_eq!(
        cloud.calls(),
        [
            Call::Acquire,
            Call::Catalog,
            Call::Checks,
            Call::List,
            Call::Unresolved,
            Call::IsGone,
            Call::Release
        ]
    );
    assert_eq!(cloud.count(Call::Acquire), 1);
    assert_eq!(cloud.count(Call::Release), 1);
}

/// Asserts the plain parts: the limits and delays start at their defaults and read back as
/// set, a clone is the same cloud and shares its clock, the cloud has no host preparation
/// and states its payload limit, and a clock that is asked to move by more than it can
/// holds at its largest value. It catches a default that drifts and a clone that forks.
#[test]
fn the_knobs_the_clone_and_the_clock_behave() {
    let cloud = cloud(Billing::Aws);
    let knobs = Knobs::default();
    assert_eq!(cloud.knobs(), knobs);
    assert_eq!(knobs.boot, secs(120));
    assert_eq!(knobs.remove_lag, secs(20));
    assert_eq!(knobs.unknown_hold, mins(30));
    assert_eq!((knobs.quota, knobs.capacity), (None, None));
    assert_eq!(knobs.list_lag, Duration::ZERO);
    let copy: ElasticFake = cloud.clone();
    launch(&copy, "a1");
    assert_eq!(cloud.launched(), 1);
    copy.clock().advance(secs(1));
    assert_eq!(cloud.clock().now_ms(), START + 1_000);
    assert!(cloud.host_prep(&offer(None)).is_empty());
    assert_eq!(cloud.payload_limit(), PAYLOAD_LIMIT);
    let far = FakeClock::new(0);
    far.advance(Duration::MAX);
    assert_eq!(far.now_ms(), u64::MAX);
}
