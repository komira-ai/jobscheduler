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
    assert_eq!(brake(&sim).unwrap().failures, 1);
    sim.run(10);
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

/// Asserts that a machine that boots ends the count of a tripped pool and leaves the trip:
/// the pool is tripped while a machine is still booting, the machine's host is heard from, the
/// failures are back to zero and the pool is still tripped and still asks for no machine. It
/// catches a success that lifts a trip, which only an operator may.
#[test]
fn a_success_ends_the_count_but_not_the_trip() {
    let mut sim = Sim::new(pool(3, minutes(120)), Billing::Lambda);
    sim.waiting = 1;
    sim.tick(None);
    assert_eq!(sim.acquires(), 1);
    let now = sim.now();
    let brake = Brake {
        failures: 3,
        tripped_at: Some(now),
    };
    sim.rig.state.records.brake.insert(POOL.to_owned(), brake);
    sim.run(8);
    assert_eq!(sim.phases(), [Phase::Registered]);
    assert_eq!(brake_of(&sim).failures, 3);
    sim.run(10);
    let after = brake_of(&sim);
    assert_eq!(after.failures, 0);
    assert_eq!(after.tripped_at, Some(now));
    sim.waiting = 2;
    sim.run(60);
    assert_eq!(sim.acquires(), 1);
}

/// Asserts the names `GET /v1/pools` shows for a reason that holds a pool back: `brake` for a
/// trip, the cap's own name for a cap, nothing for the rest. It catches a trip that is shown
/// as a cap or not shown.
#[test]
fn the_status_names_a_trip_and_a_cap_and_nothing_else() {
    assert_eq!(Blocked::Tripped.shown_name(), Some("brake"));
    assert_eq!(Blocked::MaxSpendDay.shown_name(), Some("max_spend_day"));
    assert_eq!(Blocked::MaxInstances.shown_name(), Some("max_instances"));
    assert_eq!(Blocked::Plan.shown_name(), None);
    assert_eq!(Blocked::Tripped.cap_name(), None);
}

fn brake_of(sim: &Sim) -> Brake {
    brake(sim).unwrap()
}

/// Asserts that a wave of preemptions read in one step is one strike: three registered
/// machines reclaimed together leave a count of one on a pool that trips at three, and the
/// replacement is asked for. It catches a pool tripped by a single spot reclaim wave before any
/// replacement could boot.
#[test]
fn a_wave_of_preemptions_in_one_step_is_one_strike() {
    let mut sim = Sim::new(pool(3, minutes(120)), Billing::Lambda);
    sim.waiting = 3;
    sim.run(30);
    assert_eq!(sim.fake.running().len(), 3);
    for id in sim.fake.running() {
        sim.fake.preempt(&id);
    }
    sim.tick(None);
    assert_eq!(brake(&sim).unwrap().failures, 1);
    assert!(!sim.rig.state.records.tripped(POOL));
    assert_eq!(sim.acquires(), 4);
}

/// Asserts that refusals answered in one step are one strike: three launches of one step all
/// refused count once, so a pool that trips at three is not tripped by one bad step, and the
/// next two steps with a refusal each do trip it. It catches a bad step that trips a pool alone.
#[test]
fn refusals_answered_in_one_step_are_one_strike() {
    let settings = Settings {
        max_step: 3,
        ..pool(3, minutes(120))
    };
    let mut sim = refused(settings, RefusalCode::Quota);
    sim.waiting = 3;
    learn(&mut sim, 3);
    assert_eq!(brake(&sim).unwrap().failures, 1);
    assert!(!sim.rig.state.records.tripped(POOL));
    learn(&mut sim, 6);
    assert_eq!(brake(&sim).unwrap().failures, 2);
    learn(&mut sim, 9);
    assert!(sim.rig.state.records.tripped(POOL));
}

/// Asserts that a host that says hello and then dies does not end the count: each cycle of a
/// machine that registers and is reclaimed strikes once, and the third cycle trips a pool that
/// trips at three. It catches a count that a hello alone resets, which lets a bad image loop for
/// ever.
#[test]
fn hello_then_die_cycles_trip_the_pool() {
    let mut sim = Sim::new(pool(3, minutes(120)), Billing::Lambda);
    sim.waiting = 1;
    for _ in 0..3 {
        for _ in 0..20 {
            sim.tick(None);
            if sim.phases().last() == Some(&Phase::Registered) {
                break;
            }
        }
        assert_eq!(sim.phases().last(), Some(&Phase::Registered));
        let id = sim.fake.running()[0].clone();
        sim.fake.preempt(&id);
        sim.tick(None);
    }
    assert!(sim.rig.state.records.tripped(POOL));
    assert_eq!(sim.acquires(), 3);
}

