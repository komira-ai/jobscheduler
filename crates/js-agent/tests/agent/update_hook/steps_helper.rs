//! The root helper job that releases before the one-program agent carried (`js-agent.session`):
//! `cutover` boots it out if launchd has it loaded and removes its plist from the launch daemons
//! directory before the agent is started, on every launchd host and on a first install; a
//! `rollback` to a release whose tree carries the helper's plist bootstraps and starts the
//! helper from that tree before the agent, and one to a release without it starts only the
//! agent; a systemd host never does either; a params table of an earlier release that still
//! names `helper_unit` and `socket_group` runs as one that does not.

use std::fs;
use std::path::PathBuf;

use js_agent::own_host::Lease as _;
use js_agent::update_hook::{Class, Outcome, Params, Reason, Units};

use super::rollback_entries::verify_failed_on;
use super::step_rig::{MAC_RETIRED, MAC_UNIT, Rig, Spy, end_state, killable, running};

/// A Mac host that ran a release with the helper, after `prepare`: the helper job is loaded and
/// running, its plist is in the launch daemons directory, and the previous release's tree
/// carries the plist.
fn host(name: &str) -> Rig {
    let rig = running(Rig::mac_from_two_daemon_release(name));
    rig.world.seed_unit(MAC_RETIRED, "active", "running");
    rig.leave_retired_plist();
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

/// Asserts that no `launchctl` command was run: nothing was bootstrapped.
#[track_caller]
fn assert_no_bootstrap(spy: &Spy<'_>) {
    assert!(
        spy.log().iter().all(|call| !call.contains("launchctl")),
        "{:?}",
        spy.log()
    );
}

/// Asserts that a cutover on a host that ran the helper boots the job out and removes its plist
/// before it starts the agent, bootstraps nothing, and leaves the helper down.
#[test]
fn a_cutover_boots_out_the_retired_helper_and_removes_its_plist_before_the_agent_starts() {
    let rig = host("hook-retire-cutover");
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [stop(MAC_UNIT), stop(MAC_RETIRED), start(MAC_UNIT)]
    );
    assert_no_bootstrap(&spy);
    assert_eq!(state_of(&rig, MAC_RETIRED), "inactive dead");
    assert!(!rig.retired_plist().exists());
    assert_eq!(state_of(&rig, MAC_UNIT), "active running");
}

/// Asserts that a cutover on a Mac that has no helper job loaded and no plist still asks for the
/// job to be booted out (a no-op for a job that is not loaded) and starts the agent.
#[test]
fn a_cutover_on_a_host_with_no_helper_job_or_plist_starts_the_agent() {
    let rig = running(Rig::mac_package("hook-retire-absent"));
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [stop(MAC_UNIT), stop(MAC_RETIRED), start(MAC_UNIT)]
    );
    assert_no_bootstrap(&spy);
    assert!(!rig.retired_plist().exists());
    assert_eq!(state_of(&rig, MAC_UNIT), "active running");
}

/// Asserts that a cutover that ran before does not touch the helper again.
#[test]
fn a_cutover_that_ran_before_does_not_touch_the_helper_again() {
    let rig = host("hook-retire-twice");
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    let second = Spy::new(&rig);
    assert_eq!(second.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(second.act_names(), Vec::<String>::new());
}

/// Asserts that a first install retires a helper job an earlier install of the agent left, and
/// then starts only the agent: nothing is bootstrapped.
#[test]
fn a_first_install_retires_a_leftover_helper_job_and_starts_only_the_agent() {
    let rig = Rig::mac_first_install_package("hook-retire-first");
    rig.world.seed_unit(MAC_RETIRED, "active", "running");
    rig.leave_retired_plist();
    assert_eq!(Spy::new(&rig).prepare(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), [stop(MAC_RETIRED), start(MAC_UNIT)]);
    assert_no_bootstrap(&spy);
    assert_eq!(state_of(&rig, MAC_RETIRED), "inactive dead");
    assert!(!rig.retired_plist().exists());
}

/// Asserts that a helper job launchd cannot boot out fails the cutover (environment, after the
/// commit) with the plist kept and the agent not started.
#[test]
fn a_helper_that_cannot_be_booted_out_fails_the_cutover_with_the_agent_not_started() {
    let rig = host("hook-retire-stop-fails");
    let spy = Spy::new(&rig);
    spy.fail("stop-retired");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!done.report.reversible);
    assert_eq!(
        done.report.detail.unwrap(),
        "cannot stop js-agent.session: stop-retired is down"
    );
    assert_eq!(spy.act_names(), [stop(MAC_UNIT), stop(MAC_RETIRED)]);
    assert!(rig.retired_plist().exists());
    assert_eq!(state_of(&rig, MAC_UNIT), "inactive dead");
}

