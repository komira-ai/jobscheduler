//! A taint the restarted agent re-checks (a residue of a process the pin file does not name)
//! does not halt `prepare`, `cutover`, `verify` or `rollback`: the step goes on as for a clean
//! host and the restarted agent's own re-checks clear the taint. Any other taint, an unchecked
//! one included, a residue the pin file names, and an owed check, still halt (76).

use std::time::Duration;

use js_agent::update_hook::{Clock, Done, Outcome, Reason};

use super::rollback_entries::{assert_restored, link_moved, start};
use super::step_rig::{Rig, Spy, running};

/// Taints the restarted agent re-checks and clears itself: (reason, check).
const CLEARABLE: [(&str, &str); 2] = [
    ("residue", "daemonised-child"),
    ("residue", "process-as-job-user"),
];

/// Taints the restarted agent does not re-check: (reason, check). The unchecked ones are
/// restored pinned, so a halt-free hook would leave the host tainted for good.
const NOT_CLEARABLE: [(&str, Option<&str>); 8] = [
    ("cleanup-failed", None),
    ("residue", Some("new-mount")),
    ("unchecked", Some("host-not-visible")),
    ("unchecked", Some("processes-unreadable")),
    ("unchecked", Some("watch-over-bound")),
    ("unchecked", Some("cgroup-not-found")),
    ("residue", None),
    ("host-job", None),
];

/// A host `cutover` has finished on, with the new agent's hello seen.
fn cut_over(name: &str) -> Rig {
    let rig = running(Rig::new(name));
    rig.world.pause(Duration::from_secs(5));
    let done = Spy::new(&rig).cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    rig.kill.disarm();
    rig
}

fn halted_tainted(done: &Done, what: &str) {
    assert_eq!(done.outcome, Outcome::Halt, "{what}: {done:?}");
    assert_eq!(done.outcome.code(), 76, "{what}");
    assert_eq!(done.report.reason, Some(Reason::Tainted), "{what}");
}

/// Catches a `prepare` that halts a host for a taint its own agent clears, which kept the
/// release that carries the clearing from reaching the host.
#[test]
fn a_clearable_taint_does_not_halt_prepare() {
    for (n, (reason, check)) in CLEARABLE.into_iter().enumerate() {
        let rig = Rig::new(&format!("hook-clearable-prepare-{n}"));
        rig.taint_with(reason, Some(check));
        let done = Spy::new(&rig).prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{reason}/{check}: {done:?}");
        assert_eq!(done.report.reason, None, "{reason}/{check}");
    }
}

/// Catches a `prepare` that lets a taint through that the agent cannot clear.
#[test]
fn a_taint_the_agent_cannot_clear_still_halts_prepare() {
    for (n, (reason, check)) in NOT_CLEARABLE.into_iter().enumerate() {
        let rig = Rig::new(&format!("hook-notclearable-prepare-{n}"));
        rig.taint_with(reason, check);
        let spy = Spy::new(&rig);
        let done = spy.prepare(&rig);
        halted_tainted(&done, &format!("{reason}/{check:?}"));
        assert_eq!(spy.log(), Vec::<String>::new(), "{reason}/{check:?}");
    }
}

