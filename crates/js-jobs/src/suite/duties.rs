//! The periodic duties of the servers over one store ([`crate::JobStore::claim_duty`]): one
//! server runs a duty for a period, and a duty that no server is due for is not run.

use std::time::Duration;

use js_state::StoreTime;
use js_state::conformance::Backend;

use super::rig;

/// The `secs`-th second of the store's clock.
fn at(secs: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_secs(secs))
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
    let claim = |jobs: &dyn Fn(&str, StoreTime, Duration) -> bool, duty: &str, secs: u64| {
        jobs(duty, at(secs), period)
    };
    let first = |duty: &str, now: StoreTime, period: Duration| {
        rig.jobs.claim_duty(duty, now, period).expect("answers")
    };
    let second = |duty: &str, now: StoreTime, period: Duration| {
        other.claim_duty(duty, now, period).expect("answers")
    };

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
        claim(&first, "trim", 100),
        "{name}: one duty's claim stood for another's"
    );

    assert!(
        first("every", at(100), Duration::ZERO) && second("every", at(100), Duration::ZERO),
        "{name}: a period of zero did not claim every call"
    );

    rig.put_raw("periodic/odd", vec![1, 2, 3]);
    assert!(
        claim(&first, "odd", 100),
        "{name}: a row that is not a time held the duty"
    );

    // The row is read due by one server and claimed by another before the first commits: the
    // first loses.
    let racer = rig.other();
    rig.store.before_next(move || {
        assert!(
            racer
                .claim_duty("race", at(200), Duration::from_secs(5))
                .expect("answers")
        );
    });
    assert!(
        !claim(&first, "race", 200),
        "{name}: a claim that lost the race to another server won"
    );
}

/// A claim the store fails is an error and claims nothing: the duty is run by no one, and the
/// next call finds it due. Catches a failed claim taken for a won one.
pub fn a_failed_claim_is_an_error_and_claims_nothing<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_failed_claim_is_an_error_and_claims_nothing";
    let rig = rig(backend);
    rig.store.fail_next();
    let failed = rig
        .jobs
        .claim_duty("expire", at(100), Duration::from_secs(5));
    assert!(failed.is_err(), "{name}: {failed:?}");
    let won = rig
        .jobs
        .claim_duty("expire", at(100), Duration::from_secs(5))
        .expect("answers");
    assert!(won, "{name}: a failed claim held the duty");
}