/// Asserts that a plist that cannot be removed (a directory with an entry where the file should
/// be) fails the cutover naming the path and the error, after the job was booted out and before
/// the agent is started.
#[test]
fn a_plist_that_cannot_be_removed_fails_the_cutover_with_the_agent_not_started() {
    let rig = running(Rig::mac_package("hook-retire-remove-fails"));
    let plist = rig.retired_plist();
    fs::create_dir_all(plist.join("entry")).unwrap();
    let why = fs::remove_file(&plist).unwrap_err();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!done.report.reversible);
    assert_eq!(
        done.report.detail.unwrap(),
        format!("cannot remove {}: {why}", plist.display())
    );
    assert_eq!(spy.act_names(), [stop(MAC_UNIT), stop(MAC_RETIRED)]);
    assert_eq!(state_of(&rig, MAC_UNIT), "inactive dead");
}

/// Asserts that a cutover that failed to remove the plist, run again once it can be, removes it
/// and starts the agent, the agent already stopped.
#[test]
fn a_cutover_run_again_after_the_plist_could_not_be_removed_removes_it_and_starts_the_agent() {
    let rig = running(Rig::mac_package("hook-retire-remove-again"));
    let plist = rig.retired_plist();
    fs::create_dir_all(plist.join("entry")).unwrap();
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Failure);
    rig.kill.disarm();
    fs::remove_dir_all(&plist).unwrap();
    rig.leave_retired_plist();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), [stop(MAC_RETIRED), start(MAC_UNIT)]);
    assert!(!rig.retired_plist().exists());
    assert_eq!(state_of(&rig, MAC_UNIT), "active running");
}

/// Asserts that a rollback to a release whose tree carries the helper's plist boots the helper
/// out, bootstraps the plist from that tree and starts it, in that order, before the previous
/// agent starts.
#[test]
fn a_rollback_to_a_release_with_the_helper_bootstraps_it_from_the_previous_tree() {
    let rig = verify_failed_on(host("hook-retire-rollback-with"));
    assert_eq!(state_of(&rig, MAC_RETIRED), "inactive dead");
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [
            stop(MAC_UNIT),
            stop(MAC_RETIRED),
            bootstrap(rig.params.previous_release.clone().unwrap()),
            start(MAC_RETIRED),
            start(MAC_UNIT),
        ]
    );
    assert_eq!(state_of(&rig, MAC_RETIRED), "active running");
    assert_eq!(state_of(&rig, MAC_UNIT), "active running");
}

/// Asserts that a rollback to a release whose tree carries no helper plist starts only the
/// agent: the helper is not asked about and nothing is bootstrapped.
#[test]
fn a_rollback_to_a_release_without_the_helper_starts_only_the_agent() {
    let rig = verify_failed_on(running(Rig::mac_package("hook-retire-rollback-without")));
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), [stop(MAC_UNIT), start(MAC_UNIT)]);
    assert_no_bootstrap(&spy);
    assert_eq!(state_of(&rig, MAC_RETIRED), "inactive dead");
}

/// Asserts that a rollback after a cutover that could not boot the helper out finds the agent
/// down and brings the helper and then the agent back from the previous tree.
#[test]
fn a_rollback_after_a_cutover_that_failed_on_the_helper_restores_the_previous_helper_and_agent() {
    let rig = host("hook-retire-rollback-after-failure");
    let failed = Spy::new(&rig);
    failed.fail("stop-retired");
    assert_eq!(failed.cutover(&rig).outcome, Outcome::Failure);
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [
            stop(MAC_RETIRED),
            bootstrap(rig.params.previous_release.clone().unwrap()),
            start(MAC_RETIRED),
            start(MAC_UNIT),
        ]
    );
    assert_eq!(state_of(&rig, MAC_RETIRED), "active running");
    assert_eq!(state_of(&rig, MAC_UNIT), "active running");
}

/// Asserts that a rollback whose helper cannot be booted out fails (environment) with the
/// previous agent not started.
#[test]
fn a_rollback_whose_helper_cannot_be_booted_out_leaves_the_previous_agent_unstarted() {
    let rig = verify_failed_on(host("hook-retire-rollback-stop-fails"));
    let spy = Spy::new(&rig);
    spy.fail("stop-retired");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.unwrap(),
        "cannot stop js-agent.session: stop-retired is down"
    );
    assert_eq!(spy.act_names(), [stop(MAC_UNIT), stop(MAC_RETIRED)]);
}

