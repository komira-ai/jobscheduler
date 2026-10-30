//! A crash and a restart at each point where the scaler and the cloud can disagree, against
//! the elastic fake: after the records are written and before the call is sent, after the
//! call is sent and before the answer is read, after the answer is lost, and after the list
//! is read. Each test restarts the scaler from the records on disk and runs three simulated
//! hours with one request waiting, and checks the two things that cost money: no machine is
//! launched twice for one demand, and every machine the cloud runs is a record or an
//! orphan.

mod cloud_rig;
mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use cloud_rig::{Crash, Sim};
use common::{minutes, settings};
use js_elastic_fake::{Billing, Call, Step};
use js_scaler::{Blocked, Phase, Records, Settings};

fn pool() -> Settings {
    Settings {
        idle: minutes(5),
        ..settings("gpu")
    }
}

/// Runs one waiting request until the process dies at `crash` (on the first launch), with
/// the cloud scripted to answer the acquires as `acquire` says and its list lagging by
/// `lag`, then restarts it and runs three hours.
fn crash_and_restart(crash: Crash, acquire: &[Step], lag: Duration) -> Sim {
    let mut sim = Sim::new(pool(), Billing::Lambda);
    let mut knobs = sim.fake.knobs();
    knobs.list_lag = lag;
    sim.fake.set_knobs(knobs);
    sim.fake.script(Call::Acquire, acquire);
    sim.waiting = 1;
    let crashed = (0..20).any(|_| sim.tick(Some(crash)));
    assert!(crashed, "{crash:?} never happened");
    sim.restart();
    sim.run(180);
    sim
}

/// The two invariants, on the state after the restart has settled: every machine the cloud
/// runs is accounted for by a record (an orphan is a record), no record says gone while its
/// machine runs, and no two machines carry one acquire id.
fn assert_accounted(sim: &Sim) {
    let unrecorded = sim.unrecorded();
    assert!(unrecorded.is_empty(), "no record knows {unrecorded:?}");
    let gone = sim.gone_but_running();
    assert!(gone.is_empty(), "read as gone while running: {gone:?}");
    let truth = sim.fake.truth();
    let acquires: BTreeSet<&str> = truth.iter().map(|held| held.acquire.as_str()).collect();
    assert_eq!(
        acquires.len(),
        truth.len(),
        "two machines for one acquire id"
    );
}

/// Asserts a crash before the records are written: nothing of the tick happened, the cloud
/// was never called, and after the restart the demand is launched for once. One machine, one
/// acquire call, one record, registered. It catches a restart that launches twice or never.
#[test]
fn a_crash_before_the_record_write_launches_the_demand_once_after_the_restart() {
    let sim = crash_and_restart(Crash::BeforeSave, &[], Duration::ZERO);
    assert_eq!(sim.fake.launched(), 1);
    assert_eq!(sim.acquires(), 1);
    assert_eq!(sim.phases(), [Phase::Registered]);
    assert_accounted(&sim);
}

/// Asserts a crash after the record is written and before the call is sent: the record says
/// Launching and the cloud has nothing. The record covers the request for `unknown_hold`
/// (30 minutes), is then given up on, and the demand is launched for once; the cloud sees
/// one acquire and holds one machine, which the new record owns. It catches a restart that
/// launches again at once for a record it already wrote (a double launch if the call had
/// gone out), and one that never gives the record up.
#[test]
fn a_crash_between_the_record_write_and_the_call_gives_the_record_up_and_launches_once() {
    let sim = crash_and_restart(Crash::AfterSave, &[], Duration::ZERO);
    assert_eq!(sim.fake.launched(), 1);
    assert_eq!(sim.acquires(), 1);
    assert!(
        sim.phases().contains(&Phase::Registered),
        "{:?}",
        sim.phases()
    );
    assert!(sim.phases().contains(&Phase::Failed), "{:?}", sim.phases());
    assert_accounted(&sim);
}

/// Asserts a crash after the call is sent and before the answer is read, when the cloud made
/// the machine: the answer is lost, the record says Launching, and the first list after the
/// restart shows the machine and the record takes it. One machine, one acquire call, no
/// second launch for a record whose machine exists. It catches the double launch of a
/// scaler that forgets a call it sent.
#[test]
fn a_crash_after_the_call_is_sent_finds_the_machine_in_the_list_and_launches_nothing_more() {
    let sim = crash_and_restart(Crash::AfterCall, &[], Duration::ZERO);
    assert_eq!(sim.fake.launched(), 1);
    assert_eq!(sim.acquires(), 1);
    assert_eq!(sim.phases(), [Phase::Registered]);
    assert_accounted(&sim);
}

/// Asserts a crash after an answer was lost and the machine landed: the scaler would have read
/// `Unknown`, the restart forgets even that, and the list settles it. One machine, never
/// two, however the answer was lost. It catches an Unknown read as nothing launched.
#[test]
fn a_crash_after_a_lost_answer_that_landed_leaves_one_machine_and_one_record() {
    let sim = crash_and_restart(
        Crash::AfterCall,
        &[Step::Lost { landed: true }],
        Duration::ZERO,
    );
    assert_eq!(sim.fake.launched(), 1);
    assert_eq!(sim.acquires(), 1);
    assert_eq!(sim.phases(), [Phase::Registered]);
    assert_accounted(&sim);
}

