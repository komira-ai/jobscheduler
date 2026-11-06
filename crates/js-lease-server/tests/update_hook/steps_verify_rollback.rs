//! The steps `verify`, `rollback` and `cleanup` over the rig's fakes: what each one waits for,
//! what it refuses, what it changes and in which order, and how a rerun after a kill goes on.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

use js_lease_server::update_hook::step;
use js_lease_server::update_hook::{
    Done, Held, LOCK_WAIT_SECS, Outcome, Reason, Record, Rollback, SERVE_WAIT_SECS, Step,
    StoreUnreachable, Units, Writer,
};
use js_update_hook::{Class, DirStat, VERIFY_MARGIN_SECS};

use crate::support::{Rig, UNIT, as_root, free, held, st};

fn unreachable() -> Result<Writer, StoreUnreachable> {
    Err(StoreUnreachable)
}

/// Asserts how a step failed: outcome, reason and class.
pub(crate) fn fails(done: &Done, outcome: Outcome, reason: Reason, class: Class) {
    assert_eq!(done.outcome, outcome, "{done:?}");
    assert_eq!(done.report.reason, Some(reason), "{done:?}");
    assert_eq!(done.report.class, class, "{done:?}");
}

pub(crate) fn detail(done: &Done) -> &str {
    done.report.detail.as_deref().expect("a detail")
}

/// A rig as `cutover` leaves it: the unit up on the staged release, the link there, the lock
/// held by `new`, and the record saying that the stop, the move and the start were begun.
pub(crate) fn after_cutover(name: &str) -> Rig {
    let rig = Rig::new(name);
    let mut record = rig.prepared(Some("old"));
    record.stop_begun = true;
    record.link_moved = true;
    record.start_issued = true;
    rig.put(&record);
    *rig.link.target.borrow_mut() = Some(rig.staged());
    rig.store.answer([held("new")]);
    rig
}

/// The record of an update that went wrong after the commit.
fn committed(rig: &Rig) -> Record {
    let mut record = rig.prepared(Some("old"));
    record.stop_begun = true;
    record.link_moved = true;
    record.start_issued = true;
    record
}

fn verify_secs(rig: &Rig) -> u64 {
    rig.declaration.timeouts.verify - VERIFY_MARGIN_SECS
}

fn answers_health(rig: &Rig, answers: impl IntoIterator<Item = Result<u16, String>>) {
    *rig.health.answers.borrow_mut() = answers.into_iter().collect::<VecDeque<_>>();
}

// ---------------------------------------------------------------- verify

