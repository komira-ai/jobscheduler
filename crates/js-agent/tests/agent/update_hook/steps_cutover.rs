//! `cutover` (host-updates design, 4.2): stop the unit and start it again on the staged release,
//! each act marked in the record before it is made and each a no-op when its effect is already
//! there. The updater has moved the release link to the staged release before the step runs
//! (`Spy::cutover` does that); the hook only reads it. Once the stop is begun every ending says
//! `reversible: false`. The cordon (whose it
//! is, when it is lost) has its tests in steps_cutover_cordon.rs.

use std::path::PathBuf;

use js_agent::update_hook::{Class, Link, Outcome, Reason, Units};
use serde_json::json;

use super::step_rig::{Rig, Spy, UNIT, end_state, killable, line, marks, pre_stop, running};

fn stop() -> String {
    format!("units.stop({UNIT})")
}

fn start() -> String {
    format!("units.start({UNIT})")
}

fn previous(rig: &Rig) -> PathBuf {
    rig.params.previous_release.clone().unwrap()
}

fn staged(rig: &Rig) -> PathBuf {
    rig.params.staged_release.clone()
}

fn unit_state(rig: &Rig) -> String {
    let state = rig.world.show(UNIT).unwrap();
    format!("{} {}", state.active, state.sub)
}

fn link_target(rig: &Rig) -> Option<PathBuf> {
    rig.world.resolved_target(&rig.link).unwrap()
}

#[test]
fn a_clean_cutover_marks_each_act_before_it_and_stops_and_starts_in_that_order() {
    let rig = running(Rig::new("hook-cutover-clean"));
    assert_eq!(link_target(&rig), Some(previous(&rig)));
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.report.reason, None);
    assert_eq!(
        line(&done)["reversible"],
        json!(false),
        "the stop is the commit"
    );
    // Each act found the record already saying it was begun, and no later one.
    let acts: Vec<_> = spy
        .acts
        .borrow()
        .iter()
        .map(|act| (act.call.clone(), act.marks))
        .collect();
    assert_eq!(
        acts,
        [
            (stop(), [true, false, false]),
            (start(), [true, true, true]),
        ]
    );
    let record = rig.record().unwrap();
    assert_eq!(marks(&rig), [true, true, true]);
    assert!(!record.verify_ok, "verify's mark is not cutover's");
    assert!(
        record.cordon.placed,
        "the cordon is held for verify and cleanup"
    );
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    assert_eq!(unit_state(&rig), "active running");
    assert_eq!(rig.cordon_expiry(), Some(record.cordon.expires_ms));
    // The cordon is not touched, nothing waits, and `is-active` is never asked: it cannot tell
    // a unit that failed from one that is stopped.
    let log = spy.log();
    assert!(
        log.iter()
            .all(|call| !call.starts_with("cordon(") && call != "uncordon")
    );
    assert!(log.iter().all(|call| !call.starts_with("units.is_active")));
    assert!(log.iter().any(|call| call.starts_with("units.show")));
    assert_eq!(rig.world.ticks(), 0);
}

#[test]
fn a_cutover_killed_just_before_each_act_has_its_mark_and_a_rerun_does_that_act_and_the_rest() {
    let baseline = running(Rig::new("hook-cutover-acts-base"));
    let spy = Spy::new(&baseline);
    assert_eq!(spy.cutover(&baseline).outcome, Outcome::Success);
    let acts: Vec<(String, u32)> = spy
        .acts
        .borrow()
        .iter()
        .map(|act| (act.call.clone(), act.points))
        .collect();
    assert_eq!(acts.len(), 2);
    let expected_marks = [[true, false, false], [true, true, true]];
    for (k, (call, points)) in acts.iter().enumerate() {
        let rig = running(Rig::new(&format!("hook-cutover-acts-{k}")));
        rig.kill.arm(*points);
        assert!(
            killable(|| Spy::new(&rig).cutover(&rig)).is_none(),
            "kill point {points} is reached"
        );
        rig.kill.disarm();
        assert_eq!(marks(&rig), expected_marks[k], "killed before {call}");
        // The act itself was not made.
        match k {
            0 => assert_eq!(unit_state(&rig), "active running"),
            _ => assert_eq!(unit_state(&rig), "inactive dead"),
        }
        let rerun = Spy::new(&rig);
        let done = rerun.cutover(&rig);
        assert_eq!(done.outcome, Outcome::Success, "rerun after {call}");
        assert_eq!(line(&done)["reversible"], json!(false));
        let remaining: Vec<String> = acts[k..].iter().map(|(call, _)| call.clone()).collect();
        assert_eq!(rerun.act_names(), remaining, "killed before {call}");
        assert_eq!(
            end_state(&rig),
            end_state(&baseline),
            "killed before {call}"
        );
    }
}

