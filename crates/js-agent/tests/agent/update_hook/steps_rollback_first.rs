//! `rollback` of a first install (rollback.rs, item 3): there is no previous release to start
//! again, so the step takes down the agent `cutover` started and ends in success. It stops the
//! unit only once the host can hold no work: the lease server knows no such host, does not take
//! the hook's credential as the host's, or shows it holding no lease under a cordon of this
//! update's. A server that does not answer, and a lease that outlasts the drain bound, end the
//! step with nothing stopped.

use std::time::Duration;

use js_agent::update_hook::{
    Class, Clock, Lease, Outcome, POLL_SECS, ROLLBACK_HELLO_SECS, ROLLBACK_MARGIN_SECS, Reason,
    START_WAIT_SECS, Units,
};
use serde_json::json;

use super::rollback_entries::{first_prepared, stop, unit_state};
use super::step_rig::{DRAIN_WAIT_SECS, Rig, Spy, UNIT, killable, line};

/// The calls to the lease server's cordon routes.
fn cordon_calls(spy: &Spy<'_>) -> Vec<String> {
    spy.log()
        .into_iter()
        .filter(|call| call.starts_with("cordon(") || call == "uncordon")
        .collect()
}

/// A first install after `cutover`. With `hello` the new agent said hello and the server knows
/// the host; without, the agent was refused at its hello (or has not said it) and the server
/// knows no host. The unit is `active running` either way.
fn cut_over(name: &str, hello: bool) -> Rig {
    let rig = first_prepared(name);
    let done = {
        let spy = Spy::new(&rig);
        if !hello {
            *spy.start_leaves.borrow_mut() = Some(("active", "running"));
        }
        spy.cutover(&rig)
    };
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(rig.record().unwrap().link_moved);
    rig.kill.disarm();
    assert_eq!(rig.world.host().is_ok(), hello);
    rig
}

/// The end of a rolled-back first install: the unit is down, and nothing of the update is left.
fn assert_retired(rig: &Rig) {
    assert_eq!(unit_state(rig), "inactive dead");
    assert_eq!(rig.record(), None, "the record is deleted");
    assert_eq!(
        rig.cordon_expiry(),
        None,
        "no cordon of this update is left"
    );
}

/// The farm's case: the agent was refused at its hello (`shape-not-accepted`), exits 2 and is
/// started again by the manager, so verify sees `auto-restart` and fails. The rollback stops
/// the unit and succeeds, with no cordon (the server knows no host) and no start; it says the
/// update cannot be undone, and a rerun finds no record and succeeds too. Catches a rollback that
/// fails `no_previous_release` and leaves the unit restarting.
#[test]
fn a_first_install_whose_verify_fails_rolls_back_with_success_and_leaves_no_unit_running() {
    let rig = cut_over("hook-rollback-first-refused", false);
    rig.world.seed_unit(UNIT, "activating", "auto-restart");
    let verified = Spy::new(&rig).verify(&rig);
    assert_eq!(verified.report.reason, Some(Reason::StartFailed));
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.reason, None);
    assert_eq!(
        line(&done)["reversible"],
        json!(false),
        "the link was moved"
    );
    assert_eq!(spy.act_names(), [stop()], "one stop, no start");
    assert_eq!(cordon_calls(&spy), Vec::<String>::new());
    assert!(
        !spy.log().iter().any(|call| call.contains("enable")),
        "{:?}",
        spy.log()
    );
    assert_retired(&rig);
    let again = Spy::new(&rig);
    assert_eq!(again.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(again.act_names(), Vec::<String>::new());
    assert_retired(&rig);
}