/// Asserts that with the unit active and `/healthz` 200, `verify` does not succeed while the
/// lock is free or still held by the holder `prepare` recorded: it looks three times in vain
/// (three pauses), succeeds at the fourth, which shows a new holder, sets `verify_ok`, and
/// makes no change to the unit or the link.
///
/// Catches a verify that takes a free lock or the old holder as proof.
#[test]
fn verify_waits_for_the_lock() {
    let rig = after_cutover("uvr-verify-waits");
    rig.store.answer([free(), held("old"), free(), held("new")]);
    let done = rig.run(Step::Verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.clock.pauses.get(), 3);
    assert_eq!(rig.store.probed.get(), 4);
    assert_eq!(rig.health.looked.get(), 1);
    assert!(rig.record().unwrap().verify_ok);
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// Asserts that a lock that stays with the recorded holder fails `no_lock` (class release) with
/// the full message, only after the clock has moved by the verify bound, and that the record
/// stays unverified and `/healthz` was never asked.
///
/// Catches a verify that accepts the old server, which still holds the lock, as the new one.
#[test]
fn verify_does_not_accept_the_old_holder() {
    let rig = after_cutover("uvr-verify-old-holder");
    rig.store.answer([held("old")]);
    let done = rig.run(Step::Verify);
    fails(&done, Outcome::Failure, Reason::NoLock, Class::Release);
    let secs = verify_secs(&rig);
    assert_eq!(
        detail(&done),
        format!("no new process took the store's writer lock in {secs} s")
    );
    assert!(rig.clock.now.get() - crate::support::START_MS >= secs * 1000);
    assert_eq!(rig.health.looked.get(), 0);
    assert!(!rig.record().unwrap().verify_ok);
}

/// Asserts that when `prepare` recorded no holder (the server was stopped), a lock nobody
/// holds for the whole bound fails `no_lock`, and any holder is another one.
#[test]
fn verify_with_no_recorded_holder_takes_any_holder_and_fails_on_a_free_lock() {
    let rig = Rig::new("uvr-verify-no-holder");
    let mut record = committed(&rig);
    record.holder = None;
    rig.put(&record);
    *rig.link.target.borrow_mut() = Some(rig.staged());
    rig.store.answer([free()]);
    let done = rig.run(Step::Verify);
    fails(&done, Outcome::Failure, Reason::NoLock, Class::Release);
    rig.store.answer([held("old")]);
    assert_eq!(rig.run(Step::Verify).outcome, Outcome::Success);
}

/// Asserts that a unit that is stuck `activating` for the whole bound fails `no_lock` without
/// a single probe of the store.
#[test]
fn verify_of_a_unit_that_stays_activating_fails_no_lock_at_the_bound() {
    let rig = after_cutover("uvr-verify-activating");
    rig.units.set("activating", "start");
    let done = rig.run(Step::Verify);
    fails(&done, Outcome::Failure, Reason::NoLock, Class::Release);
    assert_eq!(rig.store.probed.get(), 0);
}

/// Asserts that a unit that is `activating` for two looks and then `active` is waited for
/// (two pauses, no probe before it is active) and verified.
#[test]
fn verify_waits_for_a_unit_that_is_still_activating() {
    let rig = after_cutover("uvr-verify-moving");
    rig.units.set("activating", "start");
    *rig.units.after_start.borrow_mut() = (st("active", "running"), 2);
    Units::start(&rig.units, UNIT).unwrap();
    rig.calls();
    let done = rig.run(Step::Verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.clock.pauses.get(), 2);
    assert_eq!(rig.store.probed.get(), 1);
}

/// Asserts that a lock held by a new holder while `/healthz` answers 503 fails `not_serving`
/// with the full message, and that a `/healthz` that cannot be reached fails the same way with
/// the reason in the message.
#[test]
fn verify_fails_not_serving_when_the_lock_is_held_but_healthz_is_503() {
    let rig = after_cutover("uvr-verify-503");
    answers_health(&rig, [Ok(503)]);
    let done = rig.run(Step::Verify);
    fails(&done, Outcome::Failure, Reason::NotServing, Class::Release);
    assert_eq!(
        detail(&done),
        "a new process holds the store's writer lock and is not serving: /healthz answered 503"
    );
    assert!(!rig.record().unwrap().verify_ok);
    answers_health(&rig, [Err("connection refused".to_owned())]);
    let done = rig.run(Step::Verify);
    fails(&done, Outcome::Failure, Reason::NotServing, Class::Release);
    assert_eq!(
        detail(&done),
        "a new process holds the store's writer lock and is not serving: connection refused"
    );
}

/// Asserts that a `/healthz` that fails at first and answers 200 later is a success, and that a
/// store that does not answer at first and shows the new holder later is one too: only the last
/// look counts.
#[test]
fn verify_recovers_from_a_failed_look() {
    let rig = after_cutover("uvr-verify-recovers");
    answers_health(&rig, [Ok(503), Err("reset".to_owned()), Ok(200)]);
    assert_eq!(rig.run(Step::Verify).outcome, Outcome::Success);
    assert_eq!(rig.clock.pauses.get(), 2);
    let rig = after_cutover("uvr-verify-recovers-store");
    rig.store.answer([unreachable(), held("new")]);
    assert_eq!(rig.run(Step::Verify).outcome, Outcome::Success);
    assert_eq!(rig.clock.pauses.get(), 1);
}

/// Asserts that a store that does not answer at the last look fails `store_unreachable`, class
/// environment, with a message that says no more than that the store could not be reached.
#[test]
fn verify_reports_environment_when_the_store_cannot_be_probed_at_the_bound() {
    let rig = after_cutover("uvr-verify-store-down");
    rig.store.answer([unreachable()]);
    let done = rig.run(Step::Verify);
    fails(
        &done,
        Outcome::Failure,
        Reason::StoreUnreachable,
        Class::Environment,
    );
    assert_eq!(detail(&done), "the store could not be reached");
}

/// Asserts that a unit that is `failed`, `inactive` or `auto-restart` at the first look ends
/// `verify` at once as `start_failed` (class release): no pause, no probe.
///
/// Catches a verify that waits out its bound for a server that already died.
#[test]
fn verify_ends_at_once_when_the_unit_failed() {
    for (i, (active, sub)) in [
        ("failed", "failed"),
        ("inactive", "dead"),
        ("activating", "auto-restart"),
    ]
    .into_iter()
    .enumerate()
    {
        let rig = after_cutover(&format!("uvr-verify-dead-{i}"));
        rig.units.set(active, sub);
        let done = rig.run(Step::Verify);
        fails(&done, Outcome::Failure, Reason::StartFailed, Class::Release);
        assert_eq!(
            detail(&done),
            format!("{UNIT} is not running during verify")
        );
        assert_eq!(rig.clock.pauses.get(), 0);
        assert_eq!(rig.store.probed.get(), 0);
    }
}

/// Asserts that a unit whose state cannot be read fails `verify` as `start_failed`, class
/// environment.
#[test]
fn verify_reports_environment_when_the_unit_cannot_be_read() {
    let rig = after_cutover("uvr-verify-show");
    rig.units.show_fails.set(true);
    let done = rig.run(Step::Verify);
    fails(
        &done,
        Outcome::Failure,
        Reason::StartFailed,
        Class::Environment,
    );
}

/// Asserts that a rerun of a verified record succeeds with no look at all: no unit read (a
/// unit manager that fails would show), no probe, no `/healthz`, and the record is unchanged.
#[test]
fn verify_rerun_with_verify_ok_succeeds_without_a_look() {
    let rig = after_cutover("uvr-verify-rerun");
    let mut record = committed(&rig);
    record.verify_ok = true;
    rig.put(&record);
    rig.units.show_fails.set(true);
    let done = rig.run(Step::Verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.store.probed.get(), 0);
    assert_eq!(rig.health.looked.get(), 0);
    assert_eq!(rig.record(), Some(record));
}

/// Asserts that `verify` refuses a record that does not say cutover started the unit
/// (`state_invalid`, exit 1) and looks at nothing.
#[test]
fn verify_without_start_issued_is_state_invalid() {
    let rig = after_cutover("uvr-verify-no-start");
    let mut record = committed(&rig);
    record.start_issued = false;
    rig.put(&record);
    let done = rig.run(Step::Verify);
    fails(
        &done,
        Outcome::Failure,
        Reason::StateInvalid,
        Class::Release,
    );
    assert_eq!(
        detail(&done),
        "the record does not say that cutover started the unit"
    );
    assert_eq!(rig.store.probed.get(), 0);
}

/// Asserts that `verify` refuses a release link that is not on the staged release (here the
/// previous one) as `state_invalid`, and a link that cannot be read as `link_failed`.
#[test]
fn verify_with_the_link_off_the_staged_release_is_state_invalid() {
    let rig = after_cutover("uvr-verify-link");
    *rig.link.target.borrow_mut() = Some(rig.previous());
    let done = rig.run(Step::Verify);
    fails(
        &done,
        Outcome::Failure,
        Reason::StateInvalid,
        Class::Release,
    );
    assert_eq!(
        detail(&done),
        "the release link is not on the staged release"
    );
    *rig.link.target.borrow_mut() = None;
    let done = rig.run(Step::Verify);
    fails(
        &done,
        Outcome::Failure,
        Reason::StateInvalid,
        Class::Release,
    );
    rig.link.read_fails.set(true);
    let done = rig.run(Step::Verify);
    fails(&done, Outcome::Failure, Reason::LinkFailed, Class::Release);
    assert_eq!(rig.store.probed.get(), 0);
}

/// Asserts that `verify` without `--release-link` fails `bad_params`, and with no record fails
/// `state_invalid` saying prepare has not run.
#[test]
fn verify_needs_the_link_param_and_a_record() {
    let mut rig = after_cutover("uvr-verify-params");
    rig.params.release_link = None;
    let done = rig.run(Step::Verify);
    fails(&done, Outcome::Failure, Reason::BadParams, Class::Release);
    let rig = Rig::new("uvr-verify-no-record");
    let done = rig.run(Step::Verify);
    fails(
        &done,
        Outcome::Failure,
        Reason::StateInvalid,
        Class::Release,
    );
    assert_eq!(detail(&done), "there is no record: prepare has not run");
}

/// Asserts that a `verify` killed at the save of `verify_ok` leaves the record unverified, and
/// a clean rerun succeeds and sets it.
#[test]
fn verify_killed_before_the_mark_succeeds_on_rerun() {
    let rig = after_cutover("uvr-verify-kill");
    assert!(rig.run_killed_at(Step::Verify, 1).is_none());
    assert!(!rig.record().unwrap().verify_ok);
    assert_eq!(rig.run(Step::Verify).outcome, Outcome::Success);
    assert!(rig.record().unwrap().verify_ok);
}

// ---------------------------------------------------------------- cleanup

/// Asserts that `cleanup` of a verified record exits 0 and deletes it; that a second one exits
/// 0 saying there is no record; and that a record without `verify_ok` is kept with
/// `not_verified`.
#[test]
fn cleanup_deletes_the_record_and_is_idempotent() {
    let rig = after_cutover("uvr-cleanup");
    let mut record = committed(&rig);
    record.verify_ok = true;
    rig.put(&record);
    let done = rig.run(Step::Cleanup);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail, None);
    assert_eq!(rig.record(), None);
    let again = rig.run(Step::Cleanup);
    assert_eq!(again.outcome, Outcome::Success, "{again:?}");
    assert_eq!(detail(&again), "there is no record: cleanup has finished");
    let unverified = committed(&rig);
    rig.put(&unverified);
    let done = rig.run(Step::Cleanup);
    fails(&done, Outcome::Failure, Reason::NotVerified, Class::Release);
    assert_eq!(
        detail(&done),
        "the record does not say that verify succeeded"
    );
    assert_eq!(rig.record(), Some(unverified));
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// Asserts that a `cleanup` killed before the delete keeps the record and a clean rerun
/// deletes it.
#[test]
fn cleanup_killed_before_the_delete_succeeds_on_rerun() {
    let rig = after_cutover("uvr-cleanup-kill");
    let mut record = committed(&rig);
    record.verify_ok = true;
    rig.put(&record);
    assert!(rig.run_killed_at(Step::Cleanup, 1).is_none());
    assert_eq!(rig.record(), Some(record));
    assert_eq!(rig.run(Step::Cleanup).outcome, Outcome::Success);
    assert_eq!(rig.record(), None);
}

/// Asserts that the state rules hold for every step here: a record of another update, a
/// corrupt record, a state directory that is not as required, and a state directory another
/// step holds are all refused (`state_invalid`, `locked`) and nothing is changed.
#[test]
fn the_steps_refuse_a_state_that_is_not_as_required() {
    for (i, step) in [Step::Verify, Step::Rollback, Step::Cleanup]
        .into_iter()
        .enumerate()
    {
        let rig = after_cutover(&format!("uvr-state-{i}"));
        let mut record = committed(&rig);
        record.target = "9.9.9".to_owned();
        rig.put(&record);
        let done = rig.run(step);
        fails(
            &done,
            Outcome::Failure,
            Reason::StateInvalid,
            Class::Release,
        );
        assert_eq!(rig.record(), Some(record));
        let mut record = committed(&rig);
        record.unit = "other.service".to_owned();
        rig.put(&record);
        let done = rig.run(step);
        fails(
            &done,
            Outcome::Failure,
            Reason::StateInvalid,
            Class::Release,
        );

        fs::write(rig.declaration.state_dir.join("record.json"), b"{ not json").unwrap();
        let done = rig.run(step);
        fails(
            &done,
            Outcome::Failure,
            Reason::StateInvalid,
            Class::Release,
        );

        rig.put(&committed(&rig));
        let mut env = rig.env();
        env.stat = &writable_by_all;
        let done = step::run(step, &env, &rig.params);
        fails(
            &done,
            Outcome::Failure,
            Reason::StateInvalid,
            Class::Release,
        );

        let other = Held::open(&rig.declaration.state_dir, &as_root).unwrap();
        let done = rig.run(step);
        fails(&done, Outcome::Failure, Reason::Locked, Class::Release);
        drop(other);
        assert_eq!(rig.calls(), Vec::<String>::new());
    }
}

fn writable_by_all(_: &Path, _: BorrowedFd<'_>) -> io::Result<DirStat> {
    Ok(DirStat {
        is_symlink: false,
        is_dir: true,
        uid: 0,
        gid: 0,
        mode: 0o777,
    })
}

// ---------------------------------------------------------------- rollback

/// Asserts that a rollback of an update past its stop, with the unit up on the staged release
/// and the lock held by `new`: the calls are exactly stop, relink to the previous release,
/// start; the lock is waited for (a probe that still shows `new` after the stop costs one
/// pause) before the relink; the previous release holding the lock as another holder ends it;
/// the link is on the previous release, exit 0 with `reversible: false`, and the record is
/// gone.
///
/// Catches a rollback that moves the link under a server that still holds the lock, or that
/// restarts without moving the link.
#[test]
fn rollback_restores_the_previous_link() {
    let rig = after_cutover("uvr-rb-restore");
    rig.store
        .answer([held("new"), held("new"), free(), held("prev")]);
    let done = rig.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!done.report.reversible);
    assert_eq!(
        rig.calls(),
        [
            "stop".to_owned(),
            rig.relink_to(&rig.previous()),
            "start".to_owned()
        ]
    );
    assert_eq!(rig.store.probed.get(), 4);
    assert_eq!(rig.clock.pauses.get(), 1);
    assert_eq!(rig.link.target.borrow().clone(), Some(rig.previous()));
    assert_eq!(rig.record(), None);
}

