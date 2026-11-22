//! The post-job checks a running agent owes when an update reaches the host (host-updates
//! design, 4.2). On a busy host some job has almost always just ended and its checks have not
//! run yet: the state root then holds its `owed-<run>` record for a few seconds. Before the
//! agent is stopped such a record is work, waited for as a lease is: it neither fails the
//! update nor halts it. A check that fails writes its taint before its record goes, and that
//! taint halts. Once no running agent can settle a record (a first install, or after the stop)
//! the record halts, as before.

use js_agent::update_hook::{Cordon, Outcome, Reason, Units};
use serde_json::json;

use super::step_rig::{DRAIN_WAIT_SECS, RETRY_AFTER_SECS, Rig, Spy, UNIT, line, running};

/// The run whose checks are owed in these tests.
const RUN: &str = "run-7";

/// The state root holds the record that `RUN`'s checks are owed.
fn owe(rig: &Rig) {
    rig.state_root().owe(RUN).unwrap();
}

/// `RUN`'s checks have run: the record goes.
fn settle(rig: &Rig) {
    rig.state_root().settle(RUN).unwrap();
}

/// The checks of `RUN` could not read the process table, a taint a restart does not re-check:
/// the agent writes the taint, then removes the record, in that order.
fn fail_check(rig: &Rig) {
    rig.taint_with("unchecked", Some("processes-unreadable"));
    settle(rig);
}

fn unit_state(rig: &Rig) -> String {
    let state = rig.world.show(UNIT).unwrap();
    format!("{} {}", state.active, state.sub)
}

/// Catches a `prepare` that halts (`owed_check`, 76) on the record of a check the running agent
/// has yet to run, which stopped the roll of a busy group.
#[test]
fn a_check_owed_when_the_update_begins_is_waited_for_and_the_update_is_applied_with_no_halt() {
    let rig = Rig::new("hook-owed-applied");
    rig.world.seed_unit(&rig.unit, "active", "running");
    owe(&rig);
    let spy = Spy::new(&rig);
    let rig_ref = &rig;
    spy.on_pause(move |_, ticks| {
        if ticks == 2 {
            settle(rig_ref);
        }
    });

    let done = spy.prepare(&rig);

    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.reason, None);
    assert_eq!(
        rig.world.ticks(),
        2,
        "one pause per poll that saw the record"
    );
    assert!(
        rig.record().unwrap().cordon.placed,
        "the cordon stays for cutover"
    );
    assert_eq!(rig.state_root().owed().unwrap(), Vec::<String>::new());

    let cutover = Spy::new(&rig).cutover(&rig);
    assert_eq!(cutover.outcome, Outcome::Success, "{cutover:?}");
    let verify = Spy::new(&rig).verify(&rig);
    assert_eq!(verify.outcome, Outcome::Success, "{verify:?}");
    let cleanup = Spy::new(&rig).cleanup(&rig);
    assert_eq!(cleanup.outcome, Outcome::Success, "{cleanup:?}");
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.4.0"));
    assert_eq!(unit_state(&rig), "active running");
    assert!(rig.state_root().read().is_none(), "no taint");
}

/// Catches a drain that ends as soon as the last lease is gone while a check is still owed:
/// the stop in `cutover` would then land in the job's cleanup window and taint the host.
#[test]
fn prepare_waits_for_a_check_owed_after_the_last_lease_ended() {
    let rig = Rig::new("hook-owed-after-lease");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let rig_ref = &rig;
    spy.on_pause(move |spy, ticks| match ticks {
        // The job ends: its lease goes, and its checks are owed until they run.
        2 => {
            owe(rig_ref);
            spy.world.alter_host(|host| {
                host.leases = 0;
                host.lease = None;
            });
        }
        4 => settle(rig_ref),
        _ => {}
    });
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.world.ticks(), 4);
}

/// Catches a wait for owed checks that stops looking at the taint: a check that fails during
/// the wait still halts, the cordon left in place (a halt runs nothing further).
#[test]
fn a_check_that_fails_while_prepare_waits_for_it_halts() {
    let rig = Rig::new("hook-owed-fails-in-prepare");
    owe(&rig);
    let spy = Spy::new(&rig);
    let rig_ref = &rig;
    spy.on_pause(move |_, ticks| {
        if ticks == 1 {
            fail_check(rig_ref);
        }
    });
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    assert_eq!(done.outcome.code(), 76);
    assert_eq!(done.report.reason, Some(Reason::Tainted));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the agent is tainted: unchecked")
    );
    assert!(rig.record().unwrap().cordon.placed);
    assert!(rig.cordon_expiry().is_some(), "left to lapse");
}