/// A server that does not take the hook's credential as the host's (the agent was never
/// welcomed, so the hook presents the join token) gives the host no work: the unit is stopped
/// with no cordon asked for, for each code that says so.
#[test]
fn a_server_that_does_not_take_the_credential_as_the_hosts_lets_the_stop_go_ahead() {
    for (n, code) in [
        "host-credential-required",
        "join-token-not-admitted",
        "join-token-expired",
        "http-401",
    ]
    .into_iter()
    .enumerate()
    {
        let rig = cut_over(&format!("hook-rollback-first-credential-{n}"), false);
        let spy = Spy::new(&rig);
        spy.host_refuses.set(Some(code));
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{code}: {done:?}");
        assert_eq!(spy.act_names(), [stop()], "{code}");
        assert_eq!(cordon_calls(&spy), Vec::<String>::new(), "{code}");
        assert_retired(&rig);
    }
}

/// An agent that said hello may hold work: the host is cordoned, the rollback waits under the
/// cordon until its lease ends, then stops the unit and lifts the cordon. Catches a stop that
/// lands on a running lease.
#[test]
fn a_first_install_whose_agent_said_hello_is_drained_under_a_cordon_before_the_stop() {
    let rig = cut_over("hook-rollback-first-drain", true);
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        assert_eq!(
            spy.world.show(UNIT).unwrap().active,
            "active",
            "not stopped while a lease runs"
        );
        if ticks == base + 2 {
            spy.world.alter_host(|host| {
                host.leases = 0;
                host.lease = None;
            });
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        rig.world.ticks() - base,
        2,
        "it stops at the look after the lease"
    );
    let log = spy.log();
    let cordoned = log.iter().position(|c| c.starts_with("cordon(")).unwrap();
    let stopped = log.iter().position(|c| *c == stop()).unwrap();
    let lifted = log.iter().position(|c| c == "uncordon").unwrap();
    assert!(cordoned < stopped && stopped < lifted, "{log:?}");
    assert_eq!(spy.act_names(), [stop()]);
    assert_retired(&rig);
}

/// A lease still held at the drain bound ends the step `drain_bound` (class environment) with
/// the agent running and the record kept; once the lease ends, a rerun stops it and succeeds.
#[test]
fn a_lease_that_outlasts_the_drain_bound_leaves_the_agent_running_until_a_rerun() {
    let rig = cut_over("hook-rollback-first-bound", true);
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::DrainBound));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!done.report.reversible);
    assert_eq!(
        rig.world.ticks() - base,
        usize::try_from(DRAIN_WAIT_SECS / POLL_SECS).unwrap(),
        "it waited the drain bound, one look per poll"
    );
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(unit_state(&rig), "active running");
    assert!(rig.record().is_some());
    rig.set_leases(0);
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), [stop()]);
    assert_retired(&rig);
}

/// The longest one outage of the lease server is waited for: the declared rollback bound less
/// the start wait, the hello wait and the margin.
fn outage_ticks(rig: &Rig) -> usize {
    let secs = rig.params.settings.timeouts.rollback
        - START_WAIT_SECS
        - ROLLBACK_HELLO_SECS
        - ROLLBACK_MARGIN_SECS;
    usize::try_from(secs.div_ceil(POLL_SECS)).unwrap()
}

/// A server that does not answer, or refuses for a reason that says nothing of the credential
/// (a 503), cannot say the host holds no work: it is waited for up to the outage bound, then the
/// step fails `lease_unreachable` (class environment) with nothing stopped and the record kept.
/// When the server answers again, a rerun goes on.
#[test]
fn a_server_that_does_not_answer_is_waited_for_and_then_nothing_is_stopped() {
    for (n, refused) in [None, Some("http-503")].into_iter().enumerate() {
        let rig = cut_over(&format!("hook-rollback-first-outage-{n}"), true);
        let spy = Spy::new(&rig);
        match refused {
            Some(code) => spy.host_refuses.set(Some(code)),
            None => spy.fail("host"),
        }
        let base = rig.world.ticks();
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Failure, "{refused:?}: {done:?}");
        assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
        assert_eq!(done.report.class, Class::Environment);
        assert_eq!(rig.world.ticks() - base, outage_ticks(&rig), "{refused:?}");
        assert_eq!(spy.act_names(), Vec::<String>::new(), "{refused:?}");
        assert_eq!(unit_state(&rig), "active running");
        assert!(rig.record().is_some());
        let rerun = Spy::new(&rig);
        assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
        assert_retired(&rig);
    }
}

