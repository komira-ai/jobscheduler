//! The `cutover` step over the rig's fakes: the acts it makes and in which order, how each
//! wait ends, and the reruns after a kill. The helpers are the parent module's.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use js_lease_server::update_hook::step;
use js_lease_server::update_hook::{Outcome, PortError, Reason, Step, StoreUnreachable};
use js_update_hook::Class;

use super::{LimitedUnits, ScriptedLink, assert_ends, cut_rig, env_link, env_units, stopped_calls};
use crate::support::{Rig, START_MS, UNIT, free, held, st};

// ---------------------------------------------------------------------------------- cutover

/// Asserts that `cutover` with no record fails `state_invalid` (exit 1, still reversible) and
/// makes no call.
#[test]
fn cutover_without_a_record_says_prepare_has_not_run() {
    let rig = Rig::new("spc-cutover-no-record");
    let done = rig.run(Step::Cutover);
    assert_ends(
        &done,
        Outcome::Failure,
        Reason::StateInvalid,
        Class::Release,
    );
    assert_eq!(
        done.report.detail.as_deref(),
        Some("there is no record: prepare has not run")
    );
    assert!(done.report.reversible);
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// Asserts that the acts of `cutover` are, in this order, the stop, the move of the link to the
/// staged release and the start; that it exits 0 irreversible; and that the record carries the
/// three marks.
#[test]
fn cutover_stops_before_it_moves_the_link_and_the_link_before_the_start() {
    let rig = cut_rig("spc-cutover-order");
    let done = rig.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!done.report.reversible);
    assert_eq!(rig.calls(), stopped_calls(&rig));
    assert_eq!(*rig.link.target.borrow(), Some(rig.staged()));
    let record = rig.record().expect("a record");
    assert!(record.stop_begun && record.link_moved && record.start_issued);
}

/// Asserts that a first install's `cutover` (unit down, no link) moves the link to the staged
/// release and starts the unit, with no stop call.
#[test]
fn cutover_on_a_first_install_links_and_starts() {
    let rig = Rig::first_install("spc-cutover-first");
    rig.put(&rig.prepared(None));
    let done = rig.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        rig.calls(),
        [rig.relink_to(&rig.staged()), "start".to_owned()]
    );
    assert_eq!(*rig.link.target.borrow(), Some(rig.staged()));
}

/// Asserts that a lock still held after the stop fails `cutover` with `lock_not_released`
/// (exit 1, class environment, irreversible), names the holder and the wait, leaves the link
/// unmoved, never starts the unit, and does not mark the link as moved.
#[test]
fn cutover_fails_with_the_link_unmoved_when_the_lock_is_still_held_after_the_stop() {
    let rig = cut_rig("spc-cutover-lock-held");
    rig.store.answer([held("old")]);
    let done = rig.run(Step::Cutover);
    assert_ends(
        &done,
        Outcome::Failure,
        Reason::LockNotReleased,
        Class::Environment,
    );
    assert!(!done.report.reversible);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the unit is down and old still holds the store's writer lock after 30 s")
    );
    assert_eq!(rig.calls(), ["stop".to_owned()]);
    assert_eq!(*rig.link.target.borrow(), Some(rig.previous()));
    assert_eq!(rig.clock.now.get(), START_MS + 30_000);
    assert!(!rig.record().expect("a record").link_moved);
}

/// Asserts that a store that does not answer at the wait's bound fails `cutover` with
/// `store_unreachable`, class environment, irreversible, with the link unmoved and no start.
#[test]
fn cutover_fails_when_the_store_is_unreachable_at_the_bound_of_the_lock_wait() {
    let rig = cut_rig("spc-cutover-store-down");
    rig.store.answer([Err(StoreUnreachable)]);
    let done = rig.run(Step::Cutover);
    assert_ends(
        &done,
        Outcome::Failure,
        Reason::StoreUnreachable,
        Class::Environment,
    );
    assert!(!done.report.reversible);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the store could not be reached")
    );
    assert_eq!(rig.calls(), ["stop".to_owned()]);
    assert_eq!(*rig.link.target.borrow(), Some(rig.previous()));
}

