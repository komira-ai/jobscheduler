//! The root helper job of the releases before the one-program package (`js-agent.session`):
//! every launchd `cutover` boots it out and removes its plist before the pf step and the agent's
//! start; a launchd `rollback` to a release whose tree carries the plist puts it back, starts
//! the job and waits for it before the previous agent starts; a host whose params still name
//! `helper_unit` and `socket_group` is read as one that does not.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use js_agent::update_hook::{Class, Lease, Outcome, Params, Reason, Units};

use super::rollback_entries::verify_failed_on;
use super::step_rig::{
    MAC_HELPER, MAC_PF, MAC_UNIT, PLIST_TEXT, Rig, Spy, end_state, killable, running,
};

/// What the previous release's tree carries as the helper's plist, other than the text of the
/// plist a host has.
const SHIPPED: &str =
    "<plist><dict><key>Label</key><string>js-agent.session</string></dict></plist>\n";

fn stop(unit: &str) -> String {
    format!("units.stop({unit})")
}

fn start(unit: &str) -> String {
    format!("units.start({unit})")
}

/// The bootstrap of the helper's plist in `release`'s tree.
fn bootstrap(release: PathBuf) -> String {
    format!("launchctl.bootstrap({})", shipped_in(release).display())
}

fn shipped_in(release: PathBuf) -> PathBuf {
    release.join("tree/Library/LaunchDaemons/js-agent.session.plist")
}

fn state_of(rig: &Rig, unit: &str) -> String {
    let state = rig.world.show(unit).unwrap();
    format!("{} {}", state.active, state.sub)
}

/// Puts `text` where the machine names the helper's plist, with the directory above it.
fn lay(rig: &Rig, text: &str) {
    let path = rig.helper_plist();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A Mac host after `prepare` with the old helper running and its plist laid, as an earlier
/// package left it.
fn host(rig: Rig) -> Rig {
    let rig = running(rig);
    rig.world.seed_unit(MAC_HELPER, "active", "running");
    lay(&rig, PLIST_TEXT);
    rig
}

/// [`host`] of an update whose previous release shipped the helper ([`SHIPPED`] is its plist).
fn host_that_shipped_a_helper(name: &str) -> Rig {
    let rig = Rig::mac_with_previous_helper(name);
    fs::write(
        shipped_in(rig.params.previous_release.clone().unwrap()),
        SHIPPED,
    )
    .unwrap();
    host(rig)
}

/// Whether the helper's plist was there at each act, by the act's name.
fn plist_at(spy: &Spy<'_>, act: &str) -> bool {
    let at = spy.act_names().iter().position(|name| name == act);
    spy.plist_at_acts.borrow()[at.unwrap_or_else(|| panic!("no act {act}"))]
}

#[test]
fn a_cutover_boots_the_helper_out_and_removes_its_plist_before_the_agent_starts() {
    let rig = host(Rig::mac_with_tree("hook-retired-cutover"));
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [stop(MAC_UNIT), stop(MAC_HELPER), start(MAC_UNIT)]
    );
    assert!(plist_at(&spy, &stop(MAC_HELPER)));
    assert!(!plist_at(&spy, &start(MAC_UNIT)));
    assert!(!rig.helper_plist().exists());
    assert_eq!(state_of(&rig, MAC_HELPER), "inactive dead");
    assert_eq!(state_of(&rig, MAC_UNIT), "active running");
}

#[test]
fn a_cutover_retires_the_helper_before_the_pf_step() {
    let rig = host(Rig::mac_with_pf("hook-retired-before-pf"));
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let acts = spy.act_names();
    let at = |act: String| acts.iter().position(|name| *name == act).unwrap();
    assert!(at(stop(MAC_UNIT)) < at(stop(MAC_HELPER)));
    assert!(at(stop(MAC_HELPER)) < at(stop(MAC_PF)));
    assert!(at(stop(MAC_PF)) < at(start(MAC_UNIT)));
    assert!(plist_at(&spy, &stop(MAC_HELPER)));
    assert!(!plist_at(&spy, &stop(MAC_PF)));
}

#[test]
fn a_first_install_retires_the_helper_of_an_earlier_package_too() {
    let rig = Rig::mac_first_install_with_tree("hook-retired-first");
    assert_eq!(Spy::new(&rig).prepare(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    lay(&rig, PLIST_TEXT);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), [stop(MAC_HELPER), start(MAC_UNIT)]);
    assert!(!rig.helper_plist().exists());
}

