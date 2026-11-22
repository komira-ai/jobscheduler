//! The pf job an earlier release of the Mac package ran (`js-agent.pf-pool`): `cutover` boots it
//! out and removes its plist after the helper and before the agent starts; `rollback` to a
//! release whose tree carries that plist boots it out, loads the anchor with that release's
//! `js-mac-provision`, puts the plist back and bootstraps the job, after the helper and before the
//! agent starts. A previous release without the plist gets none of it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use js_agent::update_hook::{Class, Lease, Outcome, Reason, Units};

use super::rollback_entries::verify_failed_on;
use super::step_rig::{
    MAC_HELPER, MAC_PF, MAC_UNIT, PLIST_TEXT, Rig, Spy, end_state, killable, running,
};

/// A Mac host updated from a release of two daemons and a pf job, after `prepare`, the old helper
/// running and the old pf plist installed.
fn host(name: &str) -> Rig {
    let rig = running(Rig::mac_with_helper_from_pf_release(name));
    rig.world.seed_unit(MAC_HELPER, "active", "running");
    install_plist(&rig);
    rig
}

/// Where the host keeps the retired job's plist.
fn pf_plist(rig: &Rig) -> PathBuf {
    Spy::new(rig).pf_plist
}

/// Puts a plist at the host's path of the retired job, as the earlier release's package left it.
fn install_plist(rig: &Rig) {
    let path = pf_plist(rig);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, PLIST_TEXT).unwrap();
}

fn stop(unit: &str) -> String {
    format!("units.stop({unit})")
}

fn start(unit: &str) -> String {
    format!("units.start({unit})")
}

/// The bootstrap of `label`'s plist in `release`'s tree.
fn bootstrap(release: PathBuf, label: &str) -> String {
    format!(
        "launchctl.bootstrap({})",
        tree_plist(release, label).display()
    )
}

fn tree_plist(release: PathBuf, label: &str) -> PathBuf {
    release.join(format!("tree/Library/LaunchDaemons/{label}.plist"))
}

/// The previous release's `js-mac-provision` loading the anchor of the host's flags file.
fn install(rig: &Rig) -> String {
    format!(
        "pf.install --flags-file {} {}",
        rig.params.flags_file.display(),
        program(rig)
    )
}

fn program(rig: &Rig) -> String {
    rig.params
        .previous_release
        .clone()
        .unwrap()
        .join("tree/Library/Application Support/js-agent/bin/js-mac-provision")
        .display()
        .to_string()
}

/// What `rollback` does to the pf job, in order.
fn restore_acts(rig: &Rig) -> [String; 3] {
    [
        stop(MAC_PF),
        install(rig),
        bootstrap(rig.params.previous_release.clone().unwrap(), MAC_PF),
    ]
}

/// The helper's acts of a cutover, then the pf job's, then the agent's start.
fn cutover_acts(rig: &Rig) -> Vec<String> {
    vec![
        stop(MAC_UNIT),
        stop(MAC_HELPER),
        bootstrap(rig.params.staged_release.clone(), MAC_HELPER),
        start(MAC_HELPER),
        stop(MAC_PF),
        start(MAC_UNIT),
    ]
}

/// Asserts that a cutover boots the retired job out and removes its plist, after the helper is
/// replaced and before the agent starts. Catches a job left to load the anchor beside the agent,
/// and a plist left for the next boot to load again.
#[test]
fn a_cutover_boots_the_retired_pf_job_out_and_removes_its_plist_after_the_helper_before_the_agent()
{
    let rig = host("hook-retired-cutover");
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), cutover_acts(&rig));
    assert!(!pf_plist(&rig).exists());
}

/// Asserts that a cutover of a host with no plist only boots the job out. Catches a removal that
/// fails on a file that is not there.
#[test]
fn a_cutover_with_no_pf_plist_only_boots_the_job_out() {
    let rig = host("hook-retired-no-plist");
    fs::remove_file(pf_plist(&rig)).unwrap();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), cutover_acts(&rig));
    assert!(!pf_plist(&rig).exists());
}