/// Asserts that a lock let go after some looks (held, unreachable, then free) lets `cutover`
/// go on to move the link and start, having paused once per look that did not succeed.
#[test]
fn cutover_waits_for_the_lock_to_be_let_go() {
    let rig = cut_rig("spc-cutover-lock-waits");
    rig.store
        .answer([held("old"), Err(StoreUnreachable), free()]);
    let done = rig.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.clock.pauses.get(), 2);
    assert_eq!(rig.calls(), stopped_calls(&rig));
}

/// Asserts that a unit that is down already is not stopped: `cutover` moves the link and
/// starts, with no stop call.
#[test]
fn cutover_does_not_stop_a_unit_that_is_down() {
    let rig = cut_rig("spc-cutover-already-down");
    rig.units.set("inactive", "dead");
    let done = rig.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        rig.calls(),
        [rig.relink_to(&rig.staged()), "start".to_owned()]
    );
}

/// Asserts that a unit in the `failed` state counts as stopped: after the stop call `cutover`
/// goes on.
#[test]
fn cutover_counts_a_failed_unit_as_stopped() {
    let rig = cut_rig("spc-cutover-failed-is-down");
    *rig.units.after_stop.borrow_mut() = (st("failed", "failed"), 0);
    let done = rig.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.calls(), stopped_calls(&rig));
}

/// Asserts that a unit that takes some looks to go down is waited for (a pause per look) before
/// the lock is looked at and the link moved.
#[test]
fn cutover_waits_for_a_unit_that_is_slow_to_stop() {
    let rig = cut_rig("spc-cutover-slow-stop");
    rig.store.answer([free()]);
    *rig.units.after_stop.borrow_mut() = (st("inactive", "dead"), 2);
    let done = rig.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.clock.pauses.get(), 2);
    assert_eq!(rig.calls(), stopped_calls(&rig));
}

/// Asserts that a unit that never goes down fails `cutover` with `stop_failed` (class release,
/// irreversible) at the 90 s bound of the fake clock, with the link unmoved and no start.
#[test]
fn cutover_fails_when_the_unit_never_goes_down() {
    let rig = cut_rig("spc-cutover-never-stops");
    *rig.units.after_stop.borrow_mut() = (st("active", "running"), 0);
    let done = rig.run(Step::Cutover);
    assert_ends(&done, Outcome::Failure, Reason::StopFailed, Class::Release);
    assert!(!done.report.reversible);
    assert_eq!(
        done.report.detail,
        Some(format!("{UNIT} did not stop in 90 s"))
    );
    assert_eq!(rig.clock.now.get(), START_MS + 90_000);
    assert_eq!(rig.calls(), ["stop".to_owned()]);
    assert_eq!(*rig.link.target.borrow(), Some(rig.previous()));
}

/// Asserts that a failing stop call fails `cutover` with `stop_failed` and the manager's
/// message, irreversible, with the link unmoved.
#[test]
fn cutover_fails_when_the_stop_call_fails() {
    let rig = cut_rig("spc-cutover-stop-call-fails");
    rig.units.stop_fails.set(true);
    let done = rig.run(Step::Cutover);
    assert_ends(&done, Outcome::Failure, Reason::StopFailed, Class::Release);
    assert!(!done.report.reversible);
    assert_eq!(
        done.report.detail,
        Some(format!("cannot stop {UNIT}: systemctl: exit status 5"))
    );
    assert_eq!(rig.calls(), ["stop".to_owned()]);
    assert_eq!(*rig.link.target.borrow(), Some(rig.previous()));
}