#[test]
fn a_cutover_on_a_host_with_no_plist_and_no_loaded_helper_succeeds() {
    let rig = running(Rig::mac_with_tree("hook-retired-absent"));
    assert!(!rig.helper_plist().exists());
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [stop(MAC_UNIT), stop(MAC_HELPER), start(MAC_UNIT)]
    );
    assert!(!rig.helper_plist().exists());
}

#[test]
fn a_cutover_that_ran_before_does_not_retire_the_helper_again() {
    let rig = host(Rig::mac_with_tree("hook-retired-twice"));
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    lay(&rig, PLIST_TEXT);
    let second = Spy::new(&rig);
    assert_eq!(second.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(second.act_names(), Vec::<String>::new());
    assert!(rig.helper_plist().exists());
}

/// A plist that cannot be removed (here a directory with a file in it) fails the cutover with the
/// agent not started, and a rerun once it is gone succeeds.
#[test]
fn a_plist_that_cannot_be_removed_fails_the_cutover_with_the_agent_not_started() {
    let rig = running(Rig::mac_with_tree("hook-retired-remove-fails"));
    let path = rig.helper_plist();
    fs::create_dir_all(path.join("inside")).unwrap();
    let why = fs::remove_file(&path).unwrap_err();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.unwrap(),
        format!("cannot remove {}: {why}", path.display())
    );
    assert_eq!(
        spy.act_names(),
        [stop(MAC_UNIT), stop(MAC_HELPER)],
        "the agent is not started"
    );
    fs::remove_dir_all(&path).unwrap();
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names().last(), Some(&start(MAC_UNIT)));
}

#[test]
fn a_helper_that_cannot_be_stopped_fails_the_cutover_before_anything_else_is_done() {
    let rig = host(Rig::mac_with_pf("hook-retired-stop-fails"));
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
    assert!(rig.helper_plist().exists());
}

#[test]
fn a_rollback_to_a_release_that_shipped_the_helper_puts_its_plist_back_and_starts_it_first() {
    let rig = verify_failed_on(host_that_shipped_a_helper("hook-retired-restore"));
    assert!(!rig.helper_plist().exists());
    let previous = rig.params.previous_release.clone().unwrap();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [
            stop(MAC_UNIT),
            stop(MAC_HELPER),
            bootstrap(previous),
            start(MAC_HELPER),
            start(MAC_UNIT),
        ]
    );
    assert!(!plist_at(&spy, &stop(MAC_HELPER)));
    assert!(plist_at(&spy, &start(MAC_HELPER)));
    assert_eq!(fs::read_to_string(rig.helper_plist()).unwrap(), SHIPPED);
    let mode = fs::metadata(rig.helper_plist())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o644);
    assert!(!rig.helper_plist().with_extension("plist.new").exists());
    assert_eq!(state_of(&rig, MAC_HELPER), "active running");
    assert_eq!(state_of(&rig, MAC_UNIT), "active running");
}

#[test]
fn a_rollback_leaves_a_plist_that_is_there_as_it_is() {
    let rig = verify_failed_on(host_that_shipped_a_helper("hook-retired-restore-present"));
    lay(&rig, PLIST_TEXT);
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(spy.act_names().contains(&start(MAC_HELPER)));
    assert_eq!(fs::read_to_string(rig.helper_plist()).unwrap(), PLIST_TEXT);
}

#[test]
fn a_rollback_to_a_release_that_shipped_no_helper_touches_no_helper() {
    let rig = verify_failed_on(host(Rig::mac_with_tree("hook-retired-restore-none")));
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), [stop(MAC_UNIT), start(MAC_UNIT)]);
    assert!(!rig.helper_plist().exists());
    assert_eq!(state_of(&rig, MAC_HELPER), "inactive dead");
}

#[test]
fn a_helper_plist_that_cannot_be_put_back_fails_the_rollback_with_the_previous_agent_not_started() {
    let rig = verify_failed_on(host_that_shipped_a_helper("hook-retired-restore-no-dir"));
    let path = rig.helper_plist();
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
    let why = fs::write(path.with_extension("plist.new"), "").unwrap_err();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.unwrap(),
        format!("cannot put {} back: {why}", path.display())
    );
    assert_eq!(spy.act_names(), [stop(MAC_UNIT), stop(MAC_HELPER)]);
}

#[test]
fn a_restored_helper_that_cannot_be_bootstrapped_fails_the_rollback_with_the_previous_agent_not_started()
 {
    let rig = verify_failed_on(host_that_shipped_a_helper("hook-retired-restore-bootstrap"));
    let previous = rig.params.previous_release.clone().unwrap();
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
            shipped_in(previous).display()
        )
    );
    assert!(!spy.act_names().contains(&start(MAC_UNIT)));
}

