//! `rollback` (host-updates design, 4.2), restoring the previous release: from each state a
//! `cutover` or a `verify` leaves, the unit is stopped (if the new agent runs), the link goes
//! back, the previous agent starts and must say hello. Each act is marked in the record
//! before it is made; each mark of an observation after it. The cordon, the first install,
//! the taint and the lease server's outages have their tests in steps_rollback_guards.rs.

use std::path::PathBuf;

use js_agent::update_hook::{
    Class, Clock, Lease, Link, Outcome, ROLLBACK_HELLO_SECS, ROLLBACK_MARGIN_SECS, Reason,
    START_WAIT_SECS, Units,
};
use serde_json::json;

use super::rollback_entries::{
    RELINK, assert_restored, deactivating, link_moved, link_target, previous, rb_marks, staged,
    start, start_failed, stop, stopped, unit_state, verified, verify_failed,
};
use super::step_rig::{LEASE_SECS, Rig, Spy, UNIT, end_state, killable, line, running};

fn uncordons(spy: &Spy<'_>) -> usize {
    spy.log().iter().filter(|call| *call == "uncordon").count()
}

#[test]
fn rollback_after_the_link_moved_relinks_starts_and_waits_for_the_previous_agent() {
    let rig = link_moved("hook-rollback-link-moved");
    let old_session = rig.host().session_started_ms;
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.reason, None);
    assert_eq!(
        line(&done)["reversible"],
        json!(false),
        "the runs stopped at the cutover are not brought back"
    );
    // The agent is down already: there is nothing to drain or stop.
    assert_eq!(spy.act_names(), [RELINK.to_owned(), start()]);
    assert_restored(&rig);
    assert_ne!(rig.host().session_started_ms, old_session, "a new session");
    // The cordon of this update is lifted last, after the hello.
    assert_eq!(spy.log().last().map(String::as_str), Some("uncordon"));
    assert_eq!(uncordons(&spy), 1);
}

#[test]
fn rollback_when_the_stopped_unit_is_still_linked_to_the_previous_release_only_starts() {
    let rig = stopped("hook-rollback-stopped");
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [start()],
        "the link already names the previous release: the record's mark is not proof"
    );
    assert_restored(&rig);
}

#[test]
fn rollback_after_the_start_was_issued_and_failed_relinks_and_starts_the_failed_unit() {
    let rig = start_failed("hook-rollback-start-failed");
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [RELINK.to_owned(), start()],
        "a failed unit is not stopped"
    );
    assert_restored(&rig);
}

/// The acts of a rollback that drains and stops the new agent, with the `rb` marks on disk
/// when each was made: drained and stop_begun, then link_restored, then start_issued.
fn assert_stopped_relinked_started(spy: &Spy<'_>) {
    let acts = spy.acts.borrow();
    let seen: Vec<(String, [bool; 5])> = acts.iter().map(|a| (a.call.clone(), a.rb)).collect();
    assert_eq!(
        seen,
        [
            (stop(), [true, true, false, false, false]),
            (RELINK.to_owned(), [true, true, true, false, false]),
            (start(), [true, true, true, true, false]),
        ]
    );
}

#[test]
fn rollback_after_a_failed_verify_drains_stops_relinks_and_starts_marking_each_act_first() {
    let rig = verify_failed("hook-rollback-verify-failed");
    assert!(
        rig.cordon_expiry().is_none(),
        "verify waited out the cordon's lease: the setup is a lapsed cordon"
    );
    let new_session = rig.host().session_started_ms;
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_stopped_relinked_started(&spy);
    assert_restored(&rig);
    assert_ne!(rig.host().session_started_ms, new_session);
    // The cordon that lapsed was put back before the agent was stopped, and lifted last.
    let log = spy.log();
    let placed = log
        .iter()
        .position(|call| call.starts_with("cordon("))
        .unwrap();
    let stopped_at = log.iter().position(|call| *call == stop()).unwrap();
    assert!(placed < stopped_at, "{log:?}");
    assert_eq!(log.last().map(String::as_str), Some("uncordon"));
    assert_eq!(uncordons(&spy), 1);
}

