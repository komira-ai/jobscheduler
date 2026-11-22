//! The Mac package's root helper job (`helper_unit`): `cutover` and `rollback` boot it out after
//! the agent is stopped and bootstrap it from the release tree about to run before the agent is
//! started. A host whose params name no helper is as it was; a systemd host that names one is
//! refused by every step.

use std::path::PathBuf;

use js_agent::update_hook::{Class, Lease, Outcome, Reason, Units};

use super::rollback_entries::verify_failed_on;
use super::step_rig::{MAC_HELPER, MAC_PF, MAC_UNIT, Rig, Spy, end_state, killable, running};

/// A Mac host with a helper, after `prepare`, the old helper running.
fn host(name: &str) -> Rig {
    let rig = running(Rig::mac_with_helper(name));
    rig.world.seed_unit(MAC_HELPER, "active", "running");
    rig
}

/// The helper's plist in `release`'s tree.
fn plist_of(release: PathBuf) -> String {
    release
        .join("tree/Library/LaunchDaemons/js-agent.session.plist")
        .display()
        .to_string()
}

fn bootstrap(release: PathBuf) -> String {
    format!("launchctl.bootstrap({})", plist_of(release))
}

fn stop(unit: &str) -> String {
    format!("units.stop({unit})")
}

fn start(unit: &str) -> String {
    format!("units.start({unit})")
}

fn state_of(rig: &Rig, unit: &str) -> String {
    let state = rig.world.show(unit).unwrap();
    format!("{} {}", state.active, state.sub)
}

#[test]
fn a_cutover_of_a_host_with_no_helper_touches_only_the_agent() {
    let rig = running(Rig::mac("hook-helper-absent"));
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [stop(MAC_UNIT), stop(MAC_PF), start(MAC_UNIT)]
    );
    assert!(spy.log().iter().all(|call| !call.contains("launchctl")));
}

#[test]
fn a_cutover_stops_the_agent_then_the_helper_and_starts_the_staged_helper_then_the_agent() {
    let rig = host("hook-helper-cutover");
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [
            stop(MAC_UNIT),
            stop(MAC_HELPER),
            bootstrap(rig.params.staged_release.clone()),
            start(MAC_HELPER),
            stop(MAC_PF),
            start(MAC_UNIT),
        ]
    );
    assert_eq!(state_of(&rig, MAC_HELPER), "active running");
    assert_eq!(state_of(&rig, MAC_UNIT), "active running");
}

#[test]
fn a_cutover_that_ran_before_does_not_touch_the_helper_again() {
    let rig = host("hook-helper-twice");
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    let second = Spy::new(&rig);
    assert_eq!(second.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(second.act_names(), Vec::<String>::new());
}

#[test]
fn a_first_install_starts_the_helper_from_the_staged_tree_before_the_agent() {
    let rig = Rig::mac_first_install_with_helper("hook-helper-first");
    assert_eq!(Spy::new(&rig).prepare(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [
            stop(MAC_HELPER),
            bootstrap(rig.params.staged_release.clone()),
            start(MAC_HELPER),
            stop(MAC_PF),
            start(MAC_UNIT),
        ]
    );
}

#[test]
fn a_helper_that_cannot_be_bootstrapped_fails_the_cutover_with_the_agent_not_started() {
    let rig = host("hook-helper-bootstrap-fails");
    let spy = Spy::new(&rig);
    spy.fail("bootstrap");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
    let detail = done.report.detail.unwrap();
    assert!(
        detail.contains("cannot bootstrap js-agent.session from")
            && detail.contains("bootstrap is down"),
        "{detail}"
    );
    assert!(!spy.act_names().contains(&start(MAC_UNIT)));
    assert_eq!(state_of(&rig, MAC_UNIT), "inactive dead");
    assert_eq!(state_of(&rig, MAC_HELPER), "inactive dead");
}

#[test]
fn a_helper_that_cannot_be_stopped_fails_the_cutover_before_anything_is_bootstrapped() {
    let rig = host("hook-helper-stop-fails");
    let spy = Spy::new(&rig);
    spy.fail("stop-helper");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.unwrap(),
        "cannot stop js-agent.session: stop-helper is down"
    );
    assert_eq!(spy.act_names(), [stop(MAC_UNIT), stop(MAC_HELPER)]);
}

#[test]
fn a_helper_that_does_not_start_fails_the_cutover_and_the_agent_is_not_started() {
    let rig = host("hook-helper-start-fails");
    let spy = Spy::new(&rig);
    spy.fail("start");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(
        spy.act_names().last().map(String::as_str),
        Some(start(MAC_HELPER).as_str())
    );
}

#[test]
fn a_rollback_after_a_helper_failed_to_bootstrap_restores_the_previous_helper_and_agent() {
    let rig = host("hook-helper-rollback-after-failure");
    let failed = Spy::new(&rig);
    failed.fail("bootstrap");
    assert_eq!(failed.cutover(&rig).outcome, Outcome::Failure);
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    // Nothing runs: the agent and the helper are both down, and each comes back from the
    // previous release's tree, the helper first.
    assert_eq!(
        spy.act_names(),
        [
            stop(MAC_HELPER),
            bootstrap(rig.params.previous_release.clone().unwrap()),
            start(MAC_HELPER),
            start(MAC_UNIT),
        ]
    );
    assert_eq!(state_of(&rig, MAC_HELPER), "active running");
    assert_eq!(state_of(&rig, MAC_UNIT), "active running");
}

#[test]
fn a_rollback_of_a_running_new_agent_stops_it_then_the_helper_and_restores_the_previous_ones() {
    let rig = verify_failed_on(host("hook-helper-rollback-running"));
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [
            stop(MAC_UNIT),
            stop(MAC_HELPER),
            bootstrap(rig.params.previous_release.clone().unwrap()),
            start(MAC_HELPER),
            start(MAC_UNIT),
        ]
    );
}