/// Asserts that a unit on its way up or crashed on the staged release is stopped, and the
/// previous release is put back, as for an active one.
#[test]
fn rollback_stops_a_unit_that_is_moving_or_crashed_on_the_staged_release() {
    for (i, (active, sub)) in [("activating", "start"), ("activating", "auto-restart")]
        .into_iter()
        .enumerate()
    {
        let rig = after_cutover(&format!("uvr-rb-moving-{i}"));
        rig.units.set(active, sub);
        rig.store.answer([held("new"), free(), held("prev")]);
        let done = rig.run(Step::Rollback);
        assert_eq!(done.outcome, Outcome::Success, "{done:?}");
        assert_eq!(
            rig.calls(),
            [
                "stop".to_owned(),
                rig.relink_to(&rig.previous()),
                "start".to_owned()
            ]
        );
    }
}

/// Asserts that a record without `stop_begun` is deleted with no call at all and no probe:
/// exit 0 and `reversible: true`.
#[test]
fn rollback_before_any_stop_changes_nothing_and_deletes_the_record() {
    let rig = Rig::new("uvr-rb-before-stop");
    rig.put(&rig.prepared(Some("old")));
    let done = rig.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(done.report.reversible);
    assert_eq!(rig.calls(), Vec::<String>::new());
    assert_eq!(rig.store.probed.get(), 0);
    assert_eq!(rig.record(), None);
}