#[test]
fn rollback_after_a_successful_verify_does_the_same() {
    let rig = verified("hook-rollback-verified");
    assert!(rig.record().unwrap().verify_ok);
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_stopped_relinked_started(&spy);
    assert_restored(&rig);
    assert_eq!(uncordons(&spy), 1);
    assert_eq!(line(&done)["reversible"], json!(false));
}

#[test]
fn a_new_agent_that_crash_loops_is_stopped_and_replaced_by_the_previous_one() {
    let rig = verified("hook-rollback-crash-loop");
    rig.world.seed_unit(UNIT, "activating", "auto-restart");
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [stop(), RELINK.to_owned(), start()],
        "a unit waiting to restart is the new release running"
    );
    assert_restored(&rig);
}

#[test]
fn a_previous_agent_that_crash_loops_after_its_start_is_a_release_failure() {
    let rig = link_moved("hook-rollback-previous-crash-loop");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("activating", "auto-restart"));
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Release, "the binary ran and died");
    assert!(!done.report.reversible);
    assert_eq!(spy.act_names(), [RELINK.to_owned(), start()]);
    assert!(!rb_marks(&rig)[4], "no hello, no mark");
    assert_eq!(
        uncordons(&spy),
        0,
        "the cordon stays until the previous agent is up"
    );
}

#[test]
fn a_previous_agent_that_cannot_be_started_fails_the_rollback_and_a_rerun_only_starts() {
    let rig = link_moved("hook-rollback-start-refused");
    let spy = Spy::new(&rig);
    spy.fail("start");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment, "it never ran");
    assert!(!done.report.reversible);
    assert_eq!(rb_marks(&rig), [false, false, true, true, false]);
    let rerun = Spy::new(&rig);
    let done = rerun.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rerun.act_names(), [start()], "the link is not moved twice");
    assert_restored(&rig);
}

#[test]
fn a_link_that_cannot_be_moved_back_fails_the_rollback_before_the_start() {
    let rig = link_moved("hook-rollback-relink-refused");
    let spy = Spy::new(&rig);
    spy.fail("relink");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::LinkFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!done.report.reversible);
    assert_eq!(spy.act_names(), [RELINK.to_owned()]);
    assert_eq!(
        unit_state(&rig),
        "inactive dead",
        "nothing starts on the staged release"
    );
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
    assert_restored(&rig);
}

#[test]
fn a_unit_that_does_not_stop_in_time_fails_the_rollback_before_the_link_moves() {
    let rig = verified("hook-rollback-stop-stuck");
    let spy = Spy::new(&rig);
    *spy.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    let ticks = rig.world.ticks();
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!done.report.reversible);
    assert_eq!(spy.act_names(), [stop()]);
    let declared = rig.declaration.timeouts.rollback;
    let bound = declared
        - rig.params.drain_wait_secs
        - START_WAIT_SECS
        - ROLLBACK_HELLO_SECS
        - ROLLBACK_MARGIN_SECS;
    assert_eq!(
        rig.world.ticks() - ticks,
        usize::try_from(bound / 10).unwrap(),
        "the wait for the stop ends before the declared bound by the other waits and a margin"
    );
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    // The unit is down on a rerun: it is not signalled a second time.
    rig.world.seed_unit(UNIT, "inactive", "dead");
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), [RELINK.to_owned(), start()]);
}

#[test]
fn a_unit_still_stopping_when_rollback_starts_is_waited_for_and_not_stopped_again() {
    // A cutover killed while the unit stops: the link is still the previous release's.
    let rig = running(Rig::new("hook-rollback-deactivating"));
    let killed = Spy::new(&rig);
    *killed.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    killed.on_pause(|_, ticks| assert!(ticks < 3, "killed during the wait"));
    assert!(killable(|| killed.cutover(&rig)).is_none());
    rig.kill.disarm();
    rig.world.pause(super::rollback_entries::WAIT);
    assert_eq!(unit_state(&rig), "deactivating stop-sigterm");
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 2 {
            spy.world.seed_unit(UNIT, "inactive", "dead");
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.world.ticks() - base, 2);
    assert_eq!(
        spy.act_names(),
        [start()],
        "not stopped again, and no link to move"
    );
    assert_restored(&rig);
}

