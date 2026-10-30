//! What the scaler does with a cloud that says no or takes a machine back, against the
//! elastic fake: a refusal for quota, denial or a request that fits nothing starts the pool's
//! cooldown and costs nothing; a refusal for capacity is retried at once; a preempted machine
//! stops billing when it is taken and is replaced.

mod cloud_rig;
mod common;

use cloud_rig::{PRICE, Sim};
use common::{minutes, settings};
use js_elastic::RefusalCode;
use js_elastic_fake::{Bill, Billing, Call, Step};
use js_scaler::{Blocked, Phase, Settings};

fn pool() -> Settings {
    Settings {
        idle: minutes(5),
        ..settings("gpu")
    }
}

/// Asserts the three refusals that start the pool's cooldown (10 minutes by default), one
/// after another: the pool says why on the tick it learns of the refusal (`Quota`,
/// `Denied`, `Shape`), the refused launch leaves no record and no machine and the cloud's bill
/// is nothing, the cloud is not asked again for the whole cooldown, and on its last tick it is
/// asked once more and the machine is launched. It catches a refusal that does not hold the
/// pool back (a loop of launches against a quota that will not move), one that holds it
/// forever, and one that bills.
#[test]
fn a_quota_denial_or_shape_refusal_holds_the_pool_for_the_cooldown_and_bills_nothing() {
    let cases = [
        (RefusalCode::Quota, Blocked::Quota),
        (RefusalCode::Denied, Blocked::Denied),
        (RefusalCode::Shape, Blocked::Shape),
    ];
    for (code, blocked) in cases {
        let mut sim = Sim::new(pool(), Billing::Lambda);
        sim.fake.script(Call::Acquire, &[Step::Refuse(code)]);
        sim.waiting = 1;
        sim.run(2);
        assert_eq!(sim.blocked(), Some(blocked), "{code:?}");
        assert!(sim.records().is_empty(), "{code:?}: {:?}", sim.phases());
        sim.run(8);
        assert_eq!(sim.blocked(), Some(blocked), "{code:?}");
        assert_eq!(
            sim.acquires(),
            1,
            "{code:?}: the cloud was asked during the cooldown"
        );
        assert_eq!(sim.fake.launched(), 0);
        assert_eq!(sim.fake.bill(), Bill::default());
        sim.run(3);
        assert_eq!(sim.acquires(), 2, "{code:?}");
        assert_eq!(sim.fake.launched(), 1, "{code:?}");
    }
}

/// Asserts that a refusal for capacity is not a failure of the pool: no cooldown, the same
/// offer is not tried again in the step that learned of the refusal, and the next tick
/// launches. The cloud sees two acquires and makes one machine. It catches a capacity
/// refusal that is treated like a quota (a ten minute stall for a cloud that will have room
/// a minute later).
#[test]
fn a_capacity_refusal_is_retried_on_the_next_tick() {
    let mut sim = Sim::new(pool(), Billing::Lambda);
    sim.fake
        .script(Call::Acquire, &[Step::Refuse(RefusalCode::Capacity)]);
    sim.waiting = 1;
    sim.run(4);
    assert_eq!(sim.acquires(), 2);
    assert_eq!(sim.fake.launched(), 1);
    assert_eq!(sim.phases(), [Phase::Booting]);
}

/// Asserts preemption end to end: a registered machine is reclaimed by the cloud; the cloud's
/// bill for it stops at that instant and never moves again (six minutes, however long the
/// replacement runs); the scaler reads the listing, releases the dead machine and
/// confirms it gone, launches a replacement for the request that is still waiting without a
/// cooldown, and the cloud holds one running machine. It catches a preempted machine that
/// goes on being billed in the scaler's books as a running one, a preemption treated as a
/// failed boot (a cooldown), and one that is never released.
#[test]
fn a_preempted_machine_stops_billing_at_once_and_is_replaced() {
    let mut sim = Sim::new(pool(), Billing::Lambda);
    sim.waiting = 1;
    sim.run(6);
    assert_eq!(sim.phases(), [Phase::Registered]);
    let first = sim.fake.running()[0].clone();
    sim.fake.preempt(&first);

    sim.run(4);
    assert_eq!(sim.fake.launched(), 2);
    assert_eq!(sim.fake.running().len(), 1);
    assert_eq!(sim.records()[0].phase, Phase::Gone);
    assert!(sim.gone_but_running().is_empty());
    sim.run(120);
    let six_minutes = Bill(u128::from(PRICE) * 360_000);
    assert_eq!(sim.fake.bill_of(&first), six_minutes);
    assert_eq!(sim.phases()[1], Phase::Registered);
    assert!(sim.unrecorded().is_empty());
}