/// Asserts that a rollback whose helper plist cannot be bootstrapped fails naming the plist,
/// with the previous agent not started.
#[test]
fn a_rollback_whose_helper_fails_to_bootstrap_leaves_the_previous_agent_unstarted() {
    let rig = verify_failed_on(host("hook-retire-rollback-bootstrap-fails"));
    let spy = Spy::new(&rig);
    spy.fail("bootstrap");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.unwrap(),
        format!(
            "cannot bootstrap js-agent.session from {}: bootstrap is down",
            plist_of(rig.params.previous_release.clone().unwrap())
        )
    );
    assert!(!spy.act_names().contains(&start(MAC_UNIT)));
}

/// Asserts that a rollback whose helper does not start fails with the previous agent not
/// started.
#[test]
fn a_rollback_whose_helper_does_not_start_leaves_the_previous_agent_unstarted() {
    let rig = verify_failed_on(host("hook-retire-rollback-start-fails"));
    let spy = Spy::new(&rig);
    spy.fail("start");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(
        spy.act_names().last().map(String::as_str),
        Some(start(MAC_RETIRED).as_str())
    );
}

/// The state a step leaves behind, with the helper's and whether its plist is there. A cordon a
/// killed rollback could not lift is left to lapse within one lease, so it is not part of the
/// comparison.
fn ends(rig: &Rig) -> (serde_json::Value, String, bool) {
    let mut end = end_state(rig);
    end["cordon"] = serde_json::Value::Null;
    (
        end,
        state_of(rig, MAC_RETIRED),
        rig.retired_plist().exists(),
    )
}

/// Asserts that a cutover killed at any point and run again ends as an uninterrupted one: the
/// agent running, the helper down, its plist gone.
#[test]
fn a_cutover_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one_with_the_helper() {
    let baseline = host("hook-retire-sweep-cutover-base");
    assert_eq!(
        Spy::new(&baseline).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    let end = ends(&baseline);
    assert!(!end.2);
    for k in 1..=points {
        let rig = host(&format!("hook-retire-sweep-cutover-{k}"));
        rig.kill.arm(k);
        assert!(killable(|| Spy::new(&rig).cutover(&rig)).is_none(), "{k}");
        rig.kill.disarm();
        let done = Spy::new(&rig).cutover(&rig);
        assert_eq!(done.outcome, Outcome::Success, "killed at point {k}");
        assert_eq!(ends(&rig), end, "killed at point {k}");
    }
}

/// Asserts that a rollback to a release with the helper, killed at any point and run again, ends
/// as an uninterrupted one: the helper and the agent running.
#[test]
fn a_rollback_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one_with_the_helper() {
    let build = |name: &str| verify_failed_on(host(name));
    let baseline = build("hook-retire-sweep-rollback-base");
    let before = baseline.kill.points();
    assert_eq!(
        Spy::new(&baseline).rollback(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points() - before;
    assert!(points > 0);
    let end = ends(&baseline);
    assert_eq!(end.1, "active running");
    for k in 1..=points {
        let rig = build(&format!("hook-retire-sweep-rollback-{k}"));
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

/// Asserts that a params table of an earlier release that still names `helper_unit` and
/// `socket_group` runs a Mac's cutover as one that does not: the same acts, the retired helper
/// booted out and its plist removed.
#[test]
fn a_params_table_that_still_names_the_helper_and_its_socket_group_runs_as_one_that_does_not() {
    let mut rig = host("hook-retire-old-params");
    rig.params = Params::parse(&format!(
        "{}helper_unit = \"js-agent.session\"\nsocket_group = \"_jsagent\"\n",
        rig.params_text
    ))
    .unwrap();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [stop(MAC_UNIT), stop(MAC_RETIRED), start(MAC_UNIT)]
    );
    assert!(!rig.retired_plist().exists());
}

/// Asserts that a systemd host's cutover and rollback never ask about the helper job and leave a
/// plist at the retired path alone, even for a table that names the helper.
#[test]
fn a_systemd_host_never_touches_the_retired_helper() {
    let mut rig = running(Rig::new("hook-retire-systemd-cutover"));
    rig.params = Params::parse(&format!(
        "{}helper_unit = \"js-agent.session\"\n",
        rig.params_text
    ))
    .unwrap();
    rig.leave_retired_plist();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(spy.log().iter().all(|call| !call.contains(MAC_RETIRED)));
    assert!(rig.retired_plist().exists());

    let rig = verify_failed_on(running(Rig::new("hook-retire-systemd-rollback")));
    rig.leave_retired_plist();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(
        spy.log()
            .iter()
            .all(|call| !call.contains(MAC_RETIRED) && !call.contains("launchctl"))
    );
    assert!(rig.retired_plist().exists());
}