#[test]
fn a_previous_agent_that_still_runs_on_the_previous_release_needs_no_act_only_its_hello() {
    // The stop was begun but the unit would not stop: the old agent runs, on the old link.
    let rig = running(Rig::new("hook-rollback-never-stopped"));
    let failing = Spy::new(&rig);
    failing.fail("stop");
    assert_eq!(failing.cutover(&rig).outcome, Outcome::Failure);
    rig.kill.disarm();
    rig.world.pause(super::rollback_entries::WAIT);
    assert_eq!(unit_state(&rig), "active running");
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_restored(&rig);
    assert_eq!(uncordons(&spy), 1);
}

#[test]
fn a_unit_up_on_a_link_that_is_neither_release_is_refused_and_nothing_is_done() {
    let rig = verified("hook-rollback-foreign-link");
    let foreign = rig.tmp.path().join("releases/9.9.9");
    rig.world.relink(&rig.link, &foreign).unwrap();
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(rig.record_bytes(), before);
    assert_eq!(link_target(&rig), Some(foreign));
}

#[test]
fn a_rollback_without_the_release_link_in_the_params_is_refused_before_the_machine_is_touched() {
    let rig = verified("hook-rollback-no-link-param");
    let mut params = rig.params.clone();
    params.release_link = None;
    let spy = Spy::new(&rig);
    let done =
        js_agent::update_hook::step::rollback(&spy.env(&rig, &super::step_rig::as_root), &params);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::BadParams));
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

// The previous agent's hello is the proof, not the unit being active.

#[test]
fn the_session_the_server_showed_before_the_start_is_not_proof_even_of_the_previous_version() {
    let rig = link_moved("hook-rollback-old-session");
    // The server still shows the old agent's session, of the previous version.
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::NoHello));
    assert_eq!(done.report.class, Class::Release);
    assert!(!done.report.reversible);
    assert_eq!(spy.act_names(), [RELINK.to_owned(), start()]);
    assert!(
        !rig.record().unwrap().rb.hello_ok,
        "the mark follows the observation"
    );
    assert_eq!(uncordons(&spy), 0);
    // The agent says hello later; a rerun waits for it and acts no more.
    let rerun = Spy::new(&rig);
    let base = rig.world.ticks();
    rerun.on_pause(move |spy, ticks| {
        if ticks == base + 2 {
            spy.world.hello("1.3.0").unwrap();
        }
    });
    let done = rerun.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rerun.act_names(), Vec::<String>::new());
    assert_eq!(rig.world.ticks() - base, 2);
    assert_restored(&rig);
}

#[test]
fn the_wait_for_the_hello_ends_at_its_bound() {
    let rig = link_moved("hook-rollback-hello-bound");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let ticks = rig.world.ticks();
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::NoHello));
    assert_eq!(
        rig.world.ticks() - ticks,
        usize::try_from(ROLLBACK_HELLO_SECS / 10).unwrap()
    );
}

#[test]
fn the_new_agents_own_session_is_not_proof_after_a_failed_verify() {
    // The new agent said hello (on the wrong runtime, so that verify failed) and runs.
    let rig = running(Rig::new("hook-rollback-new-agents-session"));
    rig.world.pause(super::rollback_entries::WAIT);
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    rig.world
        .alter_host(|host| host.runtime = Some("docker".to_owned()));
    let verify = Spy::new(&rig).verify(&rig);
    assert_eq!(
        verify.report.reason,
        Some(Reason::VersionMismatch),
        "{verify:?}"
    );
    rig.kill.disarm();
    rig.world.pause(super::rollback_entries::WAIT);
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.4.0"));
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    // Not a wrong version: the session is the new agent's own, and is not another one.
    assert_eq!(done.report.reason, Some(Reason::NoHello));
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.4.0"));
}