/// Runs `cutover` of `build`'s host to its end, then again from a fresh host once for each
/// kill point it passes, killed there and run again, and requires the same end every time.
fn sweep(label: &str, build: fn(&str) -> Rig) {
    let baseline = build(&format!("{label}-base"));
    assert_eq!(
        Spy::new(&baseline).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    assert!(
        points >= 5,
        "the marks and the two acts pass kill points: {points}"
    );
    let end = end_state(&baseline);
    let baseline_marks = marks(&baseline);
    assert_eq!(baseline_marks[1..], [true, true]);
    for k in 1..=points {
        let rig = build(&format!("{label}-{k}"));
        rig.kill.arm(k);
        assert!(
            killable(|| Spy::new(&rig).cutover(&rig)).is_none(),
            "kill point {k} of {points} is reached"
        );
        rig.kill.disarm();
        let done = Spy::new(&rig).cutover(&rig);
        assert_eq!(done.outcome, Outcome::Success, "killed at point {k}");
        assert_eq!(end_state(&rig), end, "killed at point {k}");
    }
}

#[test]
fn a_cutover_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    sweep("hook-cutover-sweep", |name| running(Rig::new(name)));
}

#[test]
fn a_cutover_run_twice_does_the_second_time_nothing() {
    let rig = running(Rig::new("hook-cutover-twice"));
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    let before = end_state(&rig);
    let second = Spy::new(&rig);
    let done = second.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(second.act_names(), Vec::<String>::new());
    assert_eq!(end_state(&rig), before);
    assert_eq!(line(&done)["reversible"], json!(false));
}

#[test]
fn a_cutover_killed_while_the_unit_stops_waits_for_it_on_the_rerun_and_does_not_stop_it_again() {
    let rig = running(Rig::new("hook-cutover-killed-in-stop"));
    let spy = Spy::new(&rig);
    *spy.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    spy.on_pause(|_, ticks| assert!(ticks < 3, "killed during the wait"));
    assert!(killable(|| spy.cutover(&rig)).is_none());
    assert_eq!(spy.act_names(), [stop()]);
    assert_eq!(unit_state(&rig), "deactivating stop-sigterm");
    assert_eq!(marks(&rig), [true, false, false]);
    let rerun = Spy::new(&rig);
    let base = rig.world.ticks();
    rerun.on_pause(move |spy, ticks| {
        if ticks == base + 2 {
            spy.world.seed_unit(UNIT, "inactive", "dead");
        }
    });
    assert_eq!(rerun.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), [start()], "no second stop");
    assert_eq!(unit_state(&rig), "active running");
}

#[test]
fn a_unit_that_is_slow_to_stop_is_waited_for_and_started_after_it_is_down() {
    let rig = running(Rig::new("hook-cutover-slow-stop"));
    let spy = Spy::new(&rig);
    *spy.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    let (link, new) = (rig.link.clone(), staged(&rig));
    spy.on_pause(move |spy, ticks| {
        // The link is on the staged release throughout: the updater moved it, the hook leaves it.
        assert_eq!(
            spy.world
                .target(&link)
                .unwrap()
                .as_deref()
                .map(|t| t.to_owned())
                .map(|t| super::fakes::resolve(&link, &t)),
            Some(new.clone())
        );
        if ticks == 4 {
            spy.world.seed_unit(UNIT, "failed", "failed");
        }
    });
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(rig.world.ticks(), 4);
    assert_eq!(spy.act_names(), [stop(), start()]);
}

