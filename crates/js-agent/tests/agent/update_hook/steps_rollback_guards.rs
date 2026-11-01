//! `rollback` (host-updates design, 4.2): whose cordon it lifts, what it does on a first install
//! (which has no previous release), that a taint or an owed check halts it before any act, and
//! what it does when the lease server does not answer.

use js_agent::update_hook::{
    Class, Clock, Outcome, ROLLBACK_HELLO_SECS, ROLLBACK_MARGIN_SECS, Reason, START_WAIT_SECS,
};
use serde_json::json;

use super::rollback_entries::{
    RELINK, assert_restored, first_prepared, link_moved, link_target, staged, start, start_failed,
    stopped, unit_state, verified, verified_on, verify_failed, verify_failed_on,
};
use super::step_rig::{Rig, Spy, end_state, line, running};

fn uncordons(spy: &Spy<'_>) -> usize {
    spy.log().iter().filter(|call| *call == "uncordon").count()
}

fn cordon_calls(spy: &Spy<'_>) -> usize {
    spy.log()
        .iter()
        .filter(|call| call.starts_with("cordon(") || *call == "uncordon")
        .count()
}

// Whose cordon it is.

#[test]
fn the_cordon_this_update_placed_is_lifted_when_nothing_was_stopped() {
    let rig = running(Rig::new("hook-rollback-prepared"));
    assert!(rig.cordon_expiry().is_some());
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        line(&done)["reversible"],
        json!(true),
        "the agent was never signalled: there is nothing that cannot be undone"
    );
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(uncordons(&spy), 1);
    assert!(rig.cordon_expiry().is_none());
    assert_eq!(rig.record(), None);
    assert_eq!(unit_state(&rig), "active running");
}

#[test]
fn an_operators_cordon_is_never_lifted_renewed_or_placed_again_at_any_entry() {
    for (label, build) in [
        ("prepared", prepared_by_operator as fn(&str) -> (Rig, u64)),
        ("verify-failed", verify_failed_by_operator),
        ("verified", verified_by_operator),
    ] {
        let (rig, expiry) = build(&format!("hook-rollback-operator-{label}"));
        assert!(!rig.record().unwrap().cordon.placed, "{label}: not ours");
        let spy = Spy::new(&rig);
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{label}: {done:?}");
        assert_eq!(cordon_calls(&spy), 0, "{label}: {:?}", spy.log());
        assert_eq!(
            rig.cordon_expiry(),
            Some(expiry),
            "{label}: the operator's is in force"
        );
        assert_eq!(rig.record(), None, "{label}");
    }
}

fn prepared_by_operator(name: &str) -> (Rig, u64) {
    let rig = Rig::new(name);
    let expiry = rig.operator_cordons();
    (running(rig), expiry)
}

fn verify_failed_by_operator(name: &str) -> (Rig, u64) {
    let rig = Rig::new(name);
    let expiry = rig.operator_cordons();
    (verify_failed_on(running(rig)), expiry)
}

fn verified_by_operator(name: &str) -> (Rig, u64) {
    let rig = Rig::new(name);
    let expiry = rig.operator_cordons();
    (verified_on(running(rig)), expiry)
}

#[test]
fn a_cordon_marked_ours_but_never_confirmed_is_never_lifted_at_any_entry() {
    for (label, build) in [
        (
            "prepared",
            (|name: &str| running(Rig::new(name))) as fn(&str) -> Rig,
        ),
        ("link-moved", link_moved),
        ("verified", verified),
    ] {
        let rig = build(&format!("hook-rollback-provisional-{label}"));
        let mut record = rig.record().unwrap();
        assert!(record.cordon.placed);
        record.cordon.provisional = true;
        rig.put_record(&record);
        let expiry = rig.cordon_expiry();
        assert!(expiry.is_some());
        let spy = Spy::new(&rig);
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{label}: {done:?}");
        assert_eq!(uncordons(&spy), 0, "{label}: it may be an operator's");
        assert!(rig.cordon_expiry().is_some(), "{label}: still in force");
        assert_eq!(rig.record(), None, "{label}");
    }
}