#[test]
fn a_new_session_of_another_version_is_not_proof() {
    let rig = link_moved("hook-rollback-wrong-version");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 2 {
            spy.world.hello("1.9.0").unwrap();
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::VersionMismatch));
    assert_eq!(done.report.class, Class::Release);
    assert!(done.report.detail.unwrap().contains("1.9.0"));
    assert!(!rig.record().unwrap().rb.hello_ok);
}

#[test]
fn a_new_session_on_another_runtime_is_not_proof() {
    let rig = link_moved("hook-rollback-wrong-runtime");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 2 {
            spy.world.hello("1.3.0").unwrap();
            spy.world
                .alter_host(|host| host.runtime = Some("docker".to_owned()));
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(
        done.report.reason,
        Some(Reason::VersionMismatch),
        "{done:?}"
    );
}

#[test]
fn a_unit_that_dies_during_the_hello_wait_is_a_release_failure() {
    let rig = link_moved("hook-rollback-dies-waiting");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 1 {
            spy.world.seed_unit(UNIT, "failed", "failed");
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::StartFailed), "{done:?}");
    assert_eq!(done.report.class, Class::Release);
}

// Re-running: every kill point, every act at most once.

/// The agent a start began says hello on its own, some time after: when the unit is up on the
/// previous release and the server still shows the session of before.
fn agent_says_hello(
    spy: &Spy<'_>,
    link: &std::path::Path,
    previous: &std::path::Path,
    old_session: Option<u64>,
) {
    // A unit that was stopping finishes stopping.
    if spy.world.show(UNIT).unwrap().active == "deactivating" {
        spy.world.seed_unit(UNIT, "inactive", "dead");
    }
    let linked = spy.world.target(link).unwrap().as_deref() == Some(previous);
    let said = spy.world.host().unwrap().session_started_ms != old_session;
    if linked && !said && spy.world.is_active(UNIT).unwrap() {
        spy.world.hello("1.3.0").unwrap();
    }
}

/// Runs the rollback of `build`'s host to its end; then, from a fresh host, once for each kill
/// point it passes: killed there, run again, and the same end required, with every act made
/// as often as in the uninterrupted run. With `lapsed_ok` the cordon may be left in force at the
/// end where it was placed again and the kill came between the placing and the record of it:
/// the record then cannot say it is ours, so it is not lifted, and it lapses within a lease.
fn sweep(label: &str, build: fn(&str) -> Rig, lapsed_ok: bool) {
    let baseline = build(&format!("{label}-base"));
    let (blink, bprev) = (baseline.link.clone(), previous(&baseline));
    let bsession = baseline.host().session_started_ms;
    let spy = Spy::new(&baseline);
    spy.on_pause(move |spy, _| agent_says_hello(spy, &blink, &bprev, bsession));
    assert_eq!(spy.rollback(&baseline).outcome, Outcome::Success);
    let want = spy.act_names();
    let points = baseline.kill.points();
    assert!(
        points >= 8,
        "the marks, the acts and the record pass kill points: {points}"
    );
    let end = end_state(&baseline);
    for k in 1..=points {
        let rig = build(&format!("{label}-{k}"));
        let session = rig.host().session_started_ms;
        let (link, prev) = (rig.link.clone(), previous(&rig));
        rig.kill.arm(k);
        let first = Spy::new(&rig);
        let (flink, fprev) = (link.clone(), prev.clone());
        first.on_pause(move |spy, _| agent_says_hello(spy, &flink, &fprev, session));
        assert!(
            killable(|| first.rollback(&rig)).is_none(),
            "kill point {k} of {points} is reached"
        );
        rig.kill.disarm();
        // The session before the first act is never on disk apart from the mark of that act.
        if let Some(record) = rig.record() {
            let acted = record.rb.stop_begun || record.rb.link_restored || record.rb.start_issued;
            assert_eq!(record.rb_pre_stop_session_ms.is_some(), acted, "point {k}");
            if acted {
                assert_eq!(record.rb_pre_stop_session_ms, session, "point {k}");
            }
        }
        // An act logged and then killed inside the fake (before it changed anything) was not made.
        let mut made: Vec<_> = first.acts.borrow().clone();
        if made.last().is_some_and(|act| act.points + 1 == k) {
            made.pop();
        }
        let rerun = Spy::new(&rig);
        rerun.on_pause(move |spy, _| agent_says_hello(spy, &link, &prev, session));
        let done = rerun.rollback(&rig);
        assert_eq!(
            done.outcome,
            Outcome::Success,
            "killed at point {k}: {done:?}"
        );
        let mut names: Vec<String> = made.iter().map(|act| act.call.clone()).collect();
        names.extend(rerun.act_names());
        assert_eq!(names, want, "killed at point {k}");
        let mut got = end_state(&rig);
        let mut want_end = end.clone();
        if lapsed_ok {
            let left = got["cordon"].as_u64();
            let lapses = rig.world.now_ms() + LEASE_SECS * 1000;
            assert!(
                left.is_none_or(|expiry| expiry <= lapses),
                "killed at point {k}: {left:?}"
            );
            got["cordon"] = serde_json::Value::Null;
            want_end["cordon"] = serde_json::Value::Null;
        }
        assert_eq!(got, want_end, "killed at point {k}");
    }
}