#[test]
fn a_unit_that_does_not_stop_in_time_fails_the_cutover_as_an_environment_matter_before_it_starts() {
    let rig = running(Rig::new("hook-cutover-stuck-stop"));
    let spy = Spy::new(&rig);
    *spy.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!done.report.reversible);
    // The declared bound of cutover (4320 s) less the 120 s the start is allowed and a margin
    // of 60 s.
    assert_eq!(rig.world.ticks(), 414);
    assert_eq!(marks(&rig), [true, false, false]);
    assert_eq!(spy.act_names(), [stop()]);
}

#[test]
fn a_failed_stop_is_an_environment_failure_that_is_irreversible_and_a_rerun_goes_on() {
    let rig = running(Rig::new("hook-cutover-stop-fails"));
    let spy = Spy::new(&rig);
    spy.fail("stop");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(line(&done)["reversible"], json!(false), "the stop is begun");
    assert_eq!(marks(&rig), [true, false, false]);
    assert_eq!(spy.act_names(), [stop()]);
    spy.heal("stop");
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), [stop(), start()]);
}

#[test]
fn a_unit_manager_that_cannot_be_read_fails_the_cutover_before_anything_is_marked() {
    let rig = running(Rig::new("hook-cutover-show-fails"));
    let spy = Spy::new(&rig);
    spy.fail("show");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(line(&done)["reversible"], json!(true), "nothing was begun");
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(marks(&rig), [false, false, false]);
    assert_eq!(unit_state(&rig), "active running");
}

#[test]
fn a_link_that_cannot_be_read_fails_the_cutover_before_anything_is_marked() {
    let rig = running(Rig::new("hook-cutover-link-unreadable"));
    // A file where the link should be: the port cannot read it as a link.
    std::fs::remove_file(rig.tmp.path().join("control/link/current")).unwrap();
    std::fs::write(rig.tmp.path().join("control/link/current"), b"not a link").unwrap();
    let spy = Spy::new(&rig);
    let done = spy.cutover_as_is(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::LinkFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(line(&done)["reversible"], json!(true), "nothing was begun");
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(marks(&rig), [false, false, false]);
}

#[test]
fn a_start_that_fails_with_the_unit_never_run_is_an_environment_failure_and_a_rerun_starts_only() {
    let rig = running(Rig::new("hook-cutover-start-fails"));
    let spy = Spy::new(&rig);
    spy.fail("start");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(
        done.report.class,
        Class::Environment,
        "the unit never ran: it is not the release that failed"
    );
    assert_eq!(line(&done)["reversible"], json!(false));
    assert_eq!(marks(&rig), [true, true, true]);
    assert_eq!(spy.act_names(), [stop(), start()]);
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    spy.heal("start");
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), [start()]);
    assert_eq!(unit_state(&rig), "active running");
}

#[test]
fn a_start_that_errors_with_the_unit_failed_is_a_release_failure() {
    let rig = running(Rig::new("hook-cutover-start-errors-failed"));
    let spy = Spy::new(&rig);
    spy.fail("start");
    *spy.start_leaves.borrow_mut() = Some(("failed", "failed"));
    let done = spy.cutover(&rig);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(
        done.report.class,
        Class::Release,
        "the binary ran and failed"
    );
    assert!(!done.report.reversible);
}

#[test]
fn a_unit_that_starts_and_goes_inactive_is_an_environment_failure() {
    let rig = running(Rig::new("hook-cutover-start-inactive"));
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("inactive", "dead"));
    let done = spy.cutover(&rig);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
}

#[test]
fn a_unit_that_starts_and_fails_is_a_release_failure() {
    let rig = running(Rig::new("hook-cutover-start-then-fails"));
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("failed", "failed"));
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Release);
    assert!(!done.report.reversible);
    assert_eq!(rig.world.ticks(), 0, "a failed unit is not waited for");
}

#[test]
fn a_unit_that_waits_in_activating_start_is_given_120_s_and_then_fails_as_environment() {
    let rig = running(Rig::new("hook-cutover-start-stuck"));
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("activating", "start"));
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(
        done.report.class,
        Class::Environment,
        "waiting on a dependency is not a failure of the binary"
    );
    assert_eq!(rig.world.ticks(), 12);
    // A rerun does not start it a second time: it waits on the start that is under way.
    let rerun = Spy::new(&rig);
    let base = rig.world.ticks();
    rerun.on_pause(move |spy, ticks| {
        if ticks == base + 3 {
            spy.world.seed_unit(UNIT, "active", "running");
        }
    });
    assert_eq!(rerun.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), Vec::<String>::new());
}

