//! The failure brake against the elastic fake on a manual clock: the cooldown that doubles up
//! to a ceiling, the trip to plan after enough failures in a row, the success that starts the
//! count again, the operator's reset, and a restart in the middle of each. Nothing reads a
//! real clock or waits.

mod cloud_rig;
mod common;

use std::time::Duration;

use cloud_rig::{POOL, Sim, TICK};
use common::{MINUTE, at, count, minutes, request, settings};
use js_elastic::RefusalCode;
use js_elastic_fake::{Billing, Call, Step};
use js_scaler::{Blocked, Brake, Phase, Records, Settings};

/// A pool that trips after `trip` failures in a row and whose cooldown stops growing at
/// `ceiling`.
fn pool(trip: u32, ceiling: Duration) -> Settings {
    Settings {
        idle: minutes(5),
        boot_fail_trip_after: trip,
        boot_fail_cooldown_max: ceiling,
        ..settings(POOL)
    }
}

/// A sim whose cloud refuses every acquire with `code`, with one request waiting.
fn refused(settings: Settings, code: RefusalCode) -> Sim {
    let mut sim = Sim::new(settings, Billing::Lambda);
    sim.fake.script_forever(Call::Acquire, Step::Refuse(code));
    sim.waiting = 1;
    sim
}

/// Ticks until the cloud has been sent `n` acquires, then once more so the step that reads
/// the answer has run.
fn learn(sim: &mut Sim, n: usize) {
    for _ in 0..2000 {
        if sim.acquires() >= n {
            break;
        }
        sim.tick(None);
    }
    assert_eq!(sim.acquires(), n);
    sim.tick(None);
}

/// The instants, in milliseconds, at which acquires were sent during the next `ticks` ticks.
fn acquire_times(sim: &mut Sim, ticks: usize) -> Vec<u64> {
    let mut times = Vec::new();
    let mut seen = sim.acquires();
    for _ in 0..ticks {
        let now = sim.now().as_millis();
        sim.tick(None);
        if sim.acquires() > seen {
            times.push(now);
            seen = sim.acquires();
        }
    }
    times
}

fn brake(sim: &Sim) -> Option<Brake> {
    sim.rig.state.records.brake.get(POOL).copied()
}

/// How long the cooldown still has to run, in milliseconds.
fn remaining(sim: &Sim) -> u64 {
    let until = sim.rig.state.records.cooldowns[POOL].until;
    until.as_millis().saturating_sub(sim.now().as_millis())
}

/// Asserts the growth of the cooldown from the instants the cloud was asked: each refusal in
/// a row doubles the wait, from 10 minutes, and the wait stops at the ceiling (2 hours by
/// default, 30 minutes when the pool says). The gap between two acquires is the cooldown
/// plus the tick that read the refusal. It catches a cooldown that stays at 10 minutes (the
/// loop the brake exists to slow), one that doubles without end, and a ceiling that is
/// ignored.
#[test]
fn the_cooldown_doubles_from_ten_minutes_up_to_the_ceiling() {
    let cases = [
        (minutes(120), vec![10, 20, 40, 80, 120, 120]),
        (minutes(30), vec![10, 20, 30, 30]),
    ];
    for (ceiling, waits) in cases {
        let mut sim = refused(pool(100, ceiling), RefusalCode::Quota);
        let times = acquire_times(&mut sim, 500);
        assert!(times.len() > waits.len(), "{ceiling:?}: {times:?}");
        for (k, wait) in waits.iter().enumerate() {
            let gap = times[k + 1] - times[k];
            let tick = u64::try_from(TICK.as_millis()).unwrap();
            let low = wait * MINUTE + tick;
            let high = low + tick;
            assert!((low..=high).contains(&gap), "{ceiling:?}: gap {k} is {gap}");
        }
        assert!(brake(&sim).unwrap().tripped_at.is_none());
    }
}