#[test]
fn a_rollback_of_a_link_moved_host_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one()
{
    sweep("hook-rollback-sweep-linked", link_moved, false);
}

#[test]
fn a_rollback_of_a_failed_verify_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    sweep("hook-rollback-sweep-verify", verify_failed, true);
}

/// Kills the rollback of `build`'s host just before each of its acts: the marks of that act are
/// on disk, the act is not made, and a rerun makes it and the rest and nothing else.
fn killed_before_each_act(label: &str, build: fn(&str) -> Rig) {
    let baseline = build(&format!("{label}-base"));
    let spy = Spy::new(&baseline);
    assert_eq!(spy.rollback(&baseline).outcome, Outcome::Success);
    let acts = spy.acts.borrow().clone();
    let end = end_state(&baseline);
    for (k, act) in acts.iter().enumerate() {
        let rig = build(&format!("{label}-{k}"));
        let (link, prev) = (rig.link.clone(), previous(&rig));
        let session = rig.host().session_started_ms;
        rig.kill.arm(act.points);
        let first = Spy::new(&rig);
        assert!(killable(|| first.rollback(&rig)).is_none());
        rig.kill.disarm();
        assert_eq!(rb_marks(&rig), act.rb, "killed before {}", act.call);
        let before: Vec<String> = acts[..k].iter().map(|a| a.call.clone()).collect();
        assert_eq!(first.act_names(), before, "killed before {}", act.call);
        let rerun = Spy::new(&rig);
        rerun.on_pause(move |spy, _| agent_says_hello(spy, &link, &prev, session));
        assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
        let after: Vec<String> = acts[k..].iter().map(|a| a.call.clone()).collect();
        assert_eq!(rerun.act_names(), after, "killed before {}", act.call);
        assert_eq!(end_state(&rig), end);
    }
}

#[test]
fn a_rollback_of_a_host_whose_unit_is_stopped_or_stopping_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one()
 {
    sweep("hook-rollback-sweep-stopped", stopped, false);
    sweep("hook-rollback-sweep-start-failed", start_failed, false);
    sweep("hook-rollback-sweep-stopping", deactivating, false);
}

#[test]
fn a_rollback_killed_just_before_each_act_has_that_acts_mark_and_a_rerun_makes_that_act_and_the_rest()
 {
    killed_before_each_act("hook-rollback-before-linked", link_moved);
    killed_before_each_act("hook-rollback-before-verify", verify_failed);
}

#[test]
fn a_rollback_run_twice_does_nothing_the_second_time() {
    let rig = verified("hook-rollback-twice");
    assert_eq!(Spy::new(&rig).rollback(&rig).outcome, Outcome::Success);
    let before = end_state(&rig);
    let second = Spy::new(&rig);
    let done = second.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(second.act_names(), Vec::<String>::new());
    assert_eq!(end_state(&rig), before);
    assert!(done.report.detail.unwrap().contains("no record"));
}