/// Asserts that a first install boots the job out (launchd has none loaded, so it is a no-op) and
/// removes a plist that is there, before the agent starts. Catches a retirement that is left to
/// updates alone.
#[test]
fn a_first_install_retires_the_pf_job_before_the_agent_starts() {
    let rig = Rig::mac_first_install_with_helper("hook-retired-first");
    assert_eq!(Spy::new(&rig).prepare(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    install_plist(&rig);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [
            stop(MAC_HELPER),
            bootstrap(rig.params.staged_release.clone(), MAC_HELPER),
            start(MAC_HELPER),
            stop(MAC_PF),
            start(MAC_UNIT),
        ]
    );
    assert!(!pf_plist(&rig).exists());
}

/// Asserts that a cutover run again after the agent's start was issued makes no act, the pf job's
/// included. Catches a rerun that boots the job out of a host that has moved on.
#[test]
fn a_cutover_that_ran_before_does_not_touch_the_pf_job_again() {
    let rig = host("hook-retired-twice");
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    let second = Spy::new(&rig);
    assert_eq!(second.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(second.act_names(), Vec::<String>::new());
}

/// Asserts that a bootout that fails fails the cutover with its detail, class environment, before
/// the plist is removed and the agent is started, and that a rerun completes. Catches an agent
/// started beside a job that is still loaded.
#[test]
fn a_pf_job_that_cannot_be_booted_out_fails_the_cutover_before_the_agent_starts() {
    let rig = host("hook-retired-stop-fails");
    let spy = Spy::new(&rig);
    spy.fail("stop-pf");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("cannot stop js-agent.pf-pool: stop-pf is down")
    );
    assert_eq!(spy.act_names().last(), Some(&stop(MAC_PF)));
    assert!(pf_plist(&rig).exists());
    rig.kill.disarm();
    let rerun = Spy::new(&rig);
    let done = rerun.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rerun.act_names().last(), Some(&start(MAC_UNIT)));
    assert!(!pf_plist(&rig).exists());
}

/// Asserts that a plist that cannot be removed (the path is a directory with a file in it) fails
/// the cutover with the error's text, class environment, before the agent starts, and that a
/// rerun completes once the path is clear. Catches a removal failure that is let be.
#[test]
fn a_pf_plist_that_cannot_be_removed_fails_the_cutover_before_the_agent_starts() {
    let rig = host("hook-retired-remove-fails");
    let path = pf_plist(&rig);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("inside"), "x").unwrap();
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
    assert!(!spy.act_names().contains(&start(MAC_UNIT)));
    rig.kill.disarm();
    fs::remove_dir_all(&path).unwrap();
    let rerun = Spy::new(&rig);
    let done = rerun.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rerun.act_names().last(), Some(&start(MAC_UNIT)));
}

/// Asserts that a plist path that cannot be looked at for another reason than its absence (its
/// directory is a file) is still removed, and that the removal's error fails the cutover with
/// its text, class environment, before the agent starts. Catches a stat error read as "no plist
/// there", which would leave a plist that a reboot loads.
#[test]
fn a_pf_plist_whose_directory_is_a_file_fails_the_cutover_before_the_agent_starts() {
    let rig = host("hook-retired-dir-is-file");
    let path = pf_plist(&rig);
    let dir = path.parent().unwrap();
    fs::remove_dir_all(dir).unwrap();
    fs::write(dir, "not a directory").unwrap();
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
    assert!(!spy.act_names().contains(&start(MAC_UNIT)));
}

/// The state a step leaves behind, with the retired job's plist. A cordon is renewed at times a
/// kill moves, so it is not part of the comparison.
fn ends(rig: &Rig) -> (serde_json::Value, Option<Vec<u8>>, Option<u32>) {
    let mut end = end_state(rig);
    end["cordon"] = serde_json::Value::Null;
    let path = pf_plist(rig);
    (
        end,
        fs::read(&path).ok(),
        fs::metadata(&path)
            .ok()
            .map(|meta| meta.permissions().mode() & 0o777),
    )
}