#[test]
fn a_cordon_that_lapsed_and_was_placed_again_by_someone_else_is_not_lifted() {
    let rig = link_moved("hook-rollback-lapsed");
    let placed = rig.record().unwrap().cordon;
    assert!(placed.placed && !placed.provisional);
    // Ours lapses; an operator cordons afterwards.
    rig.world.pause(std::time::Duration::from_secs(400));
    assert!(rig.cordon_expiry().is_none());
    let operator = rig.operator_cordons();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(uncordons(&spy), 0);
    assert_eq!(rig.cordon_expiry(), Some(operator));
    assert_eq!(rig.record(), None);
}

// A first install has no previous release.

#[test]
fn a_first_install_whose_link_moved_is_irreversible_and_rollback_touches_nothing() {
    let rig = first_prepared("hook-rollback-first-moved");
    let cutover = Spy::new(&rig);
    cutover.fail("start");
    assert_eq!(cutover.cutover(&rig).outcome, Outcome::Failure);
    rig.kill.disarm();
    assert!(rig.record().unwrap().link_moved);
    let before = (rig.record_bytes(), end_state(&rig));
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::NoPreviousRelease));
    assert!(!done.report.reversible);
    assert_eq!(done.report.class, Class::Release);
    assert_eq!(
        spy.act_names(),
        Vec::<String>::new(),
        "no stop, no link, no start"
    );
    assert_eq!(cordon_calls(&spy), 0);
    assert_eq!((rig.record_bytes(), end_state(&rig)), before);
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    // The same answer every time: the record is what remembers it.
    let again = Spy::new(&rig).rollback(&rig);
    assert_eq!(again, done);
}

#[test]
fn a_first_install_that_has_not_moved_the_link_has_nothing_to_undo_and_is_reversible() {
    let rig = first_prepared("hook-rollback-first-unmoved");
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(line(&done)["reversible"], json!(true));
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(rig.record(), None);
    assert_eq!(link_target(&rig), None);
}

// A taint or an owed check halts before any act.

fn entries() -> Vec<(&'static str, Rig, bool)> {
    vec![
        (
            "prepared",
            running(Rig::new("hook-rollback-halt-prepared")),
            true,
        ),
        ("link-moved", link_moved("hook-rollback-halt-linked"), false),
        (
            "verify-failed",
            verify_failed("hook-rollback-halt-failed"),
            false,
        ),
        ("verified", verified("hook-rollback-halt-verified"), false),
    ]
}

#[test]
fn a_taint_halts_the_rollback_at_every_entry_before_any_act() {
    for (label, rig, reversible) in entries() {
        rig.taint();
        let before = (rig.record_bytes(), end_state(&rig));
        let spy = Spy::new(&rig);
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Halt, "{label}: {done:?}");
        assert_eq!(done.report.reason, Some(Reason::Tainted), "{label}");
        assert_eq!(done.report.reversible, reversible, "{label}");
        assert_eq!(spy.act_names(), Vec::<String>::new(), "{label}");
        assert_eq!(
            cordon_calls(&spy),
            0,
            "{label}: a halt runs nothing further"
        );
        assert_eq!((rig.record_bytes(), end_state(&rig)), before, "{label}");
    }
}

#[test]
fn an_owed_check_halts_the_rollback_at_every_entry_before_any_act() {
    for (label, rig, _) in entries() {
        rig.state_root().owe("run-7").unwrap();
        let spy = Spy::new(&rig);
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Halt, "{label}: {done:?}");
        assert_eq!(done.report.reason, Some(Reason::OwedCheck), "{label}");
        assert_eq!(spy.act_names(), Vec::<String>::new(), "{label}");
        assert_eq!(cordon_calls(&spy), 0, "{label}");
    }
}