#[test]
fn a_rollback_with_no_record_has_nothing_to_undo_and_says_so() {
    let rig = Rig::new("hook-rollback-no-record");
    rig.world.seed_unit(UNIT, "active", "running");
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(line(&done)["reversible"], json!(true));
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert!(spy.log().iter().all(|call| !call.contains("cordon")));
    assert_eq!(rig.record(), None);
}

// The drain.

#[test]
fn a_lease_after_the_drain_bound_fails_the_rollback_and_stops_nothing_and_a_rerun_starts_again() {
    let rig = verified("hook-rollback-lease-after-bound");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let ticks = rig.world.ticks();
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::DrainBound));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!done.report.reversible);
    assert!(done.report.detail.unwrap().contains("lease"));
    assert_eq!(
        spy.act_names(),
        Vec::<String>::new(),
        "the agent is not signalled"
    );
    assert_eq!(
        rig.world.ticks() - ticks,
        usize::try_from(rig.params.drain_wait_secs / 10).unwrap()
    );
    assert!(!rig.record().unwrap().rb.drained, "no drain was seen");
    assert_eq!(unit_state(&rig), "active running");
    assert_eq!(
        uncordons(&spy),
        0,
        "the cordon is left to lapse, not lifted"
    );
    // The lease ends: the next rollback drains again and goes on.
    rig.set_leases(0);
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), [stop(), RELINK.to_owned(), start()]);
    assert_restored(&rig);
}

#[test]
fn a_lease_that_ends_during_the_drain_lets_the_rollback_go_on_and_the_cordon_is_kept_up() {
    let rig = verified("hook-rollback-drain-waits");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 40 {
            spy.world.alter_host(|host| {
                host.leases = 0;
                host.lease = None;
            });
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.world.ticks() - base, 40);
    // 400 s of drain are more than the cordon's 300 s lease: it was renewed.
    assert!(spy.log().iter().any(|call| call.starts_with("cordon(")));
    assert_stopped_relinked_started(&spy);
}

#[test]
fn a_resumed_rollback_does_not_drain_again_once_the_drain_was_seen() {
    let rig = verified("hook-rollback-drained-already");
    let mut record = rig.record().unwrap();
    record.rb.drained = true;
    rig.put_record(&record);
    // A lease that came in after the mark: the resumed rollback does not wait for it.
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let ticks = rig.world.ticks();
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.world.ticks(), ticks, "no wait");
    assert_eq!(spy.act_names(), [stop(), RELINK.to_owned(), start()]);
}

#[test]
fn a_lapsed_cordon_that_the_cap_leaves_no_time_for_fails_the_rollback_before_the_stop() {
    let rig = verified("hook-rollback-cap-spent");
    // Time passes until the cap counted from the first placement is spent and the lease lapsed.
    rig.world.pause(std::time::Duration::from_secs(
        rig.params.cordon_max_secs + 1,
    ));
    assert!(rig.cordon_expiry().is_none());
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::CordonCapReached));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!done.report.reversible);
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(uncordons(&spy), 0);
}

#[test]
fn the_previous_release_directory_is_what_the_link_goes_back_to() {
    let rig = verified("hook-rollback-link-target");
    let want: PathBuf = previous(&rig);
    assert_ne!(link_target(&rig), Some(want.clone()));
    assert_eq!(Spy::new(&rig).rollback(&rig).outcome, Outcome::Success);
    assert_eq!(link_target(&rig), Some(want));
}

#[test]
fn a_unit_that_cannot_be_signalled_fails_the_rollback_with_the_stop_marked_and_a_rerun_signals_it_again()
 {
    let rig = verified("hook-rollback-stop-refused");
    let spy = Spy::new(&rig);
    spy.fail("stop");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(spy.act_names(), [stop()]);
    assert_eq!(rb_marks(&rig), [true, true, false, false, false]);
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), [stop(), RELINK.to_owned(), start()]);
    assert_restored(&rig);
}

