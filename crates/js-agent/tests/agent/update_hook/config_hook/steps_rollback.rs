//! `rollback` on the fakes: the unit restarted onto the previous files, the hello waited for
//! when the lease can be made as the host, and never failed for want of it.

use js_agent::update_hook::config::{Reason, Record, rollback};
use js_agent::update_hook::{Class, Outcome};

use super::rig::{NO_CERTIFICATE, Rig, Say};

/// A rig whose record says `cutover` began the restart when the host was in session 1000.
fn begun(name: &str) -> Rig {
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

/// Asserts the whole of a rollback with the host certificate named: the session is noted, the unit is
/// stopped and started, the hello (a session other than the one just before the restart) is
/// waited for, and the record is deleted.
///
/// Catches a rollback that takes the session of the config it undoes for the previous agent's
/// hello, and one that leaves the record (a rerun would restart again).
#[test]
fn rollback_restarts_the_unit_and_waits_for_the_hello_of_the_previous_files() {
    let rig = begun("config-rollback-hello");
    rig.lease.say(&[
        Say::Host("live", Some(2000)),
        Say::Host("live", Some(2000)),
        Say::Host("live", Some(3000)),
    ]);
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the unit was restarted; the agent said hello")
    );
    assert_eq!(rig.calls(), ["stop", "start"]);
    assert_eq!((rig.lease.looks.get(), rig.clock.pauses.get()), (3, 1));
    assert!(rig.record().is_none());
}

/// Asserts that when the flags file names no host certificate and key the unit is restarted and
/// no hello is waited for or asked about.
///
/// Catches a rollback that waits a minute for a host that cannot say hello until it has a
/// certificate (the agent refuses to start without one).
#[test]
fn without_a_host_certificate_the_unit_is_restarted_and_no_hello_is_waited_for() {
    let rig = begun("config-rollback-no-certificate");
    rig.without_a_host_certificate();
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "the unit was restarted; not waiting for a hello: {NO_CERTIFICATE}"
        ))
    );
    assert_eq!(rig.calls(), ["stop", "start"]);
    assert_eq!(rig.lease.looks.get(), 0);
    assert!(rig.record().is_none());
}

/// Asserts that a hello that never comes, a server that cannot be asked and a flags file that
/// cannot be used each end the rollback in success, with the reason in the detail, after at
/// most the hello wait.
///
/// Catches a rollback that fails, and so wedges the host, for a hello it cannot make come.
#[test]
fn a_missing_hello_is_told_in_the_detail_and_never_fails_the_rollback() {
    let rig = begun("config-rollback-silent");
    rig.lease.say(&[Say::Host("live", Some(2000))]);
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the unit was restarted; no hello in 60 s: no new session")
    );
    assert_eq!(rig.clock.pauses.get(), 6);

    let rig = begun("config-rollback-down");
    rig.lease.say(&[Say::Down("cannot talk to the server")]);
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the unit was restarted; no hello in 60 s: cannot talk to the server")
    );

    let rig = begun("config-rollback-bad-flags");
    *rig.flags.borrow_mut() = Some("flags_file: cannot be read".to_owned());
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the unit was restarted; not waiting for a hello: flags_file: cannot be read")
    );
    assert!(rig.record().is_none());
}

/// Asserts that a cutover that never began the restart (no record, or one that says so) leaves
/// the unit alone, and that the rollback run again after it ended restarts nothing.
///
/// Catches a rollback that restarts a healthy agent onto the files it never left, and one that
/// restarts it again when retried.
#[test]
fn a_rollback_with_nothing_restarted_leaves_the_unit_alone() {
    let rig = Rig::new("config-rollback-nothing");
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("cutover restarted nothing: the unit is left as it is")
    );
    rig.put(&rig.fresh());
    assert_eq!(rig.run(rollback).outcome, Outcome::Success);
    assert!(rig.record().is_none());
    assert!(rig.units.log.borrow().is_empty());

    let rig = begun("config-rollback-rerun");
    assert_eq!(rig.run(rollback).outcome, Outcome::Success);
    let calls = rig.calls();
    assert_eq!(rig.run(rollback).outcome, Outcome::Success);
    assert_eq!(rig.calls(), calls);
}

/// Asserts that a unit that does not stop within the rollback's bound (the timeout less the
/// start wait, the hello wait and the margin) or does not start fails the rollback with the
/// reason, and keeps the record, so that a rerun restarts again.
///
/// Catches a rollback that reports success for a unit it left down.
#[test]
fn a_unit_that_cannot_be_restarted_fails_the_rollback_and_keeps_the_record() {
    let rig = begun("config-rollback-stuck");
    rig.units
        .on_stop
        .replace(vec![("deactivating", "stop-sigterm")]);
    let done = rig.run(rollback);
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("js-agent.service is still deactivating after 360 s")
    );
    assert!(rig.record().unwrap().restart_begun);

    let rig = begun("config-rollback-no-start");
    rig.units.on_start.replace(vec![("inactive", "dead")]);
    let done = rig.run(rollback);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert!(rig.record().is_some());
}