/// Asserts that a unit state that cannot be read before anything is begun fails `cutover` with
/// `stop_failed`, class environment, still reversible, with the stop not marked and no call.
#[test]
fn cutover_fails_before_the_commit_when_the_unit_cannot_be_read() {
    let rig = cut_rig("spc-cutover-show-fails-first");
    rig.units.show_fails.set(true);
    let done = rig.run(Step::Cutover);
    assert_ends(
        &done,
        Outcome::Failure,
        Reason::StopFailed,
        Class::Environment,
    );
    assert!(done.report.reversible);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "cannot read the state of {UNIT}: systemctl: exit status 1"
        ))
    );
    assert!(!rig.record().expect("a record").stop_begun);
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// Asserts that a unit state that cannot be read at the first look of the stop, and one that
/// cannot be read while waiting for it, each end `cutover` `stop_failed` with class
/// environment, irreversible; the second after the stop call.
#[test]
fn cutover_fails_when_the_unit_cannot_be_read_during_the_stop() {
    for (allowed, calls) in [(1, 0), (2, 1)] {
        let rig = cut_rig(&format!("spc-cutover-show-fails-stop-{allowed}"));
        let units = LimitedUnits {
            inner: &rig.units,
            allowed: Cell::new(allowed),
        };
        let done = step::run(Step::Cutover, &env_units(&rig, &units), &rig.params);
        assert_ends(
            &done,
            Outcome::Failure,
            Reason::StopFailed,
            Class::Environment,
        );
        assert!(!done.report.reversible);
        assert_eq!(rig.calls().len(), calls);
        assert_eq!(*rig.link.target.borrow(), Some(rig.previous()));
    }
}

/// Asserts that a start whose unit state cannot be read (before the start call, and while
/// waiting after it) ends `cutover` with `start_failed`, class environment, irreversible.
#[test]
fn cutover_fails_when_the_unit_cannot_be_read_around_the_start() {
    for (allowed, starts) in [(3, 0), (4, 1)] {
        let rig = cut_rig(&format!("spc-cutover-show-fails-start-{allowed}"));
        let units = LimitedUnits {
            inner: &rig.units,
            allowed: Cell::new(allowed),
        };
        let done = step::run(Step::Cutover, &env_units(&rig, &units), &rig.params);
        assert_ends(
            &done,
            Outcome::Failure,
            Reason::StartFailed,
            Class::Environment,
        );
        assert!(!done.report.reversible);
        let calls = rig.calls();
        assert_eq!(calls.iter().filter(|call| *call == "start").count(), starts);
    }
}

/// Asserts that a failing start call fails `cutover` with `start_failed` and the manager's
/// message, irreversible, with the link already on the staged release.
#[test]
fn cutover_fails_when_the_start_call_fails() {
    let rig = cut_rig("spc-cutover-start-call-fails");
    rig.units.start_fails.set(true);
    let done = rig.run(Step::Cutover);
    assert_ends(&done, Outcome::Failure, Reason::StartFailed, Class::Release);
    assert!(!done.report.reversible);
    assert_eq!(
        done.report.detail,
        Some(format!("cannot start {UNIT}: systemctl: exit status 1"))
    );
    assert_eq!(*rig.link.target.borrow(), Some(rig.staged()));
}

/// Asserts that a unit that goes down after its start (`inactive`, or `failed`) and one that
/// waits to be restarted after a crash (`auto-restart`) each end `cutover` `start_failed` with
/// the message that it ran and stopped.
#[test]
fn cutover_fails_when_the_unit_dies_after_its_start() {
    for (n, after) in [
        ("inactive", "dead"),
        ("failed", "failed"),
        ("activating", "auto-restart"),
    ]
    .iter()
    .enumerate()
    {
        let rig = cut_rig(&format!("spc-cutover-dies-{n}"));
        *rig.units.after_start.borrow_mut() = (st(after.0, after.1), 0);
        let done = rig.run(Step::Cutover);
        assert_ends(&done, Outcome::Failure, Reason::StartFailed, Class::Release);
        assert_eq!(
            done.report.detail,
            Some(format!("{UNIT} ran and stopped after its start"))
        );
        assert!(!done.report.reversible);
    }
}