/// Asserts the trip for each refusal that counts (quota, denied, a shape that fits
/// nothing): after three, the pool says it is tripped, is in plan, and the cloud is not
/// asked again for three simulated days; nothing is launched and the records are not written
/// again by quiet ticks. It catches a refusal that does not count, a trip that time undoes,
/// and a latch that costs a write per tick.
#[test]
fn three_refusals_in_a_row_trip_the_pool_and_the_cloud_is_never_asked_again() {
    for code in [RefusalCode::Quota, RefusalCode::Denied, RefusalCode::Shape] {
        let mut sim = refused(pool(3, minutes(120)), code);
        learn(&mut sim, 3);
        assert_eq!(sim.blocked(), Some(Blocked::Tripped), "{code:?}");
        assert!(sim.rig.state.records.tripped(POOL), "{code:?}");
        sim.run(60);
        let saves = sim.saves;
        sim.run(3 * 24 * 60);
        assert_eq!(sim.acquires(), 3, "{code:?}");
        assert_eq!(sim.fake.launched(), 0, "{code:?}");
        assert_eq!(sim.blocked(), Some(Blocked::Tripped), "{code:?}");
        assert_eq!(sim.saves, saves, "{code:?}: a quiet tick wrote the records");
    }
}

/// Asserts the trip when the failures are boots: no agent ever says hello, three machines are
/// launched and each is given up on and released, and then the pool launches nothing for 24
/// simulated hours while the request still waits, and the cloud holds no machine. It catches
/// a boot failure that does not count, and a trip that leaves a failed machine running.
#[test]
fn three_boots_that_never_say_hello_trip_the_pool() {
    let mut sim = Sim::new(pool(3, minutes(120)), Billing::Lambda);
    sim.mute = true;
    sim.waiting = 1;
    sim.run(4 * 60);
    assert_eq!(sim.acquires(), 3);
    assert_eq!(sim.blocked(), Some(Blocked::Tripped));
    sim.run(24 * 60);
    assert_eq!(sim.acquires(), 3);
    assert_eq!(sim.fake.launched(), 3);
    assert!(sim.fake.running().is_empty());
    assert!(brake(&sim).unwrap().tripped_at.is_some());
}

/// Asserts that a refusal for capacity is not a failure: two hundred ticks of it leave no
/// count, no cooldown and no trip. It catches a capacity refusal that feeds the brake.
#[test]
fn a_capacity_refusal_does_not_count() {
    let mut sim = refused(pool(1, minutes(120)), RefusalCode::Capacity);
    sim.run(200);
    assert!(sim.acquires() > 3);
    assert_eq!(brake(&sim), None);
    assert!(sim.rig.state.records.cooldowns.is_empty());
}

/// Asserts that one success starts the count again. Two refusals, then a machine that boots
/// and is heard from: the count is gone. Two more refusals for a second request then do not
/// trip a pool that trips at three, which four in a row would. It catches a counter that
/// never resets and one that resets on a launch instead of on a heard host.
#[test]
fn a_machine_that_boots_ends_the_failures_in_a_row() {
    let mut sim = Sim::new(pool(3, minutes(120)), Billing::Lambda);
    let refuse = Step::Refuse(RefusalCode::Quota);
    sim.fake.script(
        Call::Acquire,
        &[refuse, refuse, Step::Pass, refuse, refuse, Step::Pass],
    );
    sim.waiting = 1;
    learn(&mut sim, 2);
    assert_eq!(brake(&sim).unwrap().failures, 2);
    sim.run(60);
    assert_eq!(sim.phases(), [Phase::Registered]);
    assert_eq!(brake(&sim), None);

    sim.waiting = 2;
    sim.run(200);
    assert_eq!(sim.acquires(), 6);
    assert_eq!(sim.fake.launched(), 2);
    assert!(!sim.rig.state.records.tripped(POOL));
}

