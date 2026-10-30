//! The stuck states of a cloud, against the elastic fake, run for simulated days on a clock
//! the test moves: a list that never works, a release that is never accepted, an answer that
//! is never given, a cloud that throttles everything, and a list that lags the truth. What
//! each must not do is spend: launch blind, launch twice, or read "no answer" as "gone".

mod cloud_rig;
mod common;

use cloud_rig::{DAY_MS, Sim};
use common::{minutes, settings};
use js_elastic::RefusalCode;
use js_elastic_fake::{Billing, Call, Step};
use js_scaler::{Blocked, Phase, Settings};

const DAY_TICKS: usize = 24 * 60;

fn pool() -> Settings {
    Settings {
        idle: minutes(5),
        ..settings("gpu")
    }
}

/// Asserts a list that fails for a day, in each way it can fail (refused, timed out,
/// throttled): with one machine registered and two requests waiting, the pool launches
/// nothing for the whole day and says why on every tick (`ListFailed`), because what exists
/// is not known, and the failed list is never read as an empty one (the machine's record
/// stays). When the list works again the second request is launched for. It catches a
/// launch made blind, and a failed list read as "nothing is there" (which would have the
/// scaler launch a machine for every tick).
#[test]
fn a_list_that_fails_forever_blocks_every_launch_and_says_why() {
    let failures = [
        Step::Refuse(RefusalCode::Denied),
        Step::Lost { landed: false },
        Step::Throttled,
    ];
    for failure in failures {
        let mut sim = Sim::new(pool(), Billing::Lambda);
        sim.waiting = 1;
        sim.run(6);
        assert_eq!(sim.phases(), [Phase::Registered]);
        assert_eq!(sim.acquires(), 1);

        sim.fake.script_forever(Call::List, failure);
        sim.waiting = 2;
        for tick in 0..DAY_TICKS {
            sim.tick(None);
            assert_eq!(
                sim.blocked(),
                Some(Blocked::ListFailed),
                "{failure:?} tick {tick}"
            );
        }
        assert_eq!(sim.acquires(), 1, "{failure:?}: a launch was made blind");
        assert_eq!(sim.fake.launched(), 1);
        assert_eq!(sim.phases(), [Phase::Registered]);

        sim.fake.script_forever(Call::List, Step::Pass);
        sim.run(5);
        assert_eq!(sim.fake.launched(), 2, "{failure:?}");
    }
}

/// Asserts a release that is never accepted, in each way a cloud can fail to accept it
/// (refused, throttled, answer lost with the release not landed): the record stays
/// `Releasing` for a whole day, the release is asked for again on every tick, the machine
/// goes on running and the cloud's bill for the day is exactly a day of the price (1,440
/// cents at a cent a minute), the count stays held (with `max_instances` of one, a new
/// request cannot launch, and the pool says `MaxInstances`), and the record is never `Gone`
/// while the machine runs. When the release is accepted the record goes `Gone` within two
/// ticks and the waiting request is launched for. It catches an Unknown read as gone, a
/// count released with a machine still billing, and a release that stops being tried.
#[test]
fn a_release_that_is_never_accepted_keeps_the_count_held_and_the_machine_billing() {
    let stuck = [
        Step::Refuse(RefusalCode::Denied),
        Step::Throttled,
        Step::Lost { landed: false },
    ];
    for step in stuck {
        let settings = Settings {
            max_instances: 1,
            ..pool()
        };
        let mut sim = Sim::new(settings, Billing::Lambda);
        sim.waiting = 1;
        sim.run(6);
        assert_eq!(sim.phases(), [Phase::Registered]);

        sim.fake.script_forever(Call::Release, step);
        sim.waiting = 0;
        sim.run(DAY_TICKS);
        assert_eq!(sim.phases(), [Phase::Releasing], "{step:?}");
        assert_eq!(sim.fake.running().len(), 1, "{step:?}");
        assert!(sim.gone_but_running().is_empty(), "{step:?}");
        let asked = sim.fake.count(Call::Release);
        assert!(asked > DAY_TICKS - 30, "{step:?}: asked {asked} times");

        sim.waiting = 1;
        sim.run(60);
        assert_eq!(sim.fake.launched(), 1, "{step:?}");
        assert_eq!(sim.blocked(), Some(Blocked::MaxInstances), "{step:?}");
        let now = sim.clock.now_ms();
        let day = sim.fake.bill_between(now - DAY_MS, now);
        assert_eq!(
            day.cents_up(),
            24 * 60,
            "{step:?}: a day at a cent a minute"
        );

        sim.fake.script_forever(Call::Release, Step::Pass);
        sim.run(3);
        assert!(
            sim.phases().contains(&Phase::Gone),
            "{step:?}: {:?}",
            sim.phases()
        );
        assert_eq!(sim.fake.launched(), 2, "{step:?}");
        assert_eq!(sim.fake.running().len(), 1, "{step:?}");
        assert!(sim.gone_but_running().is_empty());
    }
}