/// Asserts that a rollback with no record exits 0 saying why, and touches nothing.
#[test]
fn rollback_with_no_record_succeeds() {
    let rig = Rig::new("uvr-rb-no-record");
    let done = rig.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(done.report.reversible);
    assert_eq!(
        detail(&done),
        "there is no record: the rollback has finished, or prepare never wrote one"
    );
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// Asserts that a unit that runs while the link is on a third release, or while there is no
/// link, halts the rollback with exit 76 `state_invalid`: no call, and the record kept.
///
/// Catches a rollback that restarts a server it cannot name the release of.
#[test]
fn rollback_with_the_unit_on_an_unknown_release_halts_with_76() {
    for (i, target) in [Some("releases/3.0.0"), None].into_iter().enumerate() {
        let rig = after_cutover(&format!("uvr-rb-unknown-{i}"));
        let record = rig.record().unwrap();
        *rig.link.target.borrow_mut() = target.map(|rel| rig.path(rel));
        let done = rig.run(Step::Rollback);
        fails(&done, Outcome::Halt, Reason::StateInvalid, Class::Release);
        assert_eq!(
            detail(&done),
            format!(
                "{UNIT} is not down, and {} is on neither release of the update",
                rig.path("link/current").display()
            )
        );
        assert_eq!(rig.calls(), Vec::<String>::new());
        assert_eq!(rig.record(), Some(record));
    }
}

/// Asserts that a first install past its link move cannot be rolled back: exit 1
/// `no_previous_release`, no call, no probe, the record kept; and that one that was stopped
/// but whose link never moved just deletes the record (exit 0).
#[test]
fn rollback_without_a_previous_release_keeps_the_record_and_touches_nothing() {
    let rig = Rig::first_install("uvr-rb-first");
    let mut record = Record::new(&rig.params, None);
    record.stop_begun = true;
    record.link_moved = true;
    rig.put(&record);
    let done = rig.run(Step::Rollback);
    fails(
        &done,
        Outcome::Failure,
        Reason::NoPreviousRelease,
        Class::Release,
    );
    assert!(!done.report.reversible);
    assert_eq!(
        detail(&done),
        "a first install has no previous release to restore: nothing was changed"
    );
    assert_eq!(rig.calls(), Vec::<String>::new());
    assert_eq!(rig.store.probed.get(), 0);
    assert_eq!(rig.record(), Some(record.clone()));
    record.link_moved = false;
    rig.put(&record);
    let done = rig.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.record(), None);
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// Asserts that a unit that is down while the link is on the staged release is not stopped: the
/// lock is seen free, the link goes back to the previous release and the unit starts; the lock
/// was free before the first act, so any holder proves the previous release.
#[test]
fn rollback_with_the_unit_down_on_the_staged_link_relinks_and_starts() {
    let rig = after_cutover("uvr-rb-down");
    rig.units.set("inactive", "dead");
    rig.store.answer([free(), free(), held("old")]);
    let done = rig.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        rig.calls(),
        [rig.relink_to(&rig.previous()), "start".to_owned()]
    );
    assert_eq!(rig.record(), None);
}