/// Asserts that a unit that stays `activating` fails `cutover` with `start_failed` at the 120 s
/// bound of the fake clock.
#[test]
fn cutover_fails_when_the_unit_stays_activating() {
    let rig = cut_rig("spc-cutover-stuck-activating");
    rig.store.answer([free()]);
    *rig.units.after_start.borrow_mut() = (st("activating", "start"), 0);
    let done = rig.run(Step::Cutover);
    assert_ends(&done, Outcome::Failure, Reason::StartFailed, Class::Release);
    assert_eq!(
        done.report.detail,
        Some(format!("{UNIT} was not active 120 s after its start"))
    );
    assert_eq!(rig.clock.now.get(), START_MS + 120_000);
}

/// Asserts that a unit that is `activating` for some looks and then `active` is waited for, a
/// pause per look, and `cutover` succeeds.
#[test]
fn cutover_waits_for_a_unit_that_is_slow_to_start() {
    let rig = cut_rig("spc-cutover-slow-start");
    // The link is on the staged release, so only the start is made, from `activating`.
    *rig.link.target.borrow_mut() = Some(rig.staged());
    rig.units.set("activating", "start");
    *rig.units.after_start.borrow_mut() = (st("active", "running"), 2);
    let done = rig.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.clock.pauses.get(), 2);
}

/// Asserts that a unit that is up already, with the link on the staged release, is neither
/// stopped nor started: no call, exit 0, the stop and start marked.
#[test]
fn cutover_makes_no_call_when_the_machine_is_already_done() {
    let rig = cut_rig("spc-cutover-already-done");
    *rig.link.target.borrow_mut() = Some(rig.staged());
    let done = rig.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.calls(), Vec::<String>::new());
    let record = rig.record().expect("a record");
    assert!(record.stop_begun && record.start_issued && !record.link_moved);
}

/// Asserts that a link that cannot be read fails `cutover` with `link_failed` (class release,
/// irreversible) naming the link, and nothing is stopped.
#[test]
fn cutover_fails_when_the_link_cannot_be_read() {
    let rig = cut_rig("spc-cutover-link-unreadable");
    rig.link.read_fails.set(true);
    let done = rig.run(Step::Cutover);
    assert_ends(&done, Outcome::Failure, Reason::LinkFailed, Class::Release);
    assert!(!done.report.reversible);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "cannot read {}: Invalid argument (os error 22)",
            rig.path("link/current").display()
        ))
    );
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// Asserts that a link that reads fine at first and fails at the second read (the one before
/// the move) ends `cutover` `link_failed` after the stop, with no relink and no start.
#[test]
fn cutover_fails_when_the_link_cannot_be_read_before_the_move() {
    let rig = cut_rig("spc-cutover-link-second-read");
    let link = ScriptedLink {
        inner: &rig.link,
        script: RefCell::new(VecDeque::from([
            Ok(Some(rig.previous())),
            Err(PortError::Io("Invalid argument (os error 22)".to_owned())),
        ])),
    };
    let done = step::run(Step::Cutover, &env_link(&rig, &link), &rig.params);
    assert_ends(&done, Outcome::Failure, Reason::LinkFailed, Class::Release);
    assert_eq!(rig.calls(), ["stop".to_owned()]);
}

/// Asserts that a link that is found on the staged release by the read before the move is not
/// moved: the calls are the stop and the start.
#[test]
fn cutover_does_not_move_a_link_that_arrived_during_the_stop() {
    let rig = cut_rig("spc-cutover-link-arrived");
    let link = ScriptedLink {
        inner: &rig.link,
        script: RefCell::new(VecDeque::from([
            Ok(Some(rig.previous())),
            Ok(Some(rig.staged())),
        ])),
    };
    let done = step::run(Step::Cutover, &env_link(&rig, &link), &rig.params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.calls(), ["stop".to_owned(), "start".to_owned()]);
}