/// Asserts a storm of answers that never come, in both ways the cloud can behave: every
/// acquire is answered `Unknown`, and the machine does exist (landed), or does not. With
/// four requests waiting and four machines allowed, for a day: the cloud never runs more than
/// the four; no record is read as gone while its machine runs; when the machines landed
/// they are found by the list and used, and the cloud made exactly four; when none landed
/// each unanswered launch holds its slot for the `unknown_hold` before it is given up on,
/// so the cloud is asked at most four slots times 48 half-hours a day. It catches an
/// Unknown read as "nothing happened" (a launch every tick) and as "gone".
#[test]
fn an_unknown_storm_never_runs_more_than_the_cap_and_never_reads_a_machine_as_gone() {
    for landed in [true, false] {
        let mut sim = Sim::new(pool(), Billing::Lambda);
        sim.fake
            .script_forever(Call::Acquire, Step::Lost { landed });
        sim.waiting = 4;
        for tick in 0..DAY_TICKS {
            sim.tick(None);
            let counted = sim
                .records()
                .iter()
                .filter(|record| record.counts())
                .count();
            assert!(
                counted <= 4,
                "landed {landed} tick {tick}: {counted} records count"
            );
            assert!(sim.fake.running().len() <= 4);
            assert!(
                sim.gone_but_running().is_empty(),
                "landed {landed} tick {tick}"
            );
        }
        match landed {
            true => {
                assert_eq!(sim.fake.launched(), 4);
                assert_eq!(sim.acquires(), 4);
                assert_eq!(sim.fake.running().len(), 4);
                assert!(sim.unrecorded().is_empty());
            }
            false => {
                assert_eq!(sim.fake.launched(), 0);
                assert!(sim.acquires() > 4, "{}", sim.acquires());
                assert!(sim.acquires() <= 4 * 48, "{}", sim.acquires());
            }
        }
    }
}

/// Asserts a 429 storm on launches: the cloud answers every acquire with a deferral. For a
/// day, nothing is launched, the scaler keeps one record for the one request and retries it
/// under the same acquire id (a record that is retried is not a new launch), asks the cloud
/// no more than once a tick, and the day's cap is not eaten by the retries. When the storm
/// ends the machine that is launched carries that same acquire id. It catches a retry that
/// makes a record per attempt (a hole in the count), one that changes the acquire id (a
/// double launch if one had landed), and one that hammers the cloud.
#[test]
fn a_429_storm_on_launches_retries_one_record_under_one_acquire_id() {
    let mut sim = Sim::new(pool(), Billing::Lambda);
    sim.fake.script_forever(Call::Acquire, Step::Throttled);
    sim.waiting = 1;
    sim.run(2);
    let id = sim.records()[0].acquire.clone().unwrap();
    for tick in 0..DAY_TICKS {
        sim.tick(None);
        assert_eq!(sim.records().len(), 1, "tick {tick}");
        assert_eq!(sim.records()[0].acquire.as_ref(), Some(&id), "tick {tick}");
    }
    assert_eq!(sim.fake.launched(), 0);
    assert!(sim.acquires() > DAY_TICKS / 3, "{}", sim.acquires());
    assert!(sim.acquires() <= DAY_TICKS + 2, "{}", sim.acquires());

    sim.fake.script_forever(Call::Acquire, Step::Pass);
    sim.run(8);
    assert_eq!(sim.fake.launched(), 1);
    assert_eq!(sim.fake.truth()[0].acquire, id.as_str());
    assert_eq!(sim.phases(), [Phase::Registered]);
}