/// Asserts that a unit that is down while the link is already on the previous release is just
/// started: no relink.
#[test]
fn rollback_with_the_unit_down_on_the_previous_link_only_starts() {
    let rig = after_cutover("uvr-rb-down-prev");
    rig.units.set("failed", "failed");
    *rig.link.target.borrow_mut() = Some(rig.previous());
    rig.store.answer([free(), free(), held("old")]);
    let done = rig.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.calls(), ["start".to_owned()]);
}

/// Asserts that a unit that is up on the previous release is not touched (no call, no probe
/// before the wait) and that the step waits, two pauses, for any holder of the lock.
#[test]
fn rollback_with_the_unit_up_on_the_previous_release_makes_no_act() {
    let rig = after_cutover("uvr-rb-up-prev");
    *rig.link.target.borrow_mut() = Some(rig.previous());
    rig.store.answer([free(), free(), held("whoever")]);
    let done = rig.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.calls(), Vec::<String>::new());
    assert_eq!(rig.clock.pauses.get(), 2);
    assert_eq!(rig.store.probed.get(), 3);
    assert_eq!(rig.record(), None);
}

/// Asserts that the previous release holding the same holder that `rollback` probed before its
/// first act is not accepted: the step pauses past it and succeeds only at a different holder.
///
/// Catches a rollback that takes the stopped server's session for the restored one.
#[test]
fn rollback_does_not_accept_the_holder_it_saw_before_its_first_act() {
    let rig = after_cutover("uvr-rb-holder");
    rig.store
        .answer([held("new"), free(), held("new"), held("new"), held("prev")]);
    let done = rig.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.clock.pauses.get(), 2);
    assert_eq!(rig.store.probed.get(), 5);
}