/// Asserts that a failing move of the link ends `cutover` `link_failed` (class release,
/// irreversible) naming both paths, with the link as it was and no start.
#[test]
fn cutover_fails_when_the_link_cannot_be_moved() {
    let rig = cut_rig("spc-cutover-relink-fails");
    rig.link.relink_fails.set(true);
    let done = rig.run(Step::Cutover);
    assert_ends(&done, Outcome::Failure, Reason::LinkFailed, Class::Release);
    assert!(!done.report.reversible);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "cannot point {} at {}: Permission denied (os error 13)",
            rig.path("link/current").display(),
            rig.staged().display()
        ))
    );
    assert_eq!(
        rig.calls(),
        ["stop".to_owned(), rig.relink_to(&rig.staged())]
    );
    assert_eq!(*rig.link.target.borrow(), Some(rig.previous()));
    assert!(rig.record().expect("a record").link_moved);
}

/// Asserts that a record with the stop begun and the link marked moved (unit down, link on the
/// staged release) makes a rerun call `start` and nothing else.
#[test]
fn cutover_rerun_after_a_kill_after_the_link_move_makes_no_second_stop() {
    let rig = cut_rig("spc-cutover-rerun-after-link");
    let mut record = rig.prepared(Some("old"));
    record.stop_begun = true;
    record.link_moved = true;
    rig.put(&record);
    rig.units.set("inactive", "dead");
    *rig.link.target.borrow_mut() = Some(rig.staged());
    let done = rig.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!done.report.reversible);
    assert_eq!(rig.calls(), ["start".to_owned()]);
    assert!(rig.record().expect("a record").start_issued);
}

/// Asserts that a record with the link marked moved but the link still on the previous release
/// (killed between the mark and the move) makes a rerun move the link and start, with the unit
/// down not stopped again.
#[test]
fn cutover_rerun_after_the_link_mark_but_before_the_move_moves_the_link() {
    let rig = cut_rig("spc-cutover-rerun-before-move");
    let mut record = rig.prepared(Some("old"));
    record.stop_begun = true;
    record.link_moved = true;
    rig.put(&record);
    rig.units.set("inactive", "dead");
    let done = rig.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        rig.calls(),
        [rig.relink_to(&rig.staged()), "start".to_owned()]
    );
}

/// Asserts that a record with every mark of `cutover` set and the unit down makes a rerun only
/// start the unit.
#[test]
fn cutover_rerun_after_the_start_mark_starts_without_marking_again() {
    let rig = cut_rig("spc-cutover-rerun-start-marked");
    let mut record = rig.prepared(Some("old"));
    record.stop_begun = true;
    record.link_moved = true;
    record.start_issued = true;
    rig.put(&record);
    rig.units.set("inactive", "dead");
    *rig.link.target.borrow_mut() = Some(rig.staged());
    let done = rig.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.calls(), ["start".to_owned()]);
}

/// Asserts that, for every kill point of `cutover`, a run ended there and a clean rerun together
/// end in success with the link on the staged release and the unit active, and make no act
/// twice: at most one stop, one move of the link and one start.
#[test]
fn cutover_killed_at_every_kill_point_reruns_to_success_without_repeating_an_act() {
    let twin = cut_rig("spc-cutover-kill-twin");
    assert_eq!(twin.run(Step::Cutover).outcome, Outcome::Success);
    let points = twin.kill.points();
    assert_eq!(points, 6);
    for n in 1..=points {
        let rig = cut_rig(&format!("spc-cutover-kill-{n}"));
        assert!(rig.run_killed_at(Step::Cutover, n).is_none(), "point {n}");
        let mut calls = rig.calls();
        rig.store.answer([free()]);
        let done = rig.run(Step::Cutover);
        assert_eq!(done.outcome, Outcome::Success, "point {n}: {done:?}");
        calls.extend(rig.calls());
        assert_eq!(*rig.link.target.borrow(), Some(rig.staged()), "point {n}");
        assert_eq!(
            *rig.units.state.borrow(),
            st("active", "running"),
            "point {n}"
        );
        let count = |what: &str| calls.iter().filter(|call| *call == what).count();
        assert!(count("stop") <= 1, "point {n}: {calls:?}");
        assert!(
            count(&rig.relink_to(&rig.staged())) <= 1,
            "point {n}: {calls:?}"
        );
        assert!(count("start") <= 1, "point {n}: {calls:?}");
        assert!(!done.report.reversible);
    }
}