/// Asserts that a host heard from for ten minutes ends the count and one heard from for less
/// does not: a count of two stands while the machine's host has been heard for a few minutes
/// and is gone after ten. It catches a threshold that is ignored in either direction.
#[test]
fn a_host_heard_for_ten_minutes_ends_the_count() {
    let mut sim = Sim::new(pool(3, minutes(120)), Billing::Lambda);
    sim.waiting = 1;
    sim.tick(None);
    let seed = Brake {
        failures: 2,
        tripped_at: None,
    };
    sim.rig.state.records.brake.insert(POOL.to_owned(), seed);
    sim.run(6);
    assert_eq!(sim.phases(), [Phase::Registered]);
    assert_eq!(brake_of(&sim).failures, 2);
    sim.run(14);
    assert_eq!(brake(&sim), None);
}

/// Asserts the order of a reset and a failure in one step: the reset is applied first, so a
/// refusal answered in that very step counts from one and the cooldown is the first (ten
/// minutes), not what two earlier failures would have made. It catches a reset applied after
/// the step's failures, which would erase the new failure.
#[test]
fn a_reset_in_the_step_of_a_failure_counts_the_failure_from_one() {
    let mut sim = refused(pool(3, minutes(120)), RefusalCode::Quota);
    learn(&mut sim, 2);
    assert_eq!(brake(&sim).unwrap().failures, 2);
    while sim.acquires() < 3 {
        sim.tick(None);
    }
    sim.rig.input.brake_resets = vec![POOL.to_owned()];
    sim.tick(None);
    sim.rig.input.brake_resets.clear();
    assert_eq!(brake(&sim).unwrap().failures, 1);
    assert!(!sim.rig.state.records.tripped(POOL));
    let wait = remaining(&sim);
    assert!((9 * MINUTE..=10 * MINUTE).contains(&wait), "{wait}");
    assert_eq!(sim.acquires(), 3);
}

/// Asserts that a scaler starts from a records file whose brake was written by a newer
/// release (an unknown field in place of `failures`): the trip it holds is kept and the pool
/// stays tripped. It catches a restart that refuses the file.
#[test]
fn a_scaler_starts_from_a_brake_with_an_unknown_field() {
    let mut sim = refused(pool(3, minutes(120)), RefusalCode::Quota);
    learn(&mut sim, 3);
    assert!(sim.rig.state.records.tripped(POOL));
    let text = sim.saved.encode().unwrap();
    assert!(text.contains("\"failures\": 3"), "{text}");
    let newer = text.replacen("\"failures\": 3", "\"future\": 7", 1);
    sim.saved = Records::decode(&newer).unwrap();
    sim.restart();
    sim.run(2 * 60);
    assert!(sim.rig.state.records.tripped(POOL));
    assert_eq!(sim.acquires(), 3);
    assert_eq!(sim.blocked(), Some(Blocked::Tripped));
}

/// Asserts that a trip and a reset leave the pool's go alone: the same go (its first launch
/// and what it had settled) stands before the trip, while tripped and after the reset, so
/// the spend since the go keeps counting and a reset never re-opens a cap on it. It catches a
/// trip or a reset that ends or restarts the go.
#[test]
fn a_trip_and_a_reset_keep_the_go() {
    let mut sim = Sim::new(pool(1, minutes(120)), Billing::Lambda);
    sim.waiting = 1;
    sim.run(6);
    let go = sim.rig.state.records.go[POOL];
    let first = sim.fake.running()[0].clone();
    sim.fake.preempt(&first);
    sim.run(60);
    assert!(sim.rig.state.records.tripped(POOL));
    assert_eq!(sim.rig.state.records.go[POOL], go);
    sim.rig.input.brake_resets = vec![POOL.to_owned()];
    sim.tick(None);
    sim.rig.input.brake_resets.clear();
    sim.run(10);
    assert!(!sim.rig.state.records.tripped(POOL));
    assert_eq!(sim.rig.state.records.go[POOL].at, go.at);
    assert!(sim.rig.state.records.go[POOL].settled >= go.settled);
}