#[test]
fn an_owed_list_that_cannot_be_read_stops_the_rollback_before_any_act() {
    let rig = verified("hook-rollback-owed-unreadable");
    let spy = Spy::new(&rig);
    spy.fail("owed");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

#[test]
fn a_taint_halts_a_rollback_that_has_no_record_and_one_of_a_first_install() {
    let rig = Rig::new("hook-rollback-halt-no-record");
    rig.taint();
    let done = Spy::new(&rig).rollback(&rig);
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    let rig = first_prepared("hook-rollback-halt-first");
    rig.taint();
    let done = Spy::new(&rig).rollback(&rig);
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::Tainted));
}

#[test]
fn a_taint_that_appears_during_the_drain_halts_before_the_stop() {
    let rig = verified("hook-rollback-halt-in-drain");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 3 {
            spy.world
                .state_root()
                .write(&js_agent_wire::Taint {
                    reason: "residue".to_owned(),
                    run_id: None,
                    check: None,
                })
                .unwrap();
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    assert_eq!(
        spy.act_names(),
        Vec::<String>::new(),
        "the agent is not signalled"
    );
    assert_eq!(rig.world.ticks() - base, 3);
}

#[test]
fn a_taint_that_appears_while_the_previous_agent_is_awaited_halts_there() {
    let rig = link_moved("hook-rollback-halt-in-hello");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 2 {
            spy.world
                .state_root()
                .write(&js_agent_wire::Taint {
                    reason: "residue".to_owned(),
                    run_id: None,
                    check: None,
                })
                .unwrap();
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::Tainted));
    assert!(!done.report.reversible);
    assert_eq!(spy.act_names(), [RELINK.to_owned(), start()]);
    assert!(!rig.record().unwrap().rb.hello_ok);
    assert_eq!(uncordons(&spy), 0, "a halt runs nothing further");
}

// The lease server does not answer.

/// How long the rollback waits for a lease server that does not answer before its first act: the
/// declared bound less the start, the hello and the margin.
fn outage_bound_secs(rig: &Rig) -> u64 {
    rig.declaration.timeouts.rollback - START_WAIT_SECS - ROLLBACK_HELLO_SECS - ROLLBACK_MARGIN_SECS
}

/// The end state without the cordon, which lapses while the rollback waits.
fn untimed(rig: &Rig) -> serde_json::Value {
    let mut state = end_state(rig);
    state["cordon"] = serde_json::Value::Null;
    state
}

#[test]
fn a_lease_server_that_stays_down_before_the_first_act_is_waited_for_and_then_fails_as_environment()
{
    for (label, rig) in [
        ("link-moved", link_moved("hook-rollback-outage-linked")),
        (
            "verify-failed",
            verify_failed("hook-rollback-outage-failed"),
        ),
        ("verified", verified("hook-rollback-outage-verified")),
    ] {
        let before = (rig.record_bytes(), untimed(&rig));
        let spy = Spy::new(&rig);
        spy.fail("host");
        let ticks = rig.world.ticks();
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Failure, "{label}: {done:?}");
        assert_eq!(
            done.report.reason,
            Some(Reason::LeaseUnreachable),
            "{label}"
        );
        assert_eq!(done.report.class, Class::Environment, "{label}");
        assert_eq!(done.report.retry_after_secs, None, "{label}");
        assert!(
            !done.report.reversible,
            "{label}: the stop was made by the cutover"
        );
        assert_eq!(
            rig.world.ticks() - ticks,
            usize::try_from(outage_bound_secs(&rig) / 10).unwrap(),
            "{label}: retried inside the step until the bound"
        );
        assert_eq!(spy.act_names(), Vec::<String>::new(), "{label}");
        assert_eq!(
            (rig.record_bytes(), untimed(&rig)),
            before,
            "{label}: nothing half-done"
        );
        // The server comes back: the rollback is done as if it had not been tried.
        let rerun = Spy::new(&rig);
        assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success, "{label}");
        assert_restored(&rig);
    }
}

