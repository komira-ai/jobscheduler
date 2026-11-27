//! `cutover` on the fakes: it checks the flags file now in place before anything is restarted,
//! notes the session the lease server shows, restarts the unit (stop, wait for it to be down,
//! start, wait for it to be active) and does not restart twice.

use js_agent::update_hook::config::{CUTOVER_RETRY_SECS, Reason, cutover, verify};
use js_agent::update_hook::{Class, Outcome};

use super::rig::{Rig, Say};

/// A rig whose lease server answers that it does not know the host (no session to note), unless
/// the test says otherwise: a cutover that is not about the server restarts the unit.
fn fresh(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.lease.say(&[Say::Unknown]);
    rig
}

/// Asserts the whole of a restart: the session the server shows is recorded before the stop,
/// the unit is stopped and started in that order, and the record says it was restarted.
///
/// Catches a restart that is not recorded (a rerun would restart again), a session read after
/// the restart (verify would take the old agent's session for the new one's) and a start before
/// the stop.
#[test]
fn cutover_notes_the_session_then_stops_and_starts_the_unit() {
    let rig = fresh("config-cutover-restart");
    rig.lease.say(&[Say::Host("live", Some(1000))]);
    let done = rig.run(cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report, Default::default());
    assert_eq!(rig.calls(), ["stop", "start"]);
    let record = rig.record().unwrap();
    assert!(record.session_read && record.restart_begun && record.restarted && !record.verified);
    assert_eq!(record.session_started_ms, Some(1000));
    assert_eq!(record.unit, "js-agent.service");
    assert_eq!(rig.lease.looks.get(), 1);
}

/// Asserts that a flags file the agent could not use ends the step `bad_config`, release class,
/// reversible, with nothing stopped or started, nothing recorded and the server not asked.
///
/// Catches a restart onto a flags file the agent would refuse, which leaves the host without
/// an agent until a person looks.
#[test]
fn a_flags_file_the_agent_cannot_use_ends_cutover_before_anything_is_restarted() {
    let rig = fresh("config-cutover-bad-flags");
    *rig.flags.borrow_mut() = Some("flags_file: --pool is required".to_owned());
    let done = rig.run(cutover);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::BadConfig));
    assert_eq!(done.report.class, Class::Release);
    assert!(done.report.reversible);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("flags_file: --pool is required")
    );
    assert!(rig.units.log.borrow().is_empty());
    assert!(rig.record().is_none());
    assert_eq!(rig.lease.looks.get(), 0);
}

/// Asserts that a host the server does not know and a host it shows in no session each record
/// no session, and the unit is restarted all the same.
///
/// Catches a restart held up by a host that has not said hello yet: the agent's new flags may
/// be what registers it.
#[test]
fn a_host_the_server_shows_in_no_session_does_not_hold_the_restart() {
    for say in [Say::Unknown, Say::Host("awaiting", None)] {
        let rig = fresh("config-cutover-no-session");
        rig.lease.say(&[say]);
        let done = rig.run(cutover);
        assert_eq!(done.outcome, Outcome::Success, "{say:?}: {done:?}");
        let record = rig.record().unwrap();
        assert!(record.session_read && record.restarted, "{say:?}");
        assert_eq!(record.session_started_ms, None, "{say:?}");
        assert_eq!(rig.calls(), ["stop", "start"], "{say:?}");
    }
}