#[test]
fn a_unit_that_crash_loops_in_auto_restart_is_a_release_failure_after_120_s() {
    let rig = running(Rig::new("hook-cutover-start-crash-loop"));
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("activating", "auto-restart"));
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Release, "the binary ran and died");
    assert!(!done.report.reversible);
    assert_eq!(rig.world.ticks(), 12);
}

#[test]
fn a_unit_seen_in_auto_restart_is_a_release_failure_even_if_it_ends_in_start() {
    let rig = running(Rig::new("hook-cutover-start-crash-seen"));
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("activating", "start"));
    spy.on_pause(|spy, ticks| match ticks {
        2 => spy.world.seed_unit(UNIT, "activating", "auto-restart"),
        3 => spy.world.seed_unit(UNIT, "activating", "start"),
        _ => {}
    });
    let done = spy.cutover(&rig);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Release);
}

/// The updater moves the release link to the staged release before it runs `cutover`. A link
/// that is anywhere else (not moved, moved to another release, absent) means the updater did not
/// do its part: the step fails before it stops anything, reversibly, and the hook moves nothing.
///
/// Catches a cutover that stops the agent with the link on the old release (the new agent would
/// then start from the old release), and one that moves the link itself.
#[test]
fn a_cutover_with_the_link_not_on_the_staged_release_fails_before_anything_is_stopped() {
    let cases = ["previous", "foreign", "absent"];
    for (n, what) in cases.into_iter().enumerate() {
        let rig = running(Rig::new(&format!("hook-cutover-link-off-{n}")));
        match what {
            "previous" => {}
            "foreign" => rig
                .world
                .set_link(&rig.link, &rig.tmp.path().join("releases/9.9.9")),
            _ => std::fs::remove_file(rig.tmp.path().join("control/link/current")).unwrap(),
        }
        let before = end_state(&rig);
        let spy = Spy::new(&rig);
        let done = spy.cutover_as_is(&rig);
        assert_eq!(done.outcome, Outcome::Failure, "{what}");
        assert_eq!(done.report.reason, Some(Reason::LinkFailed), "{what}");
        assert_eq!(done.report.class, Class::Environment, "{what}");
        assert!(done.report.reversible, "{what}: nothing was begun");
        let said = done.report.detail.as_deref().unwrap();
        assert!(
            said.contains("is not on the staged release"),
            "{what}: {said}"
        );
        assert!(said.contains("1.4.0"), "{what}: {said}");
        assert_eq!(spy.act_names(), Vec::<String>::new(), "{what}");
        assert_eq!(end_state(&rig), before, "{what}");
        assert!(!rig.record().unwrap().stop_begun, "{what}");
        assert_eq!(rig.world.ticks(), 0, "{what}");
    }
}

/// The updater writes the link relative to its directory (`current -> 1.4.0`), as hostkeeper
/// does; a target that resolves to the staged release is on it, however it is spelt: relative
/// with `..` and `.`, or absolute. One that resolves elsewhere is not.
///
/// Catches a comparison of the link's text with the staged path, which would refuse every real
/// cutover, and a resolution that reads `..` wrong.
#[test]
fn a_release_link_is_on_the_staged_release_by_where_it_points_however_it_is_spelt() {
    let rig = running(Rig::new("hook-cutover-link-spelt"));
    let link = rig.tmp.path().join("control/link/current");
    let relative = super::fakes::relative_to_link(&link, &rig.params.staged_release);
    let staged = rig.params.staged_release.clone();
    let spellings = [
        (relative.clone(), true),
        (std::path::Path::new(".").join(&relative), true),
        (relative.parent().unwrap().join("./1.4.0"), true),
        (staged.clone(), true),
        (relative.parent().unwrap().join("1.3.0"), false),
        (std::path::PathBuf::from("1.4.0"), false),
        (staged.join("deeper"), false),
    ];
    for (n, (target, on_staged)) in spellings.into_iter().enumerate() {
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let spy = Spy::new(&rig);
        let done = spy.cutover_as_is(&rig);
        if on_staged {
            assert_eq!(done.outcome, Outcome::Success, "{n}: {target:?}: {done:?}");
            // Back to where the rig started, for the next spelling: a fresh host.
            return_to_start(&rig);
        } else {
            assert_eq!(
                done.report.reason,
                Some(Reason::LinkFailed),
                "{n}: {target:?}"
            );
        }
    }
}