/// Asserts that a cutover killed at any of its points and run again ends as an uninterrupted one
/// does: the plist gone, the agent started. Catches a retirement that a kill between its two acts
/// leaves half done.
#[test]
fn a_cutover_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one_with_the_pf_job() {
    let baseline = host("hook-retired-sweep-cutover-base");
    assert_eq!(
        Spy::new(&baseline).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    let end = ends(&baseline);
    for k in 1..=points {
        let rig = host(&format!("hook-retired-sweep-cutover-{k}"));
        rig.kill.arm(k);
        assert!(killable(|| Spy::new(&rig).cutover(&rig)).is_none(), "{k}");
        rig.kill.disarm();
        let done = Spy::new(&rig).cutover(&rig);
        assert_eq!(done.outcome, Outcome::Success, "killed at point {k}");
        assert_eq!(ends(&rig), end, "killed at point {k}");
    }
}

/// A host whose update failed at verify, the new agent running, from a release that ran the pf job.
fn verify_failed(name: &str) -> Rig {
    verify_failed_on(host(name))
}

/// Asserts that a rollback to a release that carries the pf job's plist stops the new agent, then
/// replaces the helper, then boots the job out, loads the anchor with the previous release's
/// `js-mac-provision`, puts the plist back (the tree's bytes, mode 0644) and bootstraps the job
/// from the previous tree, all before the previous agent starts. Catches a rollback that leaves a
/// host with the previous agent and no job to load its anchor.
#[test]
fn a_rollback_to_a_release_with_the_pf_job_restores_the_job_before_the_agent_starts() {
    let rig = verify_failed("hook-retired-rollback");
    let previous = rig.params.previous_release.clone().unwrap();
    // A tree whose plist is not world-readable: the mode of the copy is the hook's own.
    fs::set_permissions(
        tree_plist(previous.clone(), MAC_PF),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(!pf_plist(&rig).exists());
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let mut want = vec![
        stop(MAC_UNIT),
        stop(MAC_HELPER),
        bootstrap(previous.clone(), MAC_HELPER),
        start(MAC_HELPER),
    ];
    want.extend(restore_acts(&rig));
    want.push(start(MAC_UNIT));
    assert_eq!(spy.act_names(), want);
    assert_eq!(fs::read(pf_plist(&rig)).unwrap(), PLIST_TEXT.as_bytes());
    assert_eq!(
        fs::metadata(pf_plist(&rig)).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert!(!new_copy(&rig).exists(), "the copy was renamed into place");
}

/// The copy the rollback writes beside the host's plist before it renames it into place.
fn new_copy(rig: &Rig) -> PathBuf {
    let mut path = pf_plist(rig).into_os_string();
    path.push(".new");
    PathBuf::from(path)
}

/// Asserts that a partial copy a killed rollback left beside the host's path (the path itself
/// still absent) is replaced by the whole plist on the rerun, and that the path never held the
/// part. Catches a put-back written in place, whose torn file a rerun would leave for launchd to
/// refuse at the next boot.
#[test]
fn a_partial_copy_left_by_a_killed_rollback_is_replaced_by_the_whole_plist() {
    let rig = verify_failed("hook-retired-rollback-partial");
    fs::create_dir_all(pf_plist(&rig).parent().unwrap()).unwrap();
    fs::write(new_copy(&rig), "<?xml").unwrap();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(fs::read(pf_plist(&rig)).unwrap(), PLIST_TEXT.as_bytes());
    assert!(!new_copy(&rig).exists());
}

/// Asserts that a rollback whose plist is already at the host's path leaves that file as it is
/// (its bytes and mode), and still boots the job out, loads the anchor and bootstraps it. Catches
/// a rollback that overwrites a plist an operator or a later package put there.
#[test]
fn a_rollback_leaves_a_pf_plist_that_is_there_as_it_is() {
    let rig = verify_failed("hook-retired-rollback-present");
    let path = pf_plist(&rig);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "another plist").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let acts = spy.act_names();
    for act in restore_acts(&rig) {
        assert!(acts.contains(&act), "{act}");
    }
    assert_eq!(fs::read_to_string(&path).unwrap(), "another plist");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

/// Asserts that a rollback to a release whose tree has no pf job plist makes no act of the pf job
/// and puts no plist back. Catches a rollback that restores a job a release from this one on does
/// not carry.
#[test]
fn a_rollback_to_a_release_without_the_pf_job_does_none_of_it() {
    let rig = verify_failed_on(running(Rig::mac_with_helper("hook-retired-rollback-none")));
    rig.world.seed_unit(MAC_HELPER, "active", "running");
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    for act in spy.act_names() {
        assert!(
            !act.contains(MAC_PF) && !act.contains("js-mac-provision"),
            "{act}"
        );
    }
    assert!(!pf_plist(&rig).exists());
}

/// Asserts that a stop of the job that fails fails the rollback with its detail, class
/// environment, before the anchor is loaded and the previous agent started.
#[test]
fn a_rollback_whose_pf_job_cannot_be_booted_out_fails_with_the_agent_not_started() {
    let rig = verify_failed("hook-retired-rollback-stop-fails");
    let spy = Spy::new(&rig);
    spy.fail("stop-pf");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("cannot stop js-agent.pf-pool: stop-pf is down")
    );
    assert_eq!(spy.act_names().last(), Some(&stop(MAC_PF)));
}

/// Asserts that an anchor the previous release's `js-mac-provision` cannot load fails the
/// rollback with its detail before the plist is put back and the job bootstrapped.
#[test]
fn a_rollback_whose_anchor_cannot_be_loaded_fails_before_the_plist_is_put_back() {
    let rig = verify_failed("hook-retired-rollback-install-fails");
    let spy = Spy::new(&rig);
    spy.fail("pf-install");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.unwrap(),
        format!(
            "cannot load the pf anchor of {} with {}: pf-install is down",
            rig.params.flags_file.display(),
            program(&rig)
        )
    );
    assert_eq!(spy.act_names().last(), Some(&install(&rig)));
    assert!(!pf_plist(&rig).exists());
}

/// Asserts that a plist that cannot be put back (the host has no launch daemons directory) fails
/// the rollback with the error's text before the job is bootstrapped and the agent started.
#[test]
fn a_rollback_whose_pf_plist_cannot_be_put_back_fails_before_the_job_is_bootstrapped() {
    let rig = verify_failed("hook-retired-rollback-copy-fails");
    let path = pf_plist(&rig);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
    let tree = tree_plist(rig.params.previous_release.clone().unwrap(), MAC_PF);
    let why = fs::copy(&tree, &path).unwrap_err();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.unwrap(),
        format!(
            "cannot put {} back at {}: {why}",
            tree.display(),
            path.display()
        )
    );
    assert_eq!(spy.act_names().last(), Some(&install(&rig)));
}