/// Asserts that a spot preemption of a booted machine counts toward the trip and starts no
/// cooldown: the failure is counted on the step that reads it, the replacement is asked for
/// on that same step, and the replacement being heard from ends the count. It catches a
/// preemption that is not counted and one that delays its replacement.
#[test]
fn a_preemption_counts_and_the_replacement_is_not_delayed() {
    let mut sim = Sim::new(pool(3, minutes(120)), Billing::Lambda);
    sim.waiting = 1;
    sim.run(6);
    assert_eq!(sim.phases(), [Phase::Registered]);
    let first = sim.fake.running()[0].clone();
    sim.fake.preempt(&first);
    sim.tick(None);
    assert_eq!(brake(&sim).unwrap().failures, 1);
    assert!(sim.rig.state.records.cooldowns.is_empty());
    assert_eq!(sim.acquires(), 2);
    sim.run(8);
    assert_eq!(brake(&sim), None);
}

/// Asserts that a preemption trips a pool that trips at one failure: the replacement is not
/// asked for, however long the request waits. It catches a preemption that cannot trip.
#[test]
fn a_preemption_can_trip_the_pool() {
    let mut sim = Sim::new(pool(1, minutes(120)), Billing::Lambda);
    sim.waiting = 1;
    sim.run(6);
    let first = sim.fake.running()[0].clone();
    sim.fake.preempt(&first);
    sim.run(2 * 60);
    assert_eq!(sim.acquires(), 1);
    assert_eq!(sim.blocked(), Some(Blocked::Tripped));
}

/// Asserts that the brake only removes launches: a pool tripped while it holds a healthy
/// registered machine and a request that machine serves keeps the machine, and the step asks
/// for no drain, retire or release of it. It catches a trip that tears down what runs.
#[test]
fn a_trip_drains_and_releases_nothing() {
    let mut sim = Sim::new(pool(3, minutes(120)), Billing::Lambda);
    sim.waiting = 1;
    sim.run(6);
    assert_eq!(sim.phases(), [Phase::Registered]);
    let brake = Brake {
        failures: 3,
        tripped_at: Some(sim.now()),
    };
    sim.rig.state.records.brake.insert(POOL.to_owned(), brake);
    sim.waiting = 2;
    sim.run(120);
    assert_eq!(sim.phases(), [Phase::Registered]);
    assert_eq!(sim.acquires(), 1);
    assert_eq!(sim.fake.running().len(), 1);
    assert!(sim.released.is_empty());
    assert_eq!(sim.blocked(), Some(Blocked::Tripped));
}

/// Asserts a restart in the middle of the growth: after two refusals the count and the
/// running cooldown are read back, so the third refusal's wait is 40 minutes and not 10; and
/// a long outage counts toward the cooldown (the pool asks again at once) and not toward the
/// count (the fourth wait is 80 minutes). It catches a count that restarts at zero and a
/// cooldown that restarts with the process.
#[test]
fn the_count_and_the_cooldown_survive_a_restart() {
    let mut sim = refused(pool(100, minutes(120)), RefusalCode::Quota);
    learn(&mut sim, 2);
    assert_eq!(brake(&sim).unwrap().failures, 2);
    sim.saved = Records::decode(&sim.saved.encode().unwrap()).unwrap();
    sim.restart();
    assert_eq!(brake(&sim).unwrap().failures, 2);
    learn(&mut sim, 3);
    assert_eq!(brake(&sim).unwrap().failures, 3);
    let wait = remaining(&sim);
    assert!((38 * MINUTE..=40 * MINUTE).contains(&wait), "{wait}");

    sim.clock.advance(minutes(180));
    sim.saved = Records::decode(&sim.saved.encode().unwrap()).unwrap();
    sim.restart();
    learn(&mut sim, 4);
    assert_eq!(brake(&sim).unwrap().failures, 4);
    let wait = remaining(&sim);
    assert!((78 * MINUTE..=80 * MINUTE).contains(&wait), "{wait}");
}