#[test]
fn a_rollback_whose_helper_fails_to_bootstrap_leaves_the_previous_agent_unstarted() {
    let rig = verify_failed_on(host("hook-helper-rollback-bootstrap-fails"));
    let spy = Spy::new(&rig);
    spy.fail("bootstrap");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert!(!spy.act_names().contains(&start(MAC_UNIT)));
}

/// The state a step leaves behind, with the helper's. A cordon a killed rollback could not lift
/// is left to lapse within one lease, so it is not part of the comparison.
fn ends(rig: &Rig) -> (serde_json::Value, String) {
    let mut end = end_state(rig);
    end["cordon"] = serde_json::Value::Null;
    (end, state_of(rig, MAC_HELPER))
}

#[test]
fn a_cutover_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one_with_the_helper() {
    let baseline = host("hook-helper-sweep-cutover-base");
    assert_eq!(
        Spy::new(&baseline).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    let end = ends(&baseline);
    for k in 1..=points {
        let rig = host(&format!("hook-helper-sweep-cutover-{k}"));
        rig.kill.arm(k);
        assert!(killable(|| Spy::new(&rig).cutover(&rig)).is_none(), "{k}");
        rig.kill.disarm();
        let done = Spy::new(&rig).cutover(&rig);
        assert_eq!(done.outcome, Outcome::Success, "killed at point {k}");
        assert_eq!(ends(&rig), end, "killed at point {k}");
    }
}

#[test]
fn a_rollback_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one_with_the_helper() {
    let build = |name: &str| verify_failed_on(host(name));
    let baseline = build("hook-helper-sweep-rollback-base");
    let before = baseline.kill.points();
    assert_eq!(
        Spy::new(&baseline).rollback(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points() - before;
    assert!(points > 0);
    let end = ends(&baseline);
    for k in 1..=points {
        let rig = build(&format!("hook-helper-sweep-rollback-{k}"));
        let session = rig.host().session_started_ms;
        rig.kill.arm(k);
        assert!(killable(|| Spy::new(&rig).rollback(&rig)).is_none(), "{k}");
        rig.kill.disarm();
        // The previous agent a start began says hello on its own, some time after.
        let rerun = Spy::new(&rig);
        rerun.on_pause(move |spy, _| {
            if spy.world.is_active(MAC_UNIT).unwrap()
                && spy.world.host().unwrap().session_started_ms == session
            {
                spy.world.hello("1.3.0").unwrap();
            }
        });
        let done = rerun.rollback(&rig);
        assert_eq!(
            done.outcome,
            Outcome::Success,
            "killed at point {k}: {done:?}"
        );
        assert_eq!(ends(&rig), end, "killed at point {k}");
    }
}

#[test]
fn every_step_refuses_a_helper_on_a_systemd_host_before_it_does_anything() {
    let mut rig = Rig::new("hook-helper-systemd");
    rig.params.settings.helper_unit = Some(MAC_HELPER.to_owned());
    let spy = Spy::new(&rig);
    let dones = [
        spy.prepare(&rig),
        spy.cutover(&rig),
        spy.verify(&rig),
        spy.rollback(&rig),
        spy.cleanup(&rig),
    ];
    for done in dones {
        assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
        assert_eq!(done.report.reason, Some(Reason::BadParams));
        assert_eq!(
            done.report.detail.as_deref(),
            Some(
                "helper_unit \"js-agent.session\" is set, and only a launchd host has a helper job"
            )
        );
    }
    assert_eq!(spy.log(), Vec::<String>::new());
    assert!(rig.record().is_none());
}