/// Asserts that a previous release that never takes the lock, or that shows only the holder
/// of the first probe, fails `rollback_not_serving` (class release, `reversible: false`) after
/// the serve bound, with the full message, and the record is kept with the marks of the acts.
#[test]
fn rollback_whose_previous_release_never_takes_the_lock_fails_at_the_serve_bound() {
    for (i, answer) in [free(), held("new")].into_iter().enumerate() {
        let rig = after_cutover(&format!("uvr-rb-never-{i}"));
        rig.store.answer([held("new"), free(), answer]);
        let done = rig.run(Step::Rollback);
        fails(
            &done,
            Outcome::Failure,
            Reason::RollbackNotServing,
            Class::Release,
        );
        assert!(!done.report.reversible);
        assert_eq!(
            detail(&done),
            format!(
                "the previous release did not take the store's writer lock in {SERVE_WAIT_SECS} s"
            )
        );
        assert!(rig.clock.now.get() - crate::support::START_MS >= SERVE_WAIT_SECS * 1000);
        let record = rig.record().unwrap();
        assert_eq!(record.rb_holder.as_deref(), Some("new"));
        assert_eq!(
            record.rb,
            Rollback {
                stop_begun: true,
                link_restored: true,
                start_issued: true,
                serving_ok: false
            }
        );
    }
}