/// Catches a `cutover` that halts a host for a taint its own agent clears.
#[test]
fn a_clearable_taint_does_not_halt_cutover() {
    for (n, (reason, check)) in CLEARABLE.into_iter().enumerate() {
        let rig = running(Rig::new(&format!("hook-clearable-cutover-{n}")));
        rig.taint_with(reason, Some(check));
        let done = Spy::new(&rig).cutover(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{reason}/{check}: {done:?}");
    }
}

/// Catches a `cutover` that goes on over a taint the agent cannot clear.
#[test]
fn a_taint_the_agent_cannot_clear_still_halts_cutover() {
    for (n, (reason, check)) in NOT_CLEARABLE.into_iter().enumerate() {
        let rig = running(Rig::new(&format!("hook-notclearable-cutover-{n}")));
        rig.taint_with(reason, check);
        let spy = Spy::new(&rig);
        let done = spy.cutover(&rig);
        halted_tainted(&done, &format!("{reason}/{check:?}"));
        assert_eq!(spy.act_names(), Vec::<String>::new(), "{reason}/{check:?}");
    }
}

/// Catches a `verify` that halts a host for a taint its own agent clears, including a rerun
/// after the mark.
#[test]
fn a_clearable_taint_does_not_halt_verify() {
    for (n, (reason, check)) in CLEARABLE.into_iter().enumerate() {
        let rig = cut_over(&format!("hook-clearable-verify-{n}"));
        rig.taint_with(reason, Some(check));
        let done = Spy::new(&rig).verify(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{reason}/{check}: {done:?}");
        assert!(rig.record().unwrap().verify_ok, "{reason}/{check}");
        let again = Spy::new(&rig).verify(&rig);
        assert_eq!(again.outcome, Outcome::Success, "{reason}/{check}: rerun");
    }
}

/// Catches a `verify` that goes on over a taint the agent cannot clear.
#[test]
fn a_taint_the_agent_cannot_clear_still_halts_verify() {
    for (n, (reason, check)) in NOT_CLEARABLE.into_iter().enumerate() {
        let rig = cut_over(&format!("hook-notclearable-verify-{n}"));
        rig.taint_with(reason, check);
        let done = Spy::new(&rig).verify(&rig);
        halted_tainted(&done, &format!("{reason}/{check:?}"));
        assert!(!rig.record().unwrap().verify_ok, "{reason}/{check:?}");
    }
}

/// Catches a `rollback` that halts a host for a taint its own agent clears: it must go on as
/// for a clean host, restore the previous release and lift the cordon.
#[test]
fn a_clearable_taint_does_not_halt_rollback() {
    for (n, (reason, check)) in CLEARABLE.into_iter().enumerate() {
        let rig = link_moved(&format!("hook-clearable-rollback-{n}"));
        rig.taint_with(reason, Some(check));
        let spy = Spy::new(&rig);
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{reason}/{check}: {done:?}");
        assert_eq!(done.report.reason, None, "{reason}/{check}");
        assert_eq!(spy.act_names(), [start()], "{reason}/{check}");
        assert_restored(&rig);
    }
}

/// Catches a `rollback` that halts at a later guard, while it waits for the previous agent,
/// over a taint that appears there and that the agent clears.
#[test]
fn a_clearable_taint_that_appears_while_the_previous_agent_is_awaited_does_not_halt() {
    for (n, (reason, check)) in CLEARABLE.into_iter().enumerate() {
        let rig = link_moved(&format!("hook-clearable-rollback-mid-{n}"));
        let spy = Spy::new(&rig);
        *spy.start_leaves.borrow_mut() = Some(("active", "running"));
        let base = rig.world.ticks();
        spy.on_pause(move |spy, ticks| {
            if ticks == base + 2 {
                spy.world
                    .state_root()
                    .write(&js_agent_wire::Taint {
                        reason: reason.to_owned(),
                        run_id: None,
                        check: Some(check.to_owned()),
                    })
                    .unwrap();
            }
        });
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{reason}/{check}: {done:?}");
        assert_eq!(done.report.reason, None, "{reason}/{check}");
        assert_restored(&rig);
    }
}

/// Catches a `rollback` that goes on over a taint the agent cannot clear.
#[test]
fn a_taint_the_agent_cannot_clear_still_halts_rollback() {
    for (n, (reason, check)) in NOT_CLEARABLE.into_iter().enumerate() {
        let rig = running(Rig::new(&format!("hook-notclearable-rollback-{n}")));
        rig.taint_with(reason, check);
        rig.updater_links_previous();
        let spy = Spy::new(&rig);
        let done = spy.rollback(&rig);
        halted_tainted(&done, &format!("{reason}/{check:?}"));
        assert_eq!(spy.act_names(), Vec::<String>::new(), "{reason}/{check:?}");
    }
}

/// Catches an owed check that stops halting once a clearable taint is also on record.
#[test]
fn an_owed_check_halts_even_under_a_clearable_taint() {
    let rig = Rig::new("hook-clearable-and-owed");
    rig.taint_with("residue", Some("daemonised-child"));
    rig.state_root().owe("run-7").unwrap();
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::OwedCheck));
}

/// Writes the pin file of `rig`'s state root for the taint of `reason` and `check`.
fn pin(rig: &Rig, reason: &str, check: &str) {
    rig.state_root()
        .write_pin(&js_agent_wire::Taint {
            reason: reason.to_owned(),
            run_id: None,
            check: Some(check.to_owned()),
        })
        .unwrap();
}

/// Catches a `prepare` that goes on over a residue the pin file names, which the restarted
/// agent restores pinned and never clears, and one that lets a pin file of another taint
/// (the stale file of an operator's clear) halt a host whose taint the agent re-checks.
#[test]
fn a_residue_the_pin_file_names_halts_prepare_and_another_does_not() {
    let rig = Rig::new("hook-pinned-prepare");
    rig.taint_with("residue", Some("daemonised-child"));
    pin(&rig, "residue", "daemonised-child");
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    halted_tainted(&done, "pinned residue");
    assert_eq!(spy.log(), Vec::<String>::new());

    let rig = Rig::new("hook-stale-pin-prepare");
    rig.taint_with("residue", Some("daemonised-child"));
    pin(&rig, "residue", "process-as-job-user");
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}