#[test]
fn a_lease_server_that_comes_back_while_the_rollback_waits_for_it_lets_it_go_on() {
    let rig = link_moved("hook-rollback-outage-transient");
    let spy = Spy::new(&rig);
    spy.fail("host");
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 3 {
            spy.heal("host");
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.world.ticks() - base, 3);
    assert_eq!(spy.act_names(), [RELINK.to_owned(), start()]);
    assert_restored(&rig);
}

#[test]
fn a_cordon_that_cannot_be_lifted_is_left_to_lapse_and_the_rollback_ends_in_success() {
    let rig = link_moved("hook-rollback-outage-uncordon");
    let spy = Spy::new(&rig);
    spy.fail("uncordon");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.reason, None);
    assert!(
        done.report
            .detail
            .as_deref()
            .is_some_and(|text| text.contains("cordon left to lapse")),
        "{done:?}"
    );
    assert_eq!(spy.act_names(), [RELINK.to_owned(), start()]);
    // Nothing renews it any more; the record is gone, and a rerun has nothing to do.
    assert_eq!(rig.record(), None);
    assert!(rig.cordon_expiry().is_some(), "it lapses on its own");
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), Vec::<String>::new());
    rig.world.pause(std::time::Duration::from_secs(400));
    assert!(rig.cordon_expiry().is_none());
}

#[test]
fn a_cordon_that_cannot_be_lifted_when_nothing_was_stopped_is_left_to_lapse_and_stays_reversible() {
    let rig = running(Rig::new("hook-rollback-outage-prepared"));
    let spy = Spy::new(&rig);
    spy.fail("uncordon");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(done.report.reversible, "nothing irreversible happened");
    assert!(
        done.report
            .detail
            .as_deref()
            .is_some_and(|text| text.contains("cordon left to lapse")),
        "{done:?}"
    );
    assert_eq!(rig.record(), None);
    assert_eq!(unit_state(&rig), "active running");
}

#[test]
fn rollback_never_exits_75_at_any_entry_with_the_lease_server_down() {
    let entries: Vec<(&str, Rig)> = vec![
        ("prepared", running(Rig::new("hook-rollback-n75-prepared"))),
        ("stopped", stopped("hook-rollback-n75-stopped")),
        ("link-moved", link_moved("hook-rollback-n75-linked")),
        ("start-failed", start_failed("hook-rollback-n75-failed")),
        ("verify-failed", verify_failed("hook-rollback-n75-vf")),
        ("verified", verified("hook-rollback-n75-verified")),
        ("first", first_prepared("hook-rollback-n75-first")),
        ("no-record", Rig::new("hook-rollback-n75-none")),
    ];
    for (label, rig) in entries {
        for down in [
            &["host", "uncordon", "cordon"][..],
            &["host"],
            &["uncordon"],
        ] {
            let spy = Spy::new(&rig);
            for call in down {
                spy.fail(call);
            }
            let done = spy.rollback(&rig);
            assert_ne!(
                done.outcome,
                Outcome::Deferred,
                "{label} {down:?}: {done:?}"
            );
            assert_ne!(done.outcome.code(), 75, "{label} {down:?}");
            assert_eq!(done.report.retry_after_secs, None, "{label} {down:?}");
        }
    }
}
#[test]
fn a_lease_server_that_goes_down_while_the_previous_agent_is_awaited_fails_as_environment() {
    let rig = link_moved("hook-rollback-outage-hello");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 2 {
            spy.fail("host");
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(
        done.report.class,
        Class::Environment,
        "says nothing of the release"
    );
    assert!(!done.report.reversible);
    // Back up, with the hello: the rerun waits and acts no more.
    rig.world.hello("1.3.0").unwrap();
    let rerun = Spy::new(&rig);
    let done = rerun.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rerun.act_names(), Vec::<String>::new());
    assert_restored(&rig);
}