/// Asserts that a store that does not answer at the serve bound fails `store_unreachable`,
/// class environment; and one that answers again before the bound is not held against the step.
#[test]
fn rollback_reports_environment_when_the_store_is_down_at_the_serve_bound() {
    let rig = after_cutover("uvr-rb-serve-store");
    rig.store.answer([held("new"), free(), unreachable()]);
    let done = rig.run(Step::Rollback);
    fails(
        &done,
        Outcome::Failure,
        Reason::StoreUnreachable,
        Class::Environment,
    );
    assert!(!done.report.reversible);
    assert_eq!(detail(&done), "the store could not be reached");
    let rig = after_cutover("uvr-rb-serve-store-ok");
    rig.store
        .answer([held("new"), free(), unreachable(), held("prev")]);
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
}

/// Asserts that a store that cannot be probed before the first act fails `store_unreachable`
/// (environment) with nothing done: no call, the record unchanged.
#[test]
fn rollback_whose_first_probe_fails_does_nothing() {
    let rig = after_cutover("uvr-rb-first-probe");
    let record = rig.record().unwrap();
    rig.store.answer([unreachable()]);
    let done = rig.run(Step::Rollback);
    fails(
        &done,
        Outcome::Failure,
        Reason::StoreUnreachable,
        Class::Environment,
    );
    assert_eq!(rig.calls(), Vec::<String>::new());
    assert_eq!(rig.record(), Some(record));
}

/// Asserts that a rerun after `rb.serving_ok` only deletes the record: no unit read, no probe,
/// no call.
#[test]
fn rollback_rerun_after_serving_ok_only_deletes_the_record() {
    let rig = after_cutover("uvr-rb-serving-ok");
    let mut record = committed(&rig);
    record.rb = Rollback {
        stop_begun: true,
        link_restored: true,
        start_issued: true,
        serving_ok: true,
    };
    rig.put(&record);
    rig.units.show_fails.set(true);
    let done = rig.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.store.probed.get(), 0);
    assert_eq!(rig.calls(), Vec::<String>::new());
    assert_eq!(rig.record(), None);
}