/// Asserts that the trip survives a restart, read back from the file's text: the restarted
/// pool is still tripped and the cloud is not asked for two more simulated days. It catches
/// a trip held in memory only, which a crash would hand back to the loop.
#[test]
fn the_trip_survives_a_restart() {
    let mut sim = refused(pool(3, minutes(120)), RefusalCode::Quota);
    learn(&mut sim, 3);
    assert!(sim.rig.state.records.tripped(POOL));
    sim.saved = Records::decode(&sim.saved.encode().unwrap()).unwrap();
    sim.restart();
    assert!(sim.rig.state.records.tripped(POOL));
    sim.run(2 * 24 * 60);
    assert_eq!(sim.acquires(), 3);
    assert_eq!(sim.blocked(), Some(Blocked::Tripped));
    sim.restart();
    sim.run(60);
    assert_eq!(sim.acquires(), 3);
}

/// Asserts the reset: the operator's request clears the count, the trip and the cooldown in
/// one written step, the pool asks the cloud at once, and a restart afterwards finds it
/// untripped. It catches a reset that leaves the cooldown to run, one that is not written,
/// and one that a restart undoes.
#[test]
fn a_reset_ends_the_trip_and_the_end_survives_a_restart() {
    let mut sim = refused(pool(3, minutes(120)), RefusalCode::Quota);
    learn(&mut sim, 3);
    sim.run(2 * 24 * 60);
    assert!(sim.rig.state.records.tripped(POOL));
    sim.fake.script_forever(Call::Acquire, Step::Pass);

    let saves = sim.saves;
    sim.rig.input.brake_resets = vec![POOL.to_owned()];
    sim.tick(None);
    sim.rig.input.brake_resets.clear();
    assert_eq!(sim.saves, saves + 1);
    assert_eq!(sim.acquires(), 4);
    assert_eq!(brake(&sim), None);
    assert!(sim.rig.state.records.cooldowns.is_empty());

    sim.saved = Records::decode(&sim.saved.encode().unwrap()).unwrap();
    sim.restart();
    sim.run(10);
    assert!(!sim.rig.state.records.tripped(POOL));
    assert_eq!(sim.phases().last(), Some(&Phase::Registered));
}

/// Asserts that a reset of a pool with no brake changes nothing and writes nothing, so a
/// request that arrives twice is one request: the second asks for no write, and the records
/// are the same. It catches a reset that writes on every ask.
#[test]
fn a_reset_with_nothing_to_reset_writes_nothing() {
    let mut rig = common::Rig::new(settings(POOL));
    rig.input.waiting = Vec::new();
    rig.tick(at(0));
    let before = rig.state.records.clone();
    rig.input.brake_resets = vec![POOL.to_owned(), "elsewhere".to_owned()];
    let actions = rig.tick(at(MINUTE));
    assert_eq!(count(&actions, "Save"), 0, "{actions:?}");
    assert_eq!(rig.state.records, before);
}

/// Asserts that the brake keeps a pool that is not live from being switched on: a tripped
/// pool whose `launch` is plan stays plan after a reset, and a reset of a tripped pool in
/// plan only clears the latch. It catches a reset that makes a plan pool launch.
#[test]
fn a_reset_does_not_make_a_plan_pool_launch() {
    let plan = Settings {
        launch: js_scaler::LaunchMode::Plan,
        ..pool(3, minutes(120))
    };
    let mut rig = common::Rig::new(plan);
    rig.input.waiting = vec![request("r1")];
    rig.state.records.brake.insert(
        POOL.to_owned(),
        Brake {
            failures: 3,
            tripped_at: Some(at(0)),
        },
    );
    rig.input.brake_resets = vec![POOL.to_owned()];
    let actions = rig.tick(at(MINUTE));
    assert_eq!(count(&actions, "Acquire"), 0);
    assert!(rig.state.records.brake.is_empty());
    assert_eq!(rig.state.blocked[POOL], Blocked::Plan);
}
