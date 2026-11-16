//! `verify` on the fakes: the unit active and the lease server showing the host live in a
//! session other than the one recorded before the restart, within the bound.

use js_agent::update_hook::config::{Reason, Record, verify};
use js_agent::update_hook::{Class, Outcome};

use super::rig::{Rig, Say};

/// A rig whose record says `cutover` restarted the unit when the host was in session 1000.
fn restarted(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.put(&Record {
        session_read: true,
        session_started_ms: Some(1000),
        restart_begun: true,
        restarted: true,
        ..rig.fresh()
    });
    rig
}

/// Asserts that a live host in a session other than the recorded one is the proof, and that the
/// record then says so.
///
/// Catches a verify that succeeds on the old agent's session, or that does not record what it
/// saw (cleanup would refuse).
#[test]
fn a_live_host_in_a_new_session_is_the_proof() {
    let rig = restarted("config-verify-new-session");
    rig.lease.say(&[Say::Host("live", Some(2000))]);
    let done = rig.run(verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report, Default::default());
    assert!(rig.record().unwrap().verified);
    assert_eq!(rig.clock.pauses.get(), 0);
}

/// Asserts that, when no session was recorded, any live session is the proof.
///
/// Catches a verify that cannot succeed for a host the server did not show before the restart.
#[test]
fn with_no_recorded_session_any_live_session_is_the_proof() {
    let rig = Rig::new("config-verify-any-session");
    rig.put(&Record {
        session_read: true,
        restart_begun: true,
        restarted: true,
        ..rig.fresh()
    });
    rig.lease.say(&[Say::Host("live", Some(5))]);
    assert_eq!(rig.run(verify).outcome, Outcome::Success);
}

/// Asserts that the old session, a host that is not live, a session-less host and a host the
/// server does not know are no proof: the step looks until the bound (the verify timeout less
/// the margin, in polls of ten seconds) and ends `no_hello` of the config.
///
/// Catches a verify that takes the unit being active for the agent having read the new flags.
#[test]
fn no_new_session_within_the_bound_is_no_hello() {
    for say in [
        Say::Host("live", Some(1000)),
        Say::Host("awaiting", Some(2000)),
        Say::Host("live", None),
        Say::Unknown,
    ] {
        let rig = restarted("config-verify-no-hello");
        rig.lease.say(&[say]);
        let done = rig.run(verify);
        assert_eq!(done.outcome, Outcome::Failure, "{say:?}");
        assert_eq!(done.report.reason, Some(Reason::NoHello), "{say:?}");
        assert_eq!(done.report.class, Class::Release, "{say:?}");
        assert_eq!(
            done.report.detail.as_deref(),
            Some("no new session of the host said hello in 285 s"),
            "{say:?}"
        );
        assert_eq!(rig.clock.pauses.get(), 29, "{say:?}");
        assert!(!rig.record().unwrap().verified, "{say:?}");
    }
}

/// Asserts that a server that could not be asked at the last look ends the step
/// `lease_unreachable`, class environment, with the server's words; that one that answered at
/// the last look does not; and that a server that is back in time is the proof.
///
/// Catches a config blamed for a server that was down, and one cleared by a look long past.
#[test]
fn a_server_that_cannot_be_asked_is_the_environments_failure_only_at_the_last_look() {
    let rig = restarted("config-verify-unreachable");
    rig.lease.say(&[Say::Down("cannot talk to the server")]);
    let done = rig.run(verify);
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("cannot talk to the server")
    );

    let rig = restarted("config-verify-refused");
    rig.lease.say(&[Say::Refused("host-credential-required")]);
    let done = rig.run(verify);
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("refused: host-credential-required")
    );

    let rig = restarted("config-verify-answered-last");
    rig.lease
        .say(&[Say::Down("cannot talk"), Say::Host("live", Some(1000))]);
    assert_eq!(rig.run(verify).report.reason, Some(Reason::NoHello));

    let rig = restarted("config-verify-back-in-time");
    rig.lease
        .say(&[Say::Down("cannot talk"), Say::Host("live", Some(2000))]);
    assert_eq!(rig.run(verify).outcome, Outcome::Success);
    assert_eq!(rig.clock.pauses.get(), 1);
}