/// Asserts how the acts of a rollback fail: a stop that errors is `stop_failed` with the stop
/// the only call; a unit that never stops is `stop_failed`; a lock that stays held is
/// `lock_not_released` (environment, full message) and one that cannot be probed is
/// `store_unreachable`; a link that cannot be moved is `link_failed` and a start that errors is
/// `start_failed`; a unit that stays `activating` is `start_failed`. Each keeps the record.
#[test]
fn rollback_reports_each_failed_act_and_keeps_the_record() {
    let rig = after_cutover("uvr-rb-stop-fails");
    rig.units.stop_fails.set(true);
    let done = rig.run(Step::Rollback);
    fails(&done, Outcome::Failure, Reason::StopFailed, Class::Release);
    assert_eq!(rig.calls(), ["stop".to_owned()]);

    let rig = after_cutover("uvr-rb-stop-hangs");
    *rig.units.after_stop.borrow_mut() = (st("deactivating", "stop-sigterm"), 0);
    let done = rig.run(Step::Rollback);
    fails(&done, Outcome::Failure, Reason::StopFailed, Class::Release);
    assert!(detail(&done).ends_with("did not stop in 90 s"), "{done:?}");

    let rig = after_cutover("uvr-rb-lock-held");
    let done = rig.run(Step::Rollback);
    fails(
        &done,
        Outcome::Failure,
        Reason::LockNotReleased,
        Class::Environment,
    );
    assert_eq!(
        detail(&done),
        format!(
            "the unit is down and new still holds the store's writer lock after {LOCK_WAIT_SECS} s"
        )
    );
    assert_eq!(rig.calls(), ["stop".to_owned()]);

    let rig = after_cutover("uvr-rb-lock-store");
    rig.store.answer([held("new"), unreachable()]);
    let done = rig.run(Step::Rollback);
    fails(
        &done,
        Outcome::Failure,
        Reason::StoreUnreachable,
        Class::Environment,
    );

    let rig = after_cutover("uvr-rb-relink-fails");
    rig.store.answer([held("new"), free()]);
    rig.link.relink_fails.set(true);
    let done = rig.run(Step::Rollback);
    fails(&done, Outcome::Failure, Reason::LinkFailed, Class::Release);
    assert_eq!(
        rig.calls(),
        ["stop".to_owned(), rig.relink_to(&rig.previous())]
    );

    let rig = after_cutover("uvr-rb-start-fails");
    rig.store.answer([held("new"), free()]);
    rig.units.start_fails.set(true);
    let done = rig.run(Step::Rollback);
    fails(&done, Outcome::Failure, Reason::StartFailed, Class::Release);

    let rig = after_cutover("uvr-rb-start-hangs");
    rig.store.answer([held("new"), free()]);
    *rig.units.after_start.borrow_mut() = (st("activating", "start"), 1_000_000);
    let done = rig.run(Step::Rollback);
    fails(&done, Outcome::Failure, Reason::StartFailed, Class::Release);
    assert!(rig.record().unwrap().rb.start_issued);
}

/// Asserts that a rollback of an update past its stop but without `--release-link` fails
/// `bad_params` and changes nothing, and that a unit whose state cannot be read, or a link that
/// cannot be read, fails before any act (`stop_failed` environment, `link_failed`).
#[test]
fn rollback_refuses_what_it_cannot_read_before_any_act() {
    let mut rig = after_cutover("uvr-rb-no-link-param");
    rig.params.release_link = None;
    let done = rig.run(Step::Rollback);
    fails(&done, Outcome::Failure, Reason::BadParams, Class::Release);
    assert!(!done.report.reversible);

    let rig = after_cutover("uvr-rb-show-fails");
    rig.units.show_fails.set(true);
    let done = rig.run(Step::Rollback);
    fails(
        &done,
        Outcome::Failure,
        Reason::StopFailed,
        Class::Environment,
    );

    let rig = after_cutover("uvr-rb-link-read");
    rig.link.read_fails.set(true);
    let done = rig.run(Step::Rollback);
    fails(&done, Outcome::Failure, Reason::LinkFailed, Class::Release);
    assert_eq!(rig.calls(), Vec::<String>::new());
}