/// Asserts that a job that cannot be bootstrapped fails the rollback with its detail before the
/// previous agent starts, and that a rerun completes the acts. Catches a previous agent started
/// with no job to keep its anchor.
#[test]
fn a_rollback_whose_pf_job_cannot_be_bootstrapped_fails_with_the_agent_not_started() {
    let rig = verify_failed("hook-retired-rollback-bootstrap-fails");
    let spy = Spy::new(&rig);
    spy.fail("bootstrap-pf");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
    let plist = tree_plist(rig.params.previous_release.clone().unwrap(), MAC_PF);
    assert_eq!(
        done.report.detail.unwrap(),
        format!(
            "cannot bootstrap js-agent.pf-pool from {}: bootstrap-pf is down",
            plist.display()
        )
    );
    assert!(!spy.act_names().contains(&start(MAC_UNIT)));
}

/// Asserts that a rollback killed at any of its points and run again ends as an uninterrupted one
/// does: the plist back with the tree's bytes and mode 0644, the previous agent started. Catches a
/// kill between the acts of the restoration that a rerun does not finish.
#[test]
fn a_rollback_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one_with_the_pf_job() {
    let build = |name: &str| verify_failed(name);
    let baseline = build("hook-retired-sweep-rollback-base");
    let before = baseline.kill.points();
    assert_eq!(
        Spy::new(&baseline).rollback(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points() - before;
    assert!(points > 0);
    let end = ends(&baseline);
    assert_eq!(end.1.as_deref(), Some(PLIST_TEXT.as_bytes()));
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