/// Puts the host back as `prepare` left it: the unit running, the stop and start marks off.
fn return_to_start(rig: &Rig) {
    let mut record = rig.record().unwrap();
    record.stop_begun = false;
    record.link_moved = false;
    record.start_issued = false;
    record.pre_stop_session_ms = None;
    rig.put_record(&record);
    rig.world.seed_unit(UNIT, "active", "running");
}

#[test]
fn a_cutover_with_no_record_has_no_prepare_behind_it_and_does_nothing() {
    let rig = Rig::new("hook-cutover-no-record");
    rig.world.seed_unit(UNIT, "active", "running");
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert!(done.report.reversible);
    assert_eq!(spy.log(), Vec::<String>::new());
    assert_eq!(rig.record_bytes(), None);
}

#[test]
fn a_record_that_breaks_a_rule_is_refused_and_nothing_is_done() {
    let rig = running(Rig::new("hook-cutover-invalid"));
    let mut record = rig.record().unwrap();
    record.update.target = "1.5.0".to_owned();
    rig.put_record(&record);
    rig.updater_links_staged();
    let before = end_state(&rig);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(spy.log(), Vec::<String>::new());
    assert_eq!(end_state(&rig), before);
}

#[test]
fn a_taint_or_an_owed_check_halts_the_cutover_before_anything_is_stopped() {
    let rig = running(Rig::new("hook-cutover-taint"));
    rig.taint();
    rig.updater_links_staged();
    let before = end_state(&rig);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Halt);
    assert_eq!(done.report.reason, Some(Reason::Tainted));
    assert!(done.report.reversible);
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(end_state(&rig), before);

    let rig = running(Rig::new("hook-cutover-owed"));
    rig.state_root().owe("run-7").unwrap();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Halt);
    assert_eq!(done.report.reason, Some(Reason::OwedCheck));
    assert_eq!(spy.act_names(), Vec::<String>::new());

    let rig = running(Rig::new("hook-cutover-owed-unreadable"));
    let spy = Spy::new(&rig);
    spy.fail("owed");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

#[test]
fn a_taint_that_appears_after_the_stop_began_does_not_halt_the_rerun() {
    let rig = running(Rig::new("hook-cutover-taint-late"));
    let spy = Spy::new(&rig);
    spy.fail("start");
    assert_eq!(spy.cutover(&rig).outcome, Outcome::Failure);
    rig.taint();
    let done = Spy::new(&rig).cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "committed: go forward");
}

/// A first install after its `prepare`.
fn first(name: &str) -> Rig {
    let rig = Rig::first_install(name);
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    rig.kill.disarm();
    rig
}

#[test]
fn a_first_install_has_no_cordon_no_host_and_no_stop_and_starts_on_the_link_the_updater_made() {
    let rig = first("hook-cutover-first");
    assert_eq!(link_target(&rig), None);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    // There is no unit to stop, and no stop is marked.
    assert_eq!(spy.act_names(), [start()]);
    assert!(spy.log().iter().all(|call| !call.contains("cordon")));
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    assert_eq!(unit_state(&rig), "active running");
    assert_eq!(marks(&rig), [false, true, true]);
    assert_eq!(line(&done)["reversible"], json!(false));
}

#[test]
fn a_first_install_is_reversible_until_it_marks_its_start_and_not_after() {
    // A taint and an unreadable link end before the first act.
    let rig = first("hook-cutover-first-taint");
    rig.taint();
    let done = Spy::new(&rig).cutover(&rig);
    assert_eq!(done.outcome, Outcome::Halt);
    assert_eq!(line(&done)["reversible"], json!(true));

    let rig = first("hook-cutover-first-link-unreadable");
    std::fs::create_dir_all(rig.tmp.path().join("control/link")).unwrap();
    std::fs::write(rig.tmp.path().join("control/link/current"), b"not a link").unwrap();
    let done = Spy::new(&rig).cutover_as_is(&rig);
    assert_eq!(done.report.reason, Some(Reason::LinkFailed));
    assert_eq!(line(&done)["reversible"], json!(true));
    assert_eq!(marks(&rig), [false, false, false]);

    // The mark of the start is the first act: irreversible from `link_moved` on.
    let rig = first("hook-cutover-first-start-fails");
    let spy = Spy::new(&rig);
    spy.fail("start");
    let done = spy.cutover(&rig);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(line(&done)["reversible"], json!(false));
}

