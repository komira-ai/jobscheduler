//! `prepare`, `cleanup` and the state directory every step opens: the lock, the record's unit.

use js_agent::update_hook::config::{Held, Reason, Record, cleanup, prepare};
use js_agent::update_hook::{Class, Outcome};

use super::super::step_rig::as_root;
use super::rig::{Rig, all_steps};

/// Asserts that `prepare` succeeds, writes no record, and makes the state directory.
///
/// Catches a prepare that does anything to the unit, or leaves a record the next step would
/// take for a cutover that ran.
#[test]
fn prepare_changes_nothing_but_makes_the_state_directory() {
    let rig = Rig::new("config-prepare");
    let done = rig.run(prepare);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.report, Default::default());
    assert!(rig.params.state_dir.is_dir());
    assert!(rig.record().is_none());
    assert!(rig.units.log.borrow().is_empty());
    assert_eq!(rig.lease.looks.get(), 0);
}

/// Asserts that a step that finds the state directory locked by another holder is refused with
/// `locked`, whichever step it is, and does nothing.
///
/// Catches two steps of one package changing the unit at once.
#[test]
fn a_step_that_finds_the_state_directory_locked_is_refused_and_does_nothing() {
    let rig = Rig::new("config-locked");
    let held = Held::open_or_make(&rig.params.state_dir, &as_root).unwrap();
    for step in all_steps() {
        let done = rig.run(step);
        assert_eq!(done.outcome, Outcome::Failure);
        assert_eq!(done.report.reason, Some(Reason::Locked));
        assert!(done.report.detail.unwrap().starts_with("cannot lock "),);
    }
    assert!(rig.units.log.borrow().is_empty());
    drop(held);
    assert_eq!(rig.run(prepare).outcome, Outcome::Success);
}

/// Asserts that a record of another unit is `state_invalid` in every step, and nothing is done.
///
/// Catches a record of one package used for another's unit.
#[test]
fn a_record_of_another_unit_is_state_invalid_in_every_step() {
    let rig = Rig::new("config-other-unit");
    rig.put(&Record {
        unit: "other.service".to_owned(),
        restarted: true,
        ..rig.fresh()
    });
    for step in all_steps() {
        let done = rig.run(step);
        assert_eq!(done.report.reason, Some(Reason::StateInvalid));
        assert_eq!(
            done.report.detail.as_deref(),
            Some(
                "the record is not valid: the record's unit \"other.service\" is not the unit the params name"
            )
        );
        assert_eq!(done.report.class, Class::Release);
    }
    assert!(rig.units.log.borrow().is_empty());
}

/// Asserts that a record that is not a record is `state_invalid` and left as it is.
///
/// Catches a hook that overwrites a record it cannot read.
#[test]
fn a_record_that_cannot_be_read_is_state_invalid() {
    let rig = Rig::new("config-corrupt");
    std::fs::create_dir_all(&rig.params.state_dir).unwrap();
    std::fs::write(rig.record_path(), "not json").unwrap();
    let done = rig.run(cleanup);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert!(
        done.report
            .detail
            .unwrap()
            .starts_with("the record is not readable")
    );
}

/// Asserts that `cleanup` refuses, reversible, until the record says verify succeeded; deletes
/// the record after; and that a cleanup with no record has finished before.
///
/// Catches a cleanup that clears the record of a config nobody has proved took.
#[test]
fn cleanup_refuses_before_verify_and_deletes_the_record_after() {
    let rig = Rig::new("config-cleanup");
    let done = rig.run(cleanup);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("there is no record: cleanup has finished")
    );
    rig.put(&Record {
        restarted: true,
        ..rig.fresh()
    });
    let done = rig.run(cleanup);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::NotVerified));
    assert!(done.report.reversible);
    assert!(rig.record().is_some());
    rig.put(&Record {
        restarted: true,
        verified: true,
        ..rig.fresh()
    });
    let done = rig.run(cleanup);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.report, Default::default());
    assert!(rig.record().is_none());
}