/// Asserts that a unit that is down (inactive, failed, stopping) ends the step at once as
/// `start_failed` of the config, that a unit still starting is waited for, and that a unit that
/// cannot be read is `start_failed` of the environment.
///
/// Catches a verify that waits out its bound on an agent that has already died.
#[test]
fn a_unit_that_is_down_ends_verify_at_once_and_one_still_starting_is_waited_for() {
    for (active, sub) in [
        ("inactive", "dead"),
        ("failed", "failed"),
        ("deactivating", "stop-sigterm"),
    ] {
        let rig = restarted("config-verify-unit-down");
        rig.units.is(active, sub);
        let done = rig.run(verify);
        assert_eq!(done.report.reason, Some(Reason::StartFailed), "{active}");
        assert_eq!(done.report.class, Class::Release, "{active}");
        assert_eq!(
            done.report.detail.as_deref(),
            Some(format!("js-agent.service is {active} ({sub}) during verify").as_str())
        );
        assert_eq!(rig.lease.looks.get(), 0, "{active}");
    }
    let rig = restarted("config-verify-unit-starting");
    rig.units.is("activating", "start");
    rig.units
        .follow(&[("activating", "start"), ("active", "running")]);
    rig.lease.say(&[Say::Host("live", Some(2000))]);
    assert_eq!(rig.run(verify).outcome, Outcome::Success);
    assert_eq!((rig.clock.pauses.get(), rig.lease.looks.get()), (1, 1));

    let rig = restarted("config-verify-unit-unreadable");
    rig.units.fail.borrow_mut().push("show");
    let done = rig.run(verify);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
}

/// Asserts that verify needs a record that says `cutover` restarted the unit, and says
/// `state_invalid` without one, and that a flags file that cannot be used ends it `bad_config`.
///
/// Catches a verify that passes on a host where cutover never ran.
#[test]
fn verify_needs_a_cutover_that_restarted_the_unit_and_a_flags_file_the_agent_can_use() {
    for record in [None, Some(false)] {
        let rig = Rig::new("config-verify-no-restart");
        if let Some(restarted) = record {
            rig.put(&Record {
                restart_begun: true,
                restarted,
                ..rig.fresh()
            });
        }
        let done = rig.run(verify);
        assert_eq!(done.report.reason, Some(Reason::StateInvalid), "{record:?}");
        assert_eq!(
            done.report.detail.as_deref(),
            Some("the record does not say that cutover restarted the unit")
        );
        assert!(rig.units.log.borrow().is_empty());
    }
    let rig = restarted("config-verify-bad-flags");
    *rig.flags.borrow_mut() = Some("flags_file: not a flag".to_owned());
    let done = rig.run(verify);
    assert_eq!(done.report.reason, Some(Reason::BadConfig));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("flags_file: not a flag")
    );
    assert!(rig.units.log.borrow().is_empty());
}

/// Asserts that a verify run again after it succeeded succeeds without looking at the unit or
/// the server.
///
/// Catches a retried verify that fails because the new agent has since gone on to a third
/// session.
#[test]
fn a_verify_run_again_after_it_succeeded_looks_at_nothing() {
    let rig = restarted("config-verify-rerun");
    rig.lease.say(&[Say::Host("live", Some(2000))]);
    assert_eq!(rig.run(verify).outcome, Outcome::Success);
    let before = (rig.units.log.borrow().len(), rig.lease.looks.get());
    assert_eq!(rig.run(verify).outcome, Outcome::Success);
    assert_eq!(
        (rig.units.log.borrow().len(), rig.lease.looks.get()),
        before
    );
}