#[test]
fn a_first_install_killed_at_any_point_and_run_again_still_finishes() {
    sweep("hook-cutover-sweep-first", first);
}

#[test]
fn cutover_records_the_session_the_host_had_before_the_stop_in_the_write_of_the_stop_mark() {
    let rig = running(Rig::new("hook-cutover-pre-stop"));
    let session = rig.host().session_started_ms;
    assert!(session.is_some());
    // Killed just before the stop itself: the stop mark is on disk, and the session with it.
    let baseline = running(Rig::new("hook-cutover-pre-stop-base"));
    let spy = Spy::new(&baseline);
    assert_eq!(spy.cutover(&baseline).outcome, Outcome::Success);
    let before_stop = spy.acts.borrow()[0].points;
    rig.kill.arm(before_stop);
    assert!(killable(|| Spy::new(&rig).cutover(&rig)).is_none());
    rig.kill.disarm();
    assert_eq!(marks(&rig), [true, false, false]);
    assert_eq!(pre_stop(&rig), session);
    assert_eq!(unit_state(&rig), "active running", "nothing is stopped yet");
    // A finished cutover keeps it.
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    assert_eq!(pre_stop(&rig), session);
}

#[test]
fn the_session_and_the_stop_mark_are_never_on_disk_apart_at_any_kill_point() {
    let baseline = running(Rig::new("hook-cutover-pre-stop-pairs-base"));
    assert_eq!(
        Spy::new(&baseline).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    for k in 1..=points {
        let rig = running(Rig::new(&format!("hook-cutover-pre-stop-pairs-{k}")));
        rig.kill.arm(k);
        assert!(killable(|| Spy::new(&rig).cutover(&rig)).is_none());
        rig.kill.disarm();
        let [stop_begun, ..] = marks(&rig);
        assert_eq!(
            pre_stop(&rig).is_some(),
            stop_begun,
            "killed at point {k} of {points}"
        );
    }
}

#[test]
fn a_cutover_rerun_after_the_stop_keeps_the_session_it_recorded_before_it() {
    let rig = running(Rig::new("hook-cutover-pre-stop-rerun"));
    let before = rig.host().session_started_ms;
    let baseline = running(Rig::new("hook-cutover-pre-stop-rerun-base"));
    let spy = Spy::new(&baseline);
    assert_eq!(spy.cutover(&baseline).outcome, Outcome::Success);
    // Killed just before the start: the unit is stopped, the marks are on disk.
    rig.kill.arm(spy.acts.borrow()[1].points);
    assert!(killable(|| Spy::new(&rig).cutover(&rig)).is_none());
    rig.kill.disarm();
    assert_eq!(marks(&rig), [true, true, true]);
    assert_eq!(unit_state(&rig), "inactive dead");
    assert_eq!(pre_stop(&rig), before);
    // The server now shows another session (the stopped agent's successor, say).
    rig.world
        .alter_host(|host| host.session_started_ms = Some(before.unwrap() + 777));
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    assert_eq!(
        pre_stop(&rig),
        before,
        "the reading after the stop is not recorded"
    );
}

#[test]
fn a_cutover_when_the_server_shows_no_session_records_none() {
    let rig = running(Rig::new("hook-cutover-pre-stop-none"));
    // The server restarted since the agent's hello: the host is awaiting, with no session.
    rig.world.forget_host();
    assert_eq!(rig.host().session_started_ms, None);
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    assert_eq!(marks(&rig), [true, true, true]);
    assert_eq!(pre_stop(&rig), None);
}

#[test]
fn a_first_install_records_no_session() {
    let rig = first("hook-cutover-pre-stop-first");
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    assert_eq!(pre_stop(&rig), None);
}
