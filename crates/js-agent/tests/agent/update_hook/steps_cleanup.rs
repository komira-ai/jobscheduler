//! `cleanup` (host-updates design, 4.2): it finishes an update that `verify` passed, lifts the
//! cordon only if this update placed it, and deletes the record. It is idempotent on resume.

use js_agent::update_hook::{Class, Cordon, Done, Held, Lease, Outcome, Reason, Record};
use serde_json::json;

use super::step_rig::{LEASE_SECS, Rig, START, Spy, as_root, killable, line};

/// The record a `verify` that passed leaves, with the cordon `placed` by us or not.
fn verified(rig: &Rig, placed: bool) -> Record {
    let mut record = rig.fresh_record();
    record.stop_begun = true;
    record.link_moved = true;
    record.start_issued = true;
    record.verify_ok = true;
    if placed {
        record.cordon = Cordon {
            placed: true,
            first_ms: START,
            expires_ms: START + LEASE_SECS * 1000,
        };
    }
    record
}

/// The host before `cleanup`: the record put in place, and the cordon on the server.
fn staged(name: &str, placed: bool) -> Rig {
    let rig = Rig::new(name);
    rig.put_record(&verified(&rig, placed));
    rig.world.cordon(LEASE_SECS).unwrap();
    // The cordon above passed a kill point; the count is of the step's.
    rig.kill.disarm();
    rig
}

#[test]
fn cleanup_without_a_passed_verify_refuses_and_changes_nothing() {
    let rig = Rig::new("hook-cleanup-unverified");
    let mut record = verified(&rig, true);
    record.verify_ok = false;
    rig.put_record(&record);
    rig.world.cordon(LEASE_SECS).unwrap();
    let before = rig.record_bytes();
    let expiry = rig.cordon_expiry();
    let spy = Spy::new(&rig);
    let done = spy.cleanup(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::NotVerified));
    assert_eq!(done.report.class, Class::Release);
    assert_eq!(rig.record_bytes(), before);
    assert_eq!(rig.cordon_expiry(), expiry);
    assert_eq!(spy.log(), Vec::<String>::new());
}

#[test]
fn cleanup_lifts_the_cordon_it_placed_and_deletes_the_record() {
    let rig = staged("hook-cleanup-placed", true);
    let spy = Spy::new(&rig);
    let done = spy.cleanup(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.report.reason, None);
    assert_eq!(spy.log(), ["uncordon"]);
    assert_eq!(rig.cordon_expiry(), None);
    assert_eq!(rig.record_bytes(), None);
    // Nothing else of the machine was touched, and the lock is free again.
    assert!(Held::open(&rig.state_dir(), &as_root).is_ok());
}

#[test]
fn cleanup_leaves_a_cordon_the_operator_placed_and_deletes_the_record() {
    let rig = staged("hook-cleanup-operator", false);
    let expiry = rig.cordon_expiry();
    let spy = Spy::new(&rig);
    assert_eq!(spy.cleanup(&rig).outcome, Outcome::Success);
    assert_eq!(spy.log(), Vec::<String>::new());
    assert_eq!(rig.cordon_expiry(), expiry);
    assert_eq!(rig.record_bytes(), None);
}

#[test]
fn cleanup_run_twice_does_the_same_as_run_once() {
    let rig = staged("hook-cleanup-twice", true);
    let first = Spy::new(&rig);
    assert_eq!(first.cleanup(&rig).outcome, Outcome::Success);
    let second = Spy::new(&rig);
    let done = second.cleanup(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(second.log(), Vec::<String>::new(), "nothing left to undo");
    assert_eq!(rig.record_bytes(), None);
    assert_eq!(rig.cordon_expiry(), None);
}

#[test]
fn a_cordon_that_lapsed_on_its_own_is_not_an_error() {
    let rig = Rig::new("hook-cleanup-lapsed");
    rig.put_record(&verified(&rig, true));
    let spy = Spy::new(&rig);
    assert_eq!(spy.cleanup(&rig).outcome, Outcome::Success);
    assert_eq!(rig.record_bytes(), None);
}

#[test]
fn a_cleanup_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    let baseline = staged("hook-cleanup-kill-base", true);
    let spy = Spy::new(&baseline);
    assert_eq!(spy.cleanup(&baseline).outcome, Outcome::Success);
    let points = baseline.kill.points();
    assert!(points >= 2, "the uncordon and the delete pass kill points");
    for k in 1..=points {
        let rig = staged(&format!("hook-cleanup-kill-{k}"), true);
        rig.kill.arm(k);
        assert!(
            killable(|| Spy::new(&rig).cleanup(&rig)).is_none(),
            "kill point {k} of {points} is reached"
        );
        rig.kill.disarm();
        let done = Spy::new(&rig).cleanup(&rig);
        assert_eq!(done.outcome, Outcome::Success, "killed at point {k}");
        let end = json!({"record": rig.record_bytes(), "cordon": rig.cordon_expiry()});
        assert_eq!(end, json!({"record": null, "cordon": null}), "point {k}");
    }
}

#[test]
fn a_lease_server_that_fails_keeps_the_record_so_that_a_rerun_lifts_the_cordon() {
    let rig = staged("hook-cleanup-down", true);
    let spy = Spy::new(&rig);
    spy.fail("uncordon");
    let done = spy.cleanup(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.class, Class::Environment);
    assert!(rig.record().unwrap().verify_ok);
    assert!(rig.cordon_expiry().is_some());
    spy.heal("uncordon");
    assert_eq!(spy.cleanup(&rig).outcome, Outcome::Success);
    assert_eq!(rig.record_bytes(), None);
    assert_eq!(rig.cordon_expiry(), None);
}

#[test]
fn a_record_that_breaks_a_rule_or_a_directory_that_is_wrong_is_refused_as_state_invalid() {
    let rig = staged("hook-cleanup-invalid", true);
    let mut other = verified(&rig, true);
    other.update.target = "1.5.0".to_owned();
    rig.put_record(&other);
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    let done = spy.cleanup(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(rig.record_bytes(), before);
    assert_eq!(spy.log(), Vec::<String>::new());
    assert!(rig.cordon_expiry().is_some());

    let rig = staged("hook-cleanup-newer", true);
    std::fs::write(rig.record_path(), br#"{"schema":2}"#).unwrap();
    let done = Spy::new(&rig).cleanup(&rig);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(rig.record_bytes().unwrap(), br#"{"schema":2}"#);
    assert!(rig.cordon_expiry().is_some());
}

#[test]
fn a_second_holder_of_the_lock_refuses_cleanup_and_nothing_changes() {
    let rig = staged("hook-cleanup-locked", true);
    let before = rig.record_bytes();
    let other = Held::open(&rig.state_dir(), &as_root).unwrap();
    let spy = Spy::new(&rig);
    let done = spy.cleanup(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::Locked));
    assert_eq!(rig.record_bytes(), before);
    assert_eq!(spy.log(), Vec::<String>::new());
    drop(other);
    assert!(Held::open(&rig.state_dir(), &as_root).is_ok());
}

#[test]
fn cleanup_with_no_record_is_a_finished_cleanup() {
    let rig = Rig::new("hook-cleanup-no-record");
    let spy = Spy::new(&rig);
    let done: Done = spy.cleanup(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(line(&done)["reversible"], json!(true));
    assert_eq!(spy.log(), Vec::<String>::new());
}