/// Asserts that a server that cannot be reached, or refuses the question, defers cutover
/// (exit 75, class environment, `lease_unreachable`, retried after `CUTOVER_RETRY_SECS`) with
/// nothing restarted and no session noted, and that the cutover tried again once the server
/// answers reads the session then and restarts.
///
/// Catches a restart made without the session the server showed before it: verify would then
/// take the old agent's session, still live on the server, for the new agent's hello.
#[test]
fn a_server_that_cannot_be_asked_defers_the_cutover_before_anything_is_restarted() {
    for say in [
        Say::Down("cannot talk to the server"),
        Say::Refused("host-certificate-required"),
    ] {
        let rig = fresh("config-cutover-lease-down");
        rig.lease.say(&[say]);
        let done = rig.run(cutover);
        assert_eq!(done.outcome, Outcome::Deferred, "{say:?}: {done:?}");
        assert_eq!(done.report.class, Class::Environment, "{say:?}");
        assert_eq!(
            done.report.reason,
            Some(Reason::LeaseUnreachable),
            "{say:?}"
        );
        assert_eq!(
            done.report.retry_after_secs,
            Some(CUTOVER_RETRY_SECS),
            "{say:?}"
        );
        assert!(rig.calls().is_empty(), "{say:?}");
        assert!(rig.record().is_none(), "{say:?}");
        rig.lease.say(&[Say::Host("live", Some(1000))]);
        let again = rig.run(cutover);
        assert_eq!(again.outcome, Outcome::Success, "{say:?}: {again:?}");
        assert_eq!(rig.calls(), ["stop", "start"], "{say:?}");
        assert_eq!(
            rig.record().unwrap().session_started_ms,
            Some(1000),
            "{say:?}"
        );
    }
}

/// Asserts that `prepare` begins an update afresh: after a whole update whose `cleanup` never
/// ran, the next update's `prepare` deletes the record, and its `cutover` restarts the unit
/// again and its `verify` waits for another hello.
///
/// Catches a record left by an earlier update that makes the next `cutover` and `verify`
/// succeed without restarting the agent, so the new flags would never be read.
#[test]
fn prepare_clears_a_record_an_earlier_update_left_and_the_next_cutover_restarts_again() {
    let rig = fresh("config-cutover-next-update");
    rig.lease.say(&[Say::Host("live", Some(1000))]);
    assert_eq!(rig.prepare().outcome, Outcome::Success);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    rig.lease.say(&[Say::Host("live", Some(2000))]);
    assert_eq!(rig.run(verify).outcome, Outcome::Success);
    assert!(rig.record().unwrap().verified);
    rig.units.log.borrow_mut().clear();
    let done = rig.prepare();
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(rig.record().is_none());
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    assert_eq!(rig.calls(), ["stop", "start"]);
    let record = rig.record().unwrap();
    assert_eq!(
        (record.session_started_ms, record.restarted, record.verified),
        (Some(2000), true, false)
    );
}

/// Asserts that a run after the restart was recorded restarts nothing and reads nothing.
///
/// Catches a retried cutover that restarts the agent a second time, or one that re-reads the
/// session of the agent it restarted.
#[test]
fn a_cutover_run_again_after_the_restart_does_not_restart_again() {
    let rig = fresh("config-cutover-rerun");
    rig.lease.say(&[Say::Host("live", Some(1000))]);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    let before = (rig.units.log.borrow().len(), rig.lease.looks.get());
    let again = rig.run(cutover);
    assert_eq!(again.outcome, Outcome::Success);
    assert_eq!(
        again.report.detail.as_deref(),
        Some("the unit was restarted by an earlier run")
    );
    assert_eq!(
        (rig.units.log.borrow().len(), rig.lease.looks.get()),
        before
    );
}

/// Asserts that a cutover killed in the middle of the restart, run again, keeps the session it
/// recorded before the stop, and finishes the restart from where the unit is.
///
/// Catches a rerun that reads the session again after the stop (it may already be the new
/// agent's, and verify would then wait for a third session), and one that stops a unit that is
/// already down.
#[test]
fn a_cutover_that_stopped_halfway_keeps_its_session_and_finishes_the_restart() {
    let rig = fresh("config-cutover-resume");
    rig.lease.say(&[Say::Host("live", Some(1000))]);
    rig.units.fail.borrow_mut().push("start");
    let first = rig.run(cutover);
    assert_eq!(first.report.reason, Some(Reason::StartFailed));
    let record = rig.record().unwrap();
    assert!(record.restart_begun && !record.restarted);
    rig.units.fail.borrow_mut().clear();
    rig.units.log.borrow_mut().clear();
    rig.lease.say(&[Say::Host("live", Some(2000))]);
    let second = rig.run(cutover);
    assert_eq!(second.outcome, Outcome::Success, "{second:?}");
    assert_eq!(rig.calls(), ["start"]);
    let record = rig.record().unwrap();
    assert_eq!(
        (record.session_started_ms, record.restarted),
        (Some(1000), true)
    );
}

