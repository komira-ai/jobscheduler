//! `rollback` over fakes that script what the rig's cannot: a unit manager or a link that fails
//! at the `k`th look, a unit that dies in the serve wait, and a store whose lock follows the
//! unit, for the kill-point test.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use js_lease_server::update_hook::step::{self, Env};
use js_lease_server::update_hook::{
    Link, Outcome, PortError, Ports, Reason, Schema, Step, StoreUnreachable, StoreView, UnitState,
    Units, Writer,
};
use js_update_hook::Class;

use crate::steps_verify_rollback::{after_cutover, detail, fails};
use crate::support::{FakeLink, FakeUnits, Rig, UNIT, as_root, free, held};

/// The real unit fake, except that from the `from`th `show` on it answers `then`.
struct Scripted<'a> {
    inner: &'a FakeUnits,
    shows: Cell<u32>,
    from: u32,
    then: Result<(&'static str, &'static str), ()>,
}

impl Units for Scripted<'_> {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        let real = self.inner.show(unit);
        self.shows.set(self.shows.get() + 1);
        match (self.shows.get() >= self.from, self.then) {
            (true, Ok((active, sub))) => Ok(UnitState {
                active: active.to_owned(),
                sub: sub.to_owned(),
            }),
            (true, Err(())) => Err(PortError::Command("systemctl: gone".to_owned())),
            (false, _) => real,
        }
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        self.inner.stop_no_block(unit)
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        self.inner.start(unit)
    }

    fn is_active(&self, unit: &str) -> Result<bool, PortError> {
        self.inner.is_active(unit)
    }
}

/// The real link fake, except that its reads fail from the `from`th on.
struct ReadsFail<'a> {
    inner: &'a FakeLink,
    reads: Cell<u32>,
    from: u32,
}

impl Link for ReadsFail<'_> {
    fn target(&self, link: &Path) -> Result<Option<PathBuf>, PortError> {
        self.reads.set(self.reads.get() + 1);
        match self.reads.get() >= self.from {
            true => Err(PortError::Io("Invalid argument (os error 22)".to_owned())),
            false => self.inner.target(link),
        }
    }

    fn relink(&self, link: &Path, target: &Path) -> Result<(), PortError> {
        self.inner.relink(link, target)
    }
}

/// A store whose lock follows the unit: nobody holds it while the unit is not active, `new`
/// while it runs with the link on the staged release, `prev` otherwise.
struct Following<'a> {
    units: &'a FakeUnits,
    link: &'a FakeLink,
    staged: PathBuf,
}

impl StoreView for Following<'_> {
    fn writer(&self) -> Result<Writer, StoreUnreachable> {
        if self.units.state.borrow().0 != "active" {
            return Ok(Writer::Free);
        }
        let holder = match self.link.target.borrow().as_ref() == Some(&self.staged) {
            true => "new",
            false => "prev",
        };
        Ok(Writer::Held {
            holder: holder.to_owned(),
        })
    }

    fn schema(&self) -> Result<Schema, StoreUnreachable> {
        unreachable!("rollback reads no schema")
    }
}

fn env_over<'a>(
    rig: &'a Rig,
    units: &'a dyn Units,
    link: &'a dyn Link,
    store: &'a dyn StoreView,
) -> Env<'a> {
    Env {
        ports: Ports {
            units,
            link,
            store,
            health: &rig.health,
            clock: &rig.clock,
            gate: &rig.kill,
        },
        declaration: &rig.declaration,
        stat: &as_root,
    }
}

/// Asserts that a unit manager that stops answering at the `k`th look of a rollback (each look
/// in turn: the first read, the stop's two, the start's two, the serve wait's) ends the step
/// with the reason of the look it was at, class environment, and that the serve wait's look
/// ends `rollback_not_serving`.
#[test]
fn rollback_reports_environment_for_a_unit_that_cannot_be_read_at_any_look() {
    let cases = [
        (1, Reason::StopFailed),
        (2, Reason::StopFailed),
        (3, Reason::StopFailed),
        (4, Reason::StartFailed),
        (5, Reason::StartFailed),
        (6, Reason::RollbackNotServing),
    ];
    for (k, reason) in cases {
        let rig = after_cutover(&format!("uvr-rb-look-{k}"));
        rig.store.answer([held("new"), free(), held("prev")]);
        let units = Scripted {
            inner: &rig.units,
            shows: Cell::new(0),
            from: k,
            then: Err(()),
        };
        let env = env_over(&rig, &units, &rig.link, &rig.store);
        let done = step::run(Step::Rollback, &env, &rig.params);
        fails(&done, Outcome::Failure, reason, Class::Environment);
        assert_eq!(units.shows.get(), k);
    }
}

/// Asserts that a previous release that dies (down, or waiting to restart) during the serve
/// wait fails `rollback_not_serving` at once, with no pause in the wait, and the full message.
#[test]
fn rollback_fails_at_once_when_the_previous_release_dies_during_the_serve_wait() {
    for (i, state) in [("inactive", "dead"), ("activating", "auto-restart")]
        .into_iter()
        .enumerate()
    {
        let rig = after_cutover(&format!("uvr-rb-dies-{i}"));
        rig.store.answer([held("new"), free(), held("prev")]);
        let units = Scripted {
            inner: &rig.units,
            shows: Cell::new(0),
            from: 6,
            then: Ok(state),
        };
        let env = env_over(&rig, &units, &rig.link, &rig.store);
        let done = step::run(Step::Rollback, &env, &rig.params);
        fails(
            &done,
            Outcome::Failure,
            Reason::RollbackNotServing,
            Class::Release,
        );
        assert_eq!(
            detail(&done),
            format!("{UNIT} is not running on the previous release")
        );
        assert_eq!(rig.clock.pauses.get(), 0);
    }
}

