//! The caps: instances, spend per hour and spend per day, each at its exact edge, and
//! what counts toward them.
//!
//! The pool's one offer costs 60 cents an hour, a cent a minute, and `boot_timeout` is 15
//! minutes, so a launch costs at least 15 cents against the day cap.

mod common;

use common::*;
use js_elastic::{Held, HostRef, HostStatus};
use js_scaler::{Blocked, Phase, Settings, StoredOffer};

/// A pool "gpu" holding `records`, one request waiting. The records are of machines too
/// small for the request, so none of them covers it.
fn rig_with(limits: Settings, mut records: Vec<js_scaler::Record>) -> Rig {
    let mut rig = Rig::new(limits);
    for each in &mut records {
        each.offer.cpu_millis = 1000;
    }
    rig.state.records.instances = records;
    rig.input.waiting = vec![request("r1")];
    rig
}

fn launches(rig: &mut Rig, now: u64) -> usize {
    acquires(&rig.tick(at(now))).len()
}

fn orphan(id: &str) -> Held {
    Held {
        id: HostRef(id.to_owned()),
        acquire: String::new(),
        name: Some(format!("stray-{id}")),
        offer: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        status: HostStatus::Active,
    }
}

/// Asserts the instance cap at its edge with an unresolved acquire and an orphan both
/// counted: two held machines block a cap of two and leave room in a cap of three.
#[test]
fn an_unresolved_acquire_and_an_orphan_count_toward_the_instance_cap() {
    for (cap, expected) in [(2, 0), (3, 1)] {
        let mut limits = settings("gpu");
        limits.max_instances = cap;
        let mut rig = rig_with(limits, vec![record(Phase::Unresolved, 60, 0)]);
        rig.pool().held = Some(vec![orphan("i-stray")]);
        assert_eq!(launches(&mut rig, MINUTE), expected, "cap {cap}");
        assert_eq!(rig.state.records.instances[1].phase, Phase::Orphan);
    }
}

/// Asserts that when the list failed nothing launches, and that the same input with a
/// list launches.
#[test]
fn a_failed_list_blocks_launches() {
    let mut rig = rig_with(settings("gpu"), Vec::new());
    rig.pool().held = None;
    assert_eq!(launches(&mut rig, 0), 0);
    assert_eq!(rig.state.blocked["gpu"], Blocked::ListFailed);
    rig.pool().held = Some(Vec::new());
    assert_eq!(launches(&mut rig, 5 * SECOND), 1);
}

/// Asserts the hourly cap at its edge, and that the sum is over the prices stored in the
/// records: a held machine stored at 100 and a candidate at 60 fit a cap of 160 and not
/// 159, though the catalog price of the held one (60) would fit both.
#[test]
fn the_hourly_cap_sums_stored_prices_to_its_edge() {
    for (cap, expected, shown) in [(160, 1, None), (159, 0, Some(Blocked::MaxSpend))] {
        let mut limits = settings("gpu");
        limits.max_spend_per_hour = cap;
        let mut rig = rig_with(limits, vec![record(Phase::Booting, 100, 0)]);
        assert_eq!(launches(&mut rig, MINUTE), expected, "cap {cap}");
        assert_eq!(rig.state.blocked.get("gpu").copied(), shown);
    }
}

/// Asserts the day cap at its edge for a boot that failed: a machine gone two minutes
/// after launch without a hello is billed `boot_timeout` (15 cents), and with the
/// candidate's own 15 a cap of 30 allows a launch and a cap of 29 does not.
#[test]
fn a_failed_boot_counts_for_boot_timeout_against_the_day_cap() {
    for (cap, expected, shown) in [(30, 1, None), (29, 0, Some(Blocked::MaxSpendDay))] {
        let mut limits = settings("gpu");
        limits.max_spend_per_day = cap;
        let mut failed = record(Phase::Gone, 60, 0);
        failed.gone_at = Some(at(2 * MINUTE));
        let mut rig = rig_with(limits, vec![failed]);
        assert_eq!(launches(&mut rig, HOUR), expected, "cap {cap}");
        assert_eq!(rig.state.blocked.get("gpu").copied(), shown);
    }
}