#[test]
fn a_host_that_holds_no_lease_but_has_runs_to_finish_is_still_draining() {
    let rig = verified("hook-rollback-releasing");
    rig.world.alter_host(|host| host.releasing = true);
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 3 {
            spy.world.alter_host(|host| host.releasing = false);
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.world.ticks() - base, 3);
    assert_restored(&rig);
}

#[test]
fn a_hello_that_names_no_runtime_is_checked_by_version_alone() {
    let rig = link_moved("hook-rollback-no-runtime");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 1 {
            spy.world.hello("1.3.0").unwrap();
            spy.world.alter_host(|host| host.runtime = None);
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}

#[test]
fn a_unit_that_is_still_starting_when_rollback_begins_is_waited_for_not_started_again() {
    // The link is on the previous release and the unit is on its way up.
    let rig = stopped("hook-rollback-starting");
    rig.world.seed_unit(UNIT, "activating", "start");
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 2 {
            spy.world.seed_unit(UNIT, "active", "running");
            spy.world.hello("1.3.0").unwrap();
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(rig.world.ticks() - base, 2);
}

#[test]
fn a_previous_agent_seen_waiting_to_restart_while_awaited_is_a_release_failure() {
    let rig = stopped("hook-rollback-restart-wait");
    rig.world.seed_unit(UNIT, "activating", "auto-restart");
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Release);
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

#[test]
fn a_server_that_restarted_and_shows_the_host_awaiting_is_no_proof_until_the_hello() {
    let rig = link_moved("hook-rollback-awaiting");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| match ticks - base {
        1 => spy.world.forget_host(),
        4 => spy.world.hello("1.3.0").unwrap(),
        _ => {}
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.world.ticks() - base, 4);
    assert_restored(&rig);
}

#[test]
fn a_unit_manager_that_cannot_be_read_fails_the_rollback_before_anything_is_marked() {
    let rig = verified("hook-rollback-show-fails");
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    spy.fail("show");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(rig.record_bytes(), before);
}

#[test]
fn a_release_link_that_cannot_be_read_fails_the_rollback_before_anything_is_marked() {
    let rig = link_moved("hook-rollback-link-unreadable");
    std::fs::remove_file(rig.tmp.path().join("world/link/current")).unwrap();
    std::fs::write(rig.tmp.path().join("world/link/current"), b"not a link").unwrap();
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::LinkFailed));
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(rig.record_bytes(), before);
}

#[test]
fn a_unit_manager_that_goes_dark_while_the_unit_stops_ends_the_rollback_as_environment() {
    let rig = running(Rig::new("hook-rollback-dark-in-stop"));
    let killed = Spy::new(&rig);
    *killed.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    killed.on_pause(|_, ticks| assert!(ticks < 3, "killed during the wait"));
    assert!(killable(|| killed.cutover(&rig)).is_none());
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 1 {
            spy.fail("show");
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

#[test]
fn a_unit_manager_that_goes_dark_while_the_previous_agent_is_awaited_ends_the_rollback() {
    let rig = link_moved("hook-rollback-dark-in-hello");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 1 {
            spy.fail("show");
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
}

#[test]
fn a_previous_agent_whose_unit_goes_inactive_or_deactivating_while_awaited_is_a_release_failure() {
    for (name, active) in [
        ("hook-rollback-inactive-waiting", "inactive"),
        ("hook-rollback-deactivating-waiting", "deactivating"),
    ] {
        let rig = link_moved(name);
        let spy = Spy::new(&rig);
        *spy.start_leaves.borrow_mut() = Some(("active", "running"));
        let base = rig.world.ticks();
        spy.on_pause(move |spy, ticks| {
            if ticks == base + 1 {
                spy.world.seed_unit(UNIT, active, "dead");
            }
        });
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Failure, "{active}: {done:?}");
        assert_eq!(done.report.reason, Some(Reason::StartFailed), "{active}");
        assert_eq!(done.report.class, Class::Release, "{active}");
    }
}