#[test]
fn a_restored_helper_that_does_not_start_fails_the_rollback_with_the_previous_agent_not_started() {
    let rig = verify_failed_on(host_that_shipped_a_helper("hook-retired-restore-start"));
    let spy = Spy::new(&rig);
    spy.fail("start");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(spy.act_names().last(), Some(&start(MAC_HELPER)));
}

#[test]
fn a_restored_helper_that_cannot_be_stopped_fails_the_rollback_before_its_plist_is_put_back() {
    let rig = verify_failed_on(host_that_shipped_a_helper("hook-retired-restore-stop"));
    let spy = Spy::new(&rig);
    spy.fail("stop-helper");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(
        done.report.detail.unwrap(),
        "cannot stop js-agent.session: stop-helper is down"
    );
    assert_eq!(spy.act_names(), [stop(MAC_UNIT), stop(MAC_HELPER)]);
    assert!(!rig.helper_plist().exists());
}

/// The state a step leaves behind, with the helper's: its job, its plist and what is beside it.
/// A cordon a killed step could not lift is left to lapse within one lease, so it is not part of
/// the comparison.
fn ends(rig: &Rig) -> (serde_json::Value, String, Option<String>, bool) {
    let mut end = end_state(rig);
    end["cordon"] = serde_json::Value::Null;
    (
        end,
        state_of(rig, MAC_HELPER),
        fs::read_to_string(rig.helper_plist()).ok(),
        rig.helper_plist().with_extension("plist.new").exists(),
    )
}

#[test]
fn a_cutover_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one_with_the_helper_retired()
 {
    let baseline = host(Rig::mac_with_pf("hook-retired-sweep-cutover-base"));
    assert_eq!(
        Spy::new(&baseline).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    let end = ends(&baseline);
    assert_eq!(end.2, None);
    for k in 1..=points {
        let rig = host(Rig::mac_with_pf(&format!("hook-retired-sweep-cutover-{k}")));
        rig.kill.arm(k);
        assert!(killable(|| Spy::new(&rig).cutover(&rig)).is_none(), "{k}");
        rig.kill.disarm();
        let done = Spy::new(&rig).cutover(&rig);
        assert_eq!(done.outcome, Outcome::Success, "killed at point {k}");
        assert_eq!(ends(&rig), end, "killed at point {k}");
    }
}

#[test]
fn a_rollback_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one_with_the_helper_restored()
 {
    let build = |name: &str| verify_failed_on(host_that_shipped_a_helper(name));
    let baseline = build("hook-retired-sweep-rollback-base");
    let before = baseline.kill.points();
    assert_eq!(
        Spy::new(&baseline).rollback(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points() - before;
    assert!(points > 0);
    let end = ends(&baseline);
    assert_eq!(end.2.as_deref(), Some(SHIPPED));
    for k in 1..=points {
        let rig = build(&format!("hook-retired-sweep-rollback-{k}"));
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

/// A params table of an older manifest, which names the helper job and its socket group.
fn older_params(rig: &Rig) -> Params {
    let text = format!(
        "{}helper_unit = \"{MAC_HELPER}\"\nsocket_group = \"_jsagent\"\n",
        rig.params_text
    );
    Params::parse(&text).unwrap()
}

#[test]
fn a_systemd_host_whose_params_still_name_helper_unit_and_socket_group_updates_as_before() {
    let mut rig = Rig::new("hook-retired-systemd-older-params");
    rig.params = older_params(&rig);
    let rig = running(rig);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [stop(&rig.unit), start(&rig.unit)],
        "a systemd host retires no helper"
    );
    let verified = spy.verify(&rig);
    assert_eq!(verified.outcome, Outcome::Success, "{verified:?}");
}

#[test]
fn a_mac_whose_params_still_name_helper_unit_and_socket_group_ignores_both() {
    let mut rig = Rig::mac_with_tree("hook-retired-mac-older-params");
    rig.params = older_params(&rig);
    let rig = host(rig);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [stop(MAC_UNIT), stop(MAC_HELPER), start(MAC_UNIT)]
    );
}

#[test]
fn no_pf_job_is_touched_by_a_cutover_of_a_host_that_names_none() {
    let rig = host(Rig::mac_with_tree("hook-retired-no-pf"));
    let spy = Spy::new(&rig);
    assert_eq!(spy.cutover(&rig).outcome, Outcome::Success);
    assert!(spy.act_names().iter().all(|act| !act.contains(MAC_PF)));
}