/// Asserts that a machine that registered is billed for the time it ran and not for
/// `boot_timeout`: two minutes cost 2 cents, so a cap of 17 allows a launch and 16 does
/// not.
#[test]
fn a_machine_that_registered_is_billed_for_its_time() {
    for (cap, expected) in [(17, 1), (16, 0)] {
        let mut limits = settings("gpu");
        limits.max_spend_per_day = cap;
        let mut ran = record(Phase::Gone, 60, 0);
        ran.registered = true;
        ran.gone_at = Some(at(2 * MINUTE));
        let mut rig = rig_with(limits, vec![ran]);
        assert_eq!(launches(&mut rig, HOUR), expected, "cap {cap}");
    }
}

/// Asserts that a record that never reaches Gone keeps accruing: a registered machine
/// launched an hour ago and still held costs 60 cents, so with the candidate's 15 a cap
/// of 75 allows a launch and 74 blocks the next one.
#[test]
fn a_machine_that_is_never_released_keeps_eating_the_day_cap() {
    for (cap, expected) in [(75, 1), (74, 0)] {
        let mut limits = settings("gpu");
        limits.max_spend_per_day = cap;
        let mut stuck = record(Phase::Registered, 60, 0);
        stuck.registered = true;
        let mut rig = rig_with(limits, vec![stuck]);
        assert_eq!(launches(&mut rig, HOUR), expected, "cap {cap}");
    }
}

/// Asserts that a machine found in a list and unknown to the records is billed from when
/// it was found, for at least a minute, at its offer's price: one cent, so the
/// candidate's 15 fit a cap of 16 and not 15.
#[test]
fn an_orphan_is_billed_from_the_step_that_found_it() {
    for (cap, expected) in [(16, 1), (15, 0)] {
        let mut limits = settings("gpu");
        limits.max_spend_per_day = cap;
        let mut rig = rig_with(limits, Vec::new());
        rig.pool().held = Some(vec![orphan("i-stray")]);
        assert_eq!(launches(&mut rig, 0), expected, "cap {cap}");
    }
}

/// Asserts that an orphan whose offer the catalog does not know is counted at the whole
/// hourly cap, which blocks every launch until a person looks, and keeps the offer id.
#[test]
fn an_orphan_of_an_unknown_offer_blocks_launches() {
    let mut rig = rig_with(settings("gpu"), Vec::new());
    let mut stray = orphan("i-stray");
    stray.offer = "mystery".to_owned();
    rig.pool().held = Some(vec![stray]);
    assert_eq!(launches(&mut rig, 0), 0);
    assert_eq!(rig.state.blocked["gpu"], Blocked::MaxSpend);
    assert_eq!(rig.record(0).price_cents_per_hour, 1000);
    assert_eq!(
        rig.record(0).offer,
        StoredOffer::nothing("mystery", Some("z1".to_owned()))
    );
}

/// Asserts what does not count: a refused acquire, a machine confirmed gone and a failed
/// acquire that two clean lists settled leave room in a cap of one machine; a failed
/// acquire one clean list short, and a launch the provider told to wait, hold it.
#[test]
fn only_machines_that_exist_or_may_exist_count() {
    let mut settled = record(Phase::Failed, 60, 0);
    settled.clean_lists = 2;
    let mut unsettled = record(Phase::Failed, 60, 0);
    unsettled.clean_lists = 1;
    unsettled.last_clean = Some(at(0));
    let mut small = record(Phase::Planned, 60, 0);
    small.offer.cpu_millis = 1000;
    small.retry_at = Some(at(DAY));
    let cases = [
        (record(Phase::Refused, 60, 0), 1),
        (record(Phase::Gone, 60, 0), 1),
        (settled, 1),
        (unsettled, 0),
        (small, 0),
    ];
    for (held, expected) in cases {
        let mut limits = settings("gpu");
        limits.max_instances = 1;
        let mut rig = rig_with(limits, vec![held]);
        assert_eq!(launches(&mut rig, 5 * SECOND), expected);
    }
}