/// Asserts an eventually consistent list that lags shorter than the hold: a launch whose
/// answer was lost (and landed) is not in the list for 0, 5, 10 or 25 minutes, and the
/// record, which counts and covers the request, waits for it. One machine, never a second,
/// and it is registered. It catches a scaler that launches again because the list does not
/// show a machine yet.
#[test]
fn a_list_that_lags_less_than_the_hold_does_not_make_a_second_machine() {
    for lag in [0, 5, 10, 25] {
        let mut sim = Sim::new(pool(), Billing::Lambda);
        let mut knobs = sim.fake.knobs();
        knobs.list_lag = minutes(lag);
        sim.fake.set_knobs(knobs);
        sim.fake
            .script(Call::Acquire, &[Step::Lost { landed: true }]);
        sim.waiting = 1;
        sim.run(120);
        assert_eq!(sim.fake.launched(), 1, "lag {lag} minutes");
        assert_eq!(sim.phases(), [Phase::Registered], "lag {lag} minutes");
        assert!(sim.unrecorded().is_empty());
    }
}

/// Asserts the limit of that: a list that lags longer than the hold (45 minutes against 30)
/// after an answer that was lost and landed. The record is given up on at the hold, a second
/// machine is launched, and when the list finally shows the first the scaler finds a Failed
/// record's machine and releases it. In the end the cloud runs one machine, made two,
/// every machine is a record, and the release was a call to the first. It catches a late
/// machine that is left running, and one that is released while its record is not the one
/// that failed.
#[test]
fn a_list_that_lags_longer_than_the_hold_launches_a_second_machine_and_releases_the_first() {
    let mut sim = Sim::new(pool(), Billing::Lambda);
    let mut knobs = sim.fake.knobs();
    knobs.list_lag = minutes(45);
    sim.fake.set_knobs(knobs);
    sim.fake
        .script(Call::Acquire, &[Step::Lost { landed: true }]);
    sim.waiting = 1;
    let first = {
        sim.tick(None);
        sim.fake.truth()[0].id.clone()
    };
    sim.run(180);
    assert_eq!(sim.fake.launched(), 2);
    assert_eq!(sim.fake.running().len(), 1);
    assert!(
        !sim.fake.running().contains(&first),
        "the first machine was kept"
    );
    assert!(sim.fake.count(Call::Release) >= 1);
    assert!(sim.unrecorded().is_empty());
    assert!(sim.gone_but_running().is_empty());
}

/// Asserts what a list that lags a release does, as the scaler behaves today: the machine is
/// released and confirmed gone by a read by id, then the lagging list still shows it, and a
/// record that is gone does not know it, so it is counted again as an orphan. With
/// `max_instances` of one, that phantom holds the slot (`MaxInstances`) until two lists that
/// no longer show it are a hold apart; it is never released (the cloud sees no release call
/// after the machine's own), and then a waiting request is launched for. This pins the cost
/// of a lag, a pool slot and an orphan alert for about 40 minutes per release, so a change
/// that makes the scaler remember a released machine moves this test with it.
#[test]
fn a_list_that_lags_a_release_shows_a_phantom_orphan_until_two_lists_settle_it() {
    let settings = Settings {
        max_instances: 1,
        ..pool()
    };
    let mut sim = Sim::new(settings, Billing::Lambda);
    let mut knobs = sim.fake.knobs();
    knobs.list_lag = minutes(10);
    sim.fake.set_knobs(knobs);
    sim.waiting = 1;
    sim.run(20);
    assert_eq!(sim.phases(), [Phase::Registered]);

    sim.waiting = 0;
    let released = (0..60).any(|_| {
        sim.tick(None);
        sim.phases().first() == Some(&Phase::Gone)
    });
    assert!(released, "{:?}", sim.phases());
    let releases = sim.fake.count(Call::Release);
    let phantom = (0..20).any(|_| {
        sim.tick(None);
        sim.phases().contains(&Phase::Orphan)
    });
    assert!(phantom, "{:?}", sim.phases());

    sim.waiting = 1;
    sim.tick(None);
    assert_eq!(sim.blocked(), Some(Blocked::MaxInstances));
    assert_eq!(sim.fake.launched(), 1);
    sim.run(120);
    assert_eq!(sim.fake.launched(), 2);
    assert_eq!(
        sim.fake.count(Call::Release),
        releases,
        "the phantom was released"
    );
    assert!(sim.unrecorded().is_empty());
}
