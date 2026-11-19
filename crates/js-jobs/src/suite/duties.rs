//! The periodic duties of the servers over one store ([`crate::JobStore::claim_duty`]): one
//! server runs a duty for a period, and a duty that no server is due for is not run.

use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Clock, ManualClock, StoreTime};

use super::rig;

/// The `secs`-th second of the store's clock.
fn at(secs: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_secs(secs))
}

/// Moves `clock` on to its `secs`-th second (a clock never goes back, so a later call names a
/// later second).
fn to(clock: &ManualClock, secs: u64) {
    let now = clock.now();
    clock.advance(at(secs).since(now));
}

/// A duty is claimed by the one server that finds it due and advances its row: a second server
/// at the same moment, or any time before the period is out, is refused; at the period's end a
/// server claims it again; another duty is its own row; a period of zero claims every call; a
/// row that is not a time is as good as due. Catches a claim that two servers both win, one that
/// does not wait out its period, and one that lets a duty's claim stand for another's.
pub fn a_duty_is_claimed_by_one_server_for_each_period<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_duty_is_claimed_by_one_server_for_each_period";
    let rig = rig(backend);
    let other = rig.other();
    let period = Duration::from_secs(5);
    let claim = |jobs: &dyn Fn(&str, Duration) -> bool, duty: &str, secs: u64| {
        to(&rig.clock, secs);
        jobs(duty, period)
    };
    let first = |duty: &str, period: Duration| rig.jobs.claim_duty(duty, period).expect("answers");
    let second = |duty: &str, period: Duration| other.claim_duty(duty, period).expect("answers");

    assert!(
        claim(&first, "expire", 100),
        "{name}: nobody claimed a duty never run"
    );
    assert!(
        !claim(&second, "expire", 100),
        "{name}: two servers claimed one period"
    );
    assert!(
        !claim(&second, "expire", 104),
        "{name}: a duty was claimed before its period was out"
    );
    assert!(
        claim(&second, "expire", 105),
        "{name}: a duty due at the end of its period was refused"
    );
    assert!(
        !claim(&first, "expire", 105),
        "{name}: the period just claimed was claimed again"
    );
    assert!(
        claim(&first, "trim", 105),
        "{name}: one duty's claim stood for another's"
    );

    assert!(
        first("every", Duration::ZERO),
        "{name}: a period of zero did not claim"
    );
    assert!(
        second("every", Duration::ZERO),
        "{name}: a period of zero did not claim every call"
    );

    rig.put_raw("periodic/odd", vec![1, 2, 3]);
    assert!(
        claim(&first, "odd", 105),
        "{name}: a row that is not a time held the duty"
    );

    // The row is read due by one server and claimed by another before the first commits: the
    // first loses.
    let racer = rig.other();
    rig.store.before_next(move || {
        assert!(
            racer
                .claim_duty("race", Duration::from_secs(5))
                .expect("answers")
        );
    });
    assert!(
        !claim(&first, "race", 200),
        "{name}: a claim that lost the race to another server won"
    );
}

/// A claim the store fails is an error and claims nothing: the duty is run by no one, and the
/// next call finds it due. So is a claim whose store cannot read its clock: it writes no row
/// (a due time stamped from no reading would hold the duty from every server). Catches a
/// failed claim taken for a won one, and a claim that judges or stamps the row without the
/// store's time.
pub fn a_failed_claim_is_an_error_and_claims_nothing<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_failed_claim_is_an_error_and_claims_nothing";
    let rig = rig(backend);
    to(&rig.clock, 100);
    rig.store.fail_next();
    let failed = rig.jobs.claim_duty("expire", Duration::from_secs(5));
    assert!(failed.is_err(), "{name}: {failed:?}");
    rig.store.fail_clock(true);
    let unclocked = rig.jobs.claim_duty("expire", Duration::from_secs(5));
    assert!(unclocked.is_err(), "{name}: {unclocked:?}");
    assert_eq!(
        rig.raw("periodic/expire"),
        None,
        "{name}: a claim without the store's time wrote the duty's row"
    );
    rig.store.fail_clock(false);
    let won = rig
        .jobs
        .claim_duty("expire", Duration::from_secs(5))
        .expect("answers");
    assert!(won, "{name}: a failed claim held the duty");
}

/// The duty's due time is the store's clock plus the period, and it is judged by the store's
/// clock: a claim made when the store's clock reads 100 s holds the duty until it reads 105 s,
/// whatever any server's own clock says (the claim takes no time from its caller). Catches a
/// due time stamped or compared with another clock than the store's.
pub fn a_duty_is_due_by_the_store_clock<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_duty_is_due_by_the_store_clock";
    let rig = rig(backend);
    to(&rig.clock, 100);
    let period = Duration::from_secs(5);
    assert!(rig.jobs.claim_duty("expire", period).expect("answers"));
    let row = rig.raw("periodic/expire").expect("the claim wrote the row");
    let micros = u64::try_from(at(105).as_duration().as_micros()).expect("fits");
    assert_eq!(
        row.value,
        micros.to_be_bytes().to_vec(),
        "{name}: the due time is not the store's clock plus the period"
    );
    to(&rig.clock, 104);
    assert!(
        !rig.jobs.claim_duty("expire", period).expect("answers"),
        "{name}: claimed before the store's clock reached the due time"
    );
    to(&rig.clock, 105);
    assert!(
        rig.jobs.claim_duty("expire", period).expect("answers"),
        "{name}: refused once the store's clock reached the due time"
    );
}