/// Asserts a crash after an answer was lost and the machine did not land: nothing exists, the
/// record is given up on after the hold, and the demand is launched for once more. Two
/// acquire calls, one machine. It catches a record that waits forever for a machine that
/// was never made, and a second launch that does not wait for the hold.
#[test]
fn a_crash_after_a_lost_answer_that_did_not_land_launches_once_after_the_hold() {
    let sim = crash_and_restart(
        Crash::AfterCall,
        &[Step::Lost { landed: false }],
        Duration::ZERO,
    );
    assert_eq!(sim.fake.launched(), 1);
    assert_eq!(sim.acquires(), 2);
    assert!(
        sim.phases().contains(&Phase::Registered),
        "{:?}",
        sim.phases()
    );
    assert_accounted(&sim);
}

/// Asserts a crash after the list was read, with a list that lags five minutes: the launch
/// went out and its answer was dropped by the crash; the first lists after the restart do
/// not show the machine yet; the record, which was written first, holds the demand and
/// no second machine is launched; the lagging list shows the machine in the end. It
/// catches a scaler that launches again because the list does not show what it launched.
#[test]
fn a_crash_after_the_list_with_a_lagging_list_does_not_launch_again() {
    let sim = crash_and_restart(Crash::AfterList, &[], Duration::from_secs(300));
    assert_eq!(sim.fake.launched(), 1);
    assert_eq!(sim.acquires(), 1);
    assert_eq!(sim.phases(), [Phase::Registered]);
    assert_accounted(&sim);
}

/// Asserts the case the records exist for: the machine is launched and the records that
/// described it are lost (the disk did not keep the write). The first list shows a machine
/// no record knows, which becomes an orphan: counted, reported, and never released. The
/// demand it cannot serve (its agent never says hello) is launched for, so the cloud
/// runs two machines, and each is a record. It catches a machine the scaler loses track of
/// without an orphan record, and an orphan the scaler releases.
#[test]
fn a_machine_whose_records_were_lost_is_an_orphan_and_is_never_released() {
    let mut sim = Sim::new(pool(), Billing::Lambda);
    sim.silent_orphans = true;
    sim.waiting = 1;
    let crashed = (0..20).any(|_| sim.tick(Some(Crash::AfterCall)));
    assert!(crashed);
    sim.saved = Records::default();
    sim.restart();
    sim.run(180);
    assert_eq!(sim.fake.launched(), 2);
    assert_eq!(sim.fake.running().len(), 2);
    assert!(sim.phases().contains(&Phase::Orphan), "{:?}", sim.phases());
    assert!(
        sim.phases().contains(&Phase::Registered),
        "{:?}",
        sim.phases()
    );
    assert_eq!(
        sim.fake.count(Call::Release),
        0,
        "an orphan is never released"
    );
    assert_accounted(&sim);
}

/// A pool with a day cap of 100 cents whose one machine was launched and whose records were
/// lost, run for `minutes` after the restart with nothing waiting: the machine is an orphan
/// from the first list on.
fn orphan_for(minutes: usize) -> Sim {
    let settings = Settings {
        max_spend_per_day: 100,
        ..pool()
    };
    let mut sim = Sim::new(settings, Billing::Lambda);
    sim.silent_orphans = true;
    sim.waiting = 1;
    let crashed = (0..20).any(|_| sim.tick(Some(Crash::AfterCall)));
    assert!(crashed);
    sim.saved = Records::default();
    sim.restart();
    sim.waiting = 0;
    sim.run(minutes);
    sim
}

/// Asserts that an orphan's spend counts toward the day cap, from the moment it is found.
/// The cap here is 100 cents and a launch needs 15 more (its least cost). After the orphan
/// has run an hour (about 60 cents counted) a waiting request is launched for; after it has
/// run 100 minutes (about 100 counted) the same request is refused with `MaxSpendDay` and
/// the cloud is not asked. The time before the orphan was found is not counted, because the
/// scaler cannot know when it was launched: that is how orphans are counted today (`adopt`
/// stamps the finding time), and it is why a bound on the cloud's bill cannot include
/// orphans found late. It catches an orphan left out of the day's sum, which would let a
/// pool with a forgotten machine launch past its cap.
#[test]
fn an_orphans_spend_counts_toward_the_day_cap_from_when_it_is_found() {
    let mut early = orphan_for(60);
    assert!(early.phases().contains(&Phase::Orphan));
    early.waiting = 1;
    early.tick(None);
    assert_eq!(early.fake.launched(), 2, "an hour of orphan leaves room");

    let mut late = orphan_for(100);
    late.waiting = 1;
    late.tick(None);
    assert_eq!(late.blocked(), Some(Blocked::MaxSpendDay));
    assert_eq!(
        late.fake.launched(),
        1,
        "100 minutes of orphan use the cap up"
    );
    assert_eq!(late.acquires(), 1);
}