/// Asserts that a link that cannot be read at the move (the second read) fails `link_failed`
/// after the stop, with no relink and no start.
#[test]
fn rollback_fails_link_failed_when_the_link_cannot_be_read_at_the_move() {
    let rig = after_cutover("uvr-rb-relink-read");
    rig.store.answer([held("new"), free()]);
    let link = ReadsFail {
        inner: &rig.link,
        reads: Cell::new(0),
        from: 2,
    };
    let env = env_over(&rig, &rig.units, &link, &rig.store);
    let done = step::run(Step::Rollback, &env, &rig.params);
    fails(&done, Outcome::Failure, Reason::LinkFailed, Class::Release);
    assert_eq!(rig.calls(), ["stop".to_owned()]);
}

/// Asserts that a rollback killed at any of its kill points leaves the record, and that a clean
/// rerun ends in success with the link on the previous release, the record gone, and, over the
/// two runs together, exactly one stop, one relink and one start, in that order: no act the
/// first run already showed done is repeated.
///
/// Catches a rerun that stops, relinks or starts a second time, or that cannot finish.
#[test]
fn rollback_killed_at_every_point_finishes_on_rerun_without_repeating_an_act() {
    let run = |rig: &Rig| {
        let store = Following {
            units: &rig.units,
            link: &rig.link,
            staged: rig.staged(),
        };
        let env = env_over(rig, &rig.units, &rig.link, &store);
        step::run(Step::Rollback, &env, &rig.params)
    };
    let clean = after_cutover("uvr-rb-kill-clean");
    assert_eq!(run(&clean).outcome, Outcome::Success);
    let points = clean.kill.points();
    assert!(points >= 8, "{points} kill points");
    for n in 1..=points {
        let rig = after_cutover(&format!("uvr-rb-kill-{n}"));
        rig.kill.arm(n);
        assert!(catch_unwind(AssertUnwindSafe(|| run(&rig))).is_err());
        rig.kill.disarm();
        assert!(rig.record().is_some(), "killed at {n}");
        let done = run(&rig);
        assert_eq!(done.outcome, Outcome::Success, "killed at {n}: {done:?}");
        assert_eq!(rig.record(), None, "killed at {n}");
        assert_eq!(rig.link.target.borrow().clone(), Some(rig.previous()));
        assert_eq!(rig.units.state.borrow().0, "active");
        assert_eq!(
            rig.calls(),
            [
                "stop".to_owned(),
                rig.relink_to(&rig.previous()),
                "start".to_owned()
            ],
            "killed at {n}"
        );
    }
}

/// Asserts that a unit still stopping on the previous release (a cutover whose stop ran out of
/// time) is not taken as the previous release serving: the step stops it, waits for the lock to
/// be free, starts it, and succeeds only once a holder other than the dying process's holds the
/// lock. Catches a rollback that takes the old process's lock, about to be let go, as proof.
#[test]
fn rollback_does_not_take_a_stopping_unit_on_the_previous_release_as_serving() {
    let rig = after_cutover("rbs-stopping-prev");
    rig.units.set("deactivating", "stop-sigterm");
    *rig.link.target.borrow_mut() = Some(rig.previous());
    rig.store.answer([held("dying"), free(), held("restarted")]);
    let done = rig.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.calls(), ["stop".to_owned(), "start".to_owned()]);
    assert_eq!(rig.record(), None);

    // The same, where nothing but the dying holder is ever seen: no proof, and the step fails.
    let rig = after_cutover("rbs-stopping-prev-held");
    rig.units.set("deactivating", "stop-sigterm");
    *rig.link.target.borrow_mut() = Some(rig.previous());
    rig.store
        .answer([held("dying"), free(), free(), held("dying")]);
    let done = rig.run(Step::Rollback);
    fails(
        &done,
        Outcome::Failure,
        Reason::RollbackNotServing,
        Class::Release,
    );
    assert!(rig.record().is_some());
}

/// Asserts that a rollback of a unit that is already down waits for the writer lock to be free
/// before it starts the previous release (the stopped process's session can outlive it), and
/// that a holder left at the bound fails `lock_not_released`, class environment, with nothing
/// started. Catches a start beside a lock the previous release would refuse to start under.
#[test]
fn rollback_waits_for_the_lock_before_starting_a_unit_that_is_down() {
    let rig = after_cutover("rbs-down-lock-wait");
    rig.units.set("inactive", "dead");
    *rig.link.target.borrow_mut() = Some(rig.previous());
    rig.store
        .answer([held("stale"), held("stale"), free(), free(), held("old")]);
    let done = rig.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.calls(), ["start".to_owned()]);
    assert_eq!(rig.clock.pauses.get(), 2);

    let rig = after_cutover("rbs-down-lock-stuck");
    rig.units.set("inactive", "dead");
    rig.store.answer([held("stale")]);
    let done = rig.run(Step::Rollback);
    fails(
        &done,
        Outcome::Failure,
        Reason::LockNotReleased,
        Class::Environment,
    );
    assert_eq!(rig.calls(), Vec::<String>::new());
    assert_eq!(rig.link.target.borrow().clone(), Some(rig.staged()));
}