/// An outage that ends inside the bound is not a failure: the answer that ends it is judged as
/// any other, here a server that knows no host.
#[test]
fn a_server_that_answers_again_within_the_bound_lets_the_stop_go_ahead() {
    let rig = cut_over("hook-rollback-first-outage-ends", false);
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
    assert_eq!(spy.act_names(), [stop()]);
    assert_retired(&rig);
}

/// A unit that is down already (the agent died and the manager gave up) needs no stop and no
/// word from the lease server; one that is stopping is waited for. Both end in success. The
/// lease server is down throughout, so a step that asked it would fail.
#[test]
fn a_unit_that_is_down_or_stopping_is_left_to_stop_without_asking_the_server() {
    for (n, (active, sub, ends)) in [
        ("failed", "failed", "failed failed"),
        ("inactive", "dead", "inactive dead"),
        ("deactivating", "stop-sigterm", "inactive dead"),
    ]
    .into_iter()
    .enumerate()
    {
        let rig = cut_over(&format!("hook-rollback-first-down-{n}"), false);
        rig.world.seed_unit(UNIT, active, sub);
        let spy = Spy::new(&rig);
        spy.fail("host");
        spy.on_pause(|spy, _| spy.world.seed_unit(UNIT, "inactive", "dead"));
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{active}: {done:?}");
        assert_eq!(spy.act_names(), Vec::<String>::new(), "{active}");
        assert_eq!(rig.record(), None, "{active}");
        assert_eq!(unit_state(&rig), ends, "{active}");
    }
}

/// A unit that does not stop within the bound ends the step `stop_failed` (class environment),
/// the record kept for a rerun.
#[test]
fn a_unit_that_does_not_stop_fails_the_step_as_the_environments() {
    let rig = cut_over("hook-rollback-first-stuck", false);
    let spy = Spy::new(&rig);
    *spy.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(rig.record().is_some());
    rig.world.seed_unit(UNIT, "inactive", "dead");
    assert_eq!(Spy::new(&rig).rollback(&rig).outcome, Outcome::Success);
    assert_retired(&rig);
}

/// A unit manager that cannot stop the unit fails the step `stop_failed` (class environment).
#[test]
fn a_stop_the_manager_refuses_fails_the_step_as_the_environments() {
    let rig = cut_over("hook-rollback-first-stop-refused", false);
    let spy = Spy::new(&rig);
    spy.fail("stop");
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::StopFailed), "{done:?}");
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(unit_state(&rig), "active running");
    assert!(rig.record().is_some());
}

/// A taint halts the step (76) before the stop: a tainted agent refuses every start, and the
/// host is the operator's to look at.
#[test]
fn a_tainted_first_install_halts_before_the_stop() {
    let rig = cut_over("hook-rollback-first-taint", false);
    rig.taint();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::Tainted));
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(unit_state(&rig), "active running");
}

/// A rollback killed at any kill point, with the host known and cordoned on the way, ends the
/// same when run again: the unit down, no record. The stop has no mark: a rerun reads the unit.
#[test]
fn a_first_install_rollback_killed_at_any_point_ends_the_same_on_the_rerun() {
    let mut killed = 0;
    for n in 1.. {
        let rig = cut_over(&format!("hook-rollback-first-kill-{n}"), true);
        rig.kill.arm(n);
        if killable(|| Spy::new(&rig).rollback(&rig)).is_some() {
            break;
        }
        killed += 1;
        rig.kill.disarm();
        rig.world.pause(Duration::from_secs(POLL_SECS));
        let done = Spy::new(&rig).rollback(&rig);
        assert_eq!(done.outcome, Outcome::Success, "killed at {n}: {done:?}");
        assert_eq!(unit_state(&rig), "inactive dead", "killed at {n}");
        assert_eq!(rig.record(), None, "killed at {n}");
    }
    assert!(
        killed >= 4,
        "the cordon, its mark, the stop and the delete: {killed}"
    );
}