/// Asserts how the stop ends: a unit that is slow to stop is waited for, one that does not stop
/// within the bound (the cutover timeout less the start wait and the margin) is `stop_failed`,
/// class environment, after the polls the bound allows, a stop that cannot be asked for and a
/// unit that cannot be read are too, and a unit already down or already stopping is not stopped
/// again.
///
/// Catches a wait that never ends, one that ends early, and a unit stopped twice.
#[test]
fn the_stop_is_waited_for_within_its_bound() {
    let rig = fresh("config-cutover-slow-stop");
    rig.units.on_stop.replace(
        vec![("deactivating", "stop-sigterm"); 3]
            .into_iter()
            .chain([("inactive", "dead")])
            .collect(),
    );
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    assert_eq!(rig.clock.pauses.get(), 3);

    let rig = fresh("config-cutover-stuck-stop");
    rig.units
        .on_stop
        .replace(vec![("deactivating", "stop-sigterm")]);
    let done = rig.run(cutover);
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("js-agent.service is still deactivating after 420 s")
    );
    assert_eq!(rig.clock.pauses.get(), 42);
    assert_eq!(rig.calls(), ["stop"]);

    let rig = fresh("config-cutover-no-stop");
    rig.units.fail.borrow_mut().push("stop");
    let done = rig.run(cutover);
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("cannot stop js-agent.service: stop failed")
    );

    let rig = fresh("config-cutover-no-show");
    rig.units.fail.borrow_mut().push("show");
    let done = rig.run(cutover);
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("cannot read the state of js-agent.service: show failed")
    );

    let rig = fresh("config-cutover-already-down");
    rig.units.is("inactive", "dead");
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    assert_eq!(rig.calls(), ["start"]);

    let rig = fresh("config-cutover-already-stopping");
    rig.units.is("deactivating", "stop-sigterm");
    rig.units
        .follow(&[("deactivating", "stop-sigterm"), ("inactive", "dead")]);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    assert_eq!(rig.calls(), ["start"]);
}

/// Asserts how the start ends: a start that is refused is `start_failed` of the environment; a
/// unit that is not active within the start wait is too; a unit that ran and died (`failed`, or
/// waiting to restart after a crash) is `start_failed` of the config, release class; every one
/// leaves the step reversible, and the record keeps the restart as begun.
///
/// Catches a crash blamed on the host (the fleet would not roll the config back) and a wait
/// that outlasts the updater's bound.
#[test]
fn a_unit_that_does_not_come_up_is_start_failed_and_a_crash_is_the_configs_fault() {
    let cases = [
        (
            "never active",
            vec![("activating", "start")],
            Class::Environment,
            "js-agent.service is still activating after 120 s",
        ),
        (
            "down without running",
            vec![("inactive", "dead")],
            Class::Environment,
            "js-agent.service is inactive after it was started",
        ),
        (
            "ran and died",
            vec![("activating", "start"), ("failed", "failed")],
            Class::Release,
            "js-agent.service is failed after it was started",
        ),
        (
            "restarting after a crash",
            vec![("activating", "auto-restart")],
            Class::Release,
            "js-agent.service is still activating after 120 s",
        ),
    ];
    for (what, script, class, detail) in cases {
        let rig = fresh("config-cutover-start");
        rig.units.on_start.replace(script);
        let done = rig.run(cutover);
        assert_eq!(done.outcome, Outcome::Failure, "{what}");
        assert_eq!(done.report.reason, Some(Reason::StartFailed), "{what}");
        assert_eq!(done.report.class, class, "{what}");
        assert_eq!(done.report.detail.as_deref(), Some(detail), "{what}");
        assert!(done.report.reversible, "{what}");
        assert!(rig.record().unwrap().restart_begun, "{what}");
    }
    let rig = fresh("config-cutover-start-refused");
    rig.units.fail.borrow_mut().push("start");
    let done = rig.run(cutover);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("cannot start js-agent.service: start failed")
    );
}