/// Catches a check still owed at the drain bound that fails the update or halts it: it defers
/// (75, class environment, the service's backoff), lifts the cordon this update placed and stops
/// nothing, and the `rollback` the updater runs next does not halt on the record either.
#[test]
fn a_check_still_owed_at_the_drain_bound_defers_and_the_rollback_after_it_succeeds() {
    let rig = Rig::new("hook-owed-bound");
    rig.world.seed_unit(&rig.unit, "active", "running");
    owe(&rig);
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert_eq!(done.outcome.code(), 75);
    assert_eq!(done.report.reason, Some(Reason::DrainBound));
    assert_eq!(line(&done)["class"], "environment");
    assert_eq!(done.report.retry_after_secs, Some(RETRY_AFTER_SECS));
    assert_eq!(
        done.report.detail,
        Some(format!(
            "the host still held 0 lease(s) after {DRAIN_WAIT_SECS} s; checks are owed for {RUN}; \
             draining because the previous release's agent does not hand its jobs over \
             (no `handover` on its `stops:` line, or it could not be run)"
        ))
    );
    assert_eq!(rig.record().unwrap().cordon, Cordon::default());
    assert_eq!(rig.cordon_expiry(), None, "the cordon was lifted");
    assert_eq!(spy.act_names(), Vec::<String>::new(), "nothing stopped");

    let rollback = Spy::new(&rig);
    let done = rollback.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(line(&done)["reversible"], json!(true));
    assert_eq!(rollback.act_names(), Vec::<String>::new());
    assert_eq!(rig.record(), None);
    assert_eq!(
        unit_state(&rig),
        "active running",
        "the agent was never touched"
    );
    assert_eq!(
        rig.state_root().owed().unwrap(),
        [RUN],
        "the agent settles it, not the hook"
    );
}

/// Catches a `cutover` that stops the agent while a check is owed (the record would outlive the
/// stop and taint the host at the next start), and one that halts on it: it defers with nothing
/// stopped, and the rollback after it succeeds.
#[test]
fn a_check_owed_at_cutover_defers_with_nothing_stopped_and_the_rollback_after_it_succeeds() {
    let rig = running(Rig::new("hook-owed-cutover"));
    owe(&rig);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::DrainBound));
    assert_eq!(line(&done)["class"], "environment");
    assert_eq!(line(&done)["reversible"], json!(true));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the host is not idle: 0 lease(s), releasing: false; checks are owed for run-7")
    );
    assert_eq!(spy.act_names(), Vec::<String>::new(), "nothing stopped");
    let record = rig.record().unwrap();
    assert!(!record.stop_begun);
    assert_eq!(record.cordon, Cordon::default());
    assert_eq!(rig.cordon_expiry(), None, "the cordon was lifted");
    assert_eq!(unit_state(&rig), "active running");

    let done = Spy::new(&rig).rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.record(), None);
}

/// Catches a `cutover` that reads the taint before the records and so misses a check that
/// failed between the two reads: the agent writes the taint before the record goes, so the taint
/// read after the records halts the step before the stop.
#[test]
fn a_check_that_fails_as_cutover_lists_the_records_halts_it_before_the_stop() {
    let rig = running(Rig::new("hook-owed-fails-at-cutover"));
    owe(&rig);
    let spy = Spy::new(&rig);
    spy.check_fails_at_list.set(Some(RUN));
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::Tainted));
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert!(!rig.record().unwrap().stop_begun);
    assert_eq!(unit_state(&rig), "active running");
    assert_eq!(rig.state_root().owed().unwrap(), Vec::<String>::new());
}

/// Catches a first install that goes on over an `owed-*` record: no agent runs that could
/// settle it, and the agent the install starts would taint itself for it.
#[test]
fn an_owed_record_still_halts_a_first_install() {
    let rig = Rig::first_install("hook-owed-first");
    owe(&rig);
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::OwedCheck));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("checks are owed for run-7")
    );
    assert_eq!(rig.record_bytes(), None);
}

/// Catches an owed record that stops halting once the update has stopped the agent: the stop
/// left it, and the agent started next taints itself for it.
#[test]
fn an_owed_record_after_the_stop_still_halts_verify_and_rollback() {
    let rig = running(Rig::new("hook-owed-after-stop"));
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    owe(&rig);
    let done = Spy::new(&rig).verify(&rig);
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::OwedCheck));
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::OwedCheck));
    assert_eq!(line(&done)["reversible"], json!(false));
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

/// Catches a wait that reads the records once and then trusts them: a state root that cannot
/// be listed during the wait fails the step (`state_invalid`) instead of reading as "nothing
/// owed".
#[test]
fn a_state_root_that_cannot_be_listed_during_the_wait_fails_prepare() {
    let rig = Rig::new("hook-owed-unlistable-in-wait");
    owe(&rig);
    let spy = Spy::new(&rig);
    spy.on_pause(|spy, ticks| {
        if ticks == 1 {
            spy.fail("owed");
        }
    });
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
}
