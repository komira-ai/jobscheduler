//! The Mac package's pf job (`pf_unit`): `cutover` boots it out, loads the anchor of the host's
//! flags file with the staged release's `js-mac-provision`, and bootstraps the job from the staged
//! release, after the helper and before the agent starts. `rollback` leaves it as it is. A
//! systemd host that names one is refused by every step.

use std::path::PathBuf;

use js_agent::update_hook::{Class, Outcome, Reason};

use super::rollback_entries::verify_failed_on;
use super::step_rig::{MAC_HELPER, MAC_PF, MAC_UNIT, Rig, Spy, end_state, killable, running};

/// A Mac host with the helper and the pf job, after `prepare`, the old helper running.
fn host(name: &str) -> Rig {
    let rig = running(Rig::mac_with_helper_and_pf(name));
    rig.world.seed_unit(MAC_HELPER, "active", "running");
    rig
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
        release
            .join(format!("tree/Library/LaunchDaemons/{label}.plist"))
            .display()
    )
}

/// The staged release's `js-mac-provision` loading the anchor of the host's flags file.
fn install(rig: &Rig) -> String {
    format!(
        "pf.install --flags-file {} {}",
        rig.params.flags_file.display(),
        program(rig)
    )
}

fn program(rig: &Rig) -> String {
    rig.params
        .staged_release
        .join("tree/Library/Application Support/js-agent/bin/js-mac-provision")
        .display()
        .to_string()
}

/// What `cutover` does to the pf job, in order.
fn pf_acts(rig: &Rig) -> [String; 3] {
    [
        stop(MAC_PF),
        install(rig),
        bootstrap(rig.params.staged_release.clone(), MAC_PF),
    ]
}

#[test]
fn a_cutover_loads_the_anchor_and_bootstraps_the_pf_job_after_the_helper_before_the_agent() {
    let rig = host("hook-pf-cutover");
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let staged = rig.params.staged_release.clone();
    let mut want = vec![
        stop(MAC_UNIT),
        stop(MAC_HELPER),
        bootstrap(staged, MAC_HELPER),
        start(MAC_HELPER),
    ];
    want.extend(pf_acts(&rig));
    want.push(start(MAC_UNIT));
    assert_eq!(spy.act_names(), want);
}

#[test]
fn a_first_install_loads_the_anchor_and_bootstraps_the_pf_job_before_the_agent_starts() {
    let rig = Rig::mac_first_install_with_helper_and_pf("hook-pf-first");
    assert_eq!(Spy::new(&rig).prepare(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let acts = spy.act_names();
    assert_eq!(acts[acts.len() - 4..acts.len() - 1], pf_acts(&rig));
    assert_eq!(acts.last(), Some(&start(MAC_UNIT)));
}

#[test]
fn a_cutover_that_ran_before_does_not_touch_the_pf_job_again() {
    let rig = host("hook-pf-twice");
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    let second = Spy::new(&rig);
    assert_eq!(second.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(second.act_names(), Vec::<String>::new());
}

/// An anchor that cannot be loaded fails the cutover before the agent is started (it would refuse
/// to start without it), and the job is not bootstrapped.
#[test]
fn an_anchor_that_cannot_be_loaded_fails_the_cutover_with_the_agent_not_started() {
    let rig = host("hook-pf-install-fails");
    let spy = Spy::new(&rig);
    spy.fail("pf-install");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!done.report.reversible);
    assert_eq!(
        done.report.detail.unwrap(),
        format!(
            "cannot load the pf anchor of {} with {}: pf-install is down",
            rig.params.flags_file.display(),
            program(&rig)
        )
    );
    let acts = spy.act_names();
    assert_eq!(acts.last(), Some(&install(&rig)));
    assert!(!acts.contains(&start(MAC_UNIT)));
}

#[test]
fn a_pf_job_that_cannot_be_stopped_fails_the_cutover_before_the_anchor_is_loaded() {
    let rig = host("hook-pf-stop-fails");
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
}

#[test]
fn a_pf_job_that_cannot_be_bootstrapped_fails_the_cutover_with_the_agent_not_started() {
    let rig = host("hook-pf-bootstrap-fails");
    let spy = Spy::new(&rig);
    spy.fail("bootstrap-pf");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
    let plist = rig
        .params
        .staged_release
        .join("tree/Library/LaunchDaemons/js-agent.pf-pool.plist");
    assert_eq!(
        done.report.detail.unwrap(),
        format!(
            "cannot bootstrap js-agent.pf-pool from {}: bootstrap-pf is down",
            plist.display()
        )
    );
    let acts = spy.act_names();
    assert_eq!(
        acts.last(),
        Some(&bootstrap(rig.params.staged_release.clone(), MAC_PF))
    );
    assert!(!acts.contains(&start(MAC_UNIT)));
}

/// A cutover that failed on the pf job is run again once the fault is gone: it does the pf job's
/// acts again from the bootout on, and starts the agent.
#[test]
fn a_cutover_run_again_after_the_anchor_failed_loads_it_and_starts_the_agent() {
    let rig = host("hook-pf-install-again");
    let failed = Spy::new(&rig);
    failed.fail("pf-install");
    assert_eq!(failed.cutover(&rig).outcome, Outcome::Failure);
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let acts = spy.act_names();
    assert_eq!(acts[acts.len() - 4..acts.len() - 1], pf_acts(&rig));
    assert_eq!(acts.last(), Some(&start(MAC_UNIT)));
}

#[test]
fn a_rollback_leaves_the_pf_job_as_it_is() {
    let rig = verify_failed_on(host("hook-pf-rollback"));
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let acts = spy.act_names();
    assert!(!acts.is_empty());
    for act in acts {
        assert!(
            !act.contains(MAC_PF) && !act.contains("js-mac-provision"),
            "{act}"
        );
    }
}

/// The state a step leaves behind. A cordon is renewed at times a kill moves, so it is not part of
/// the comparison.
fn ends(rig: &Rig) -> serde_json::Value {
    let mut end = end_state(rig);
    end["cordon"] = serde_json::Value::Null;
    end
}

#[test]
fn a_cutover_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one_with_the_pf_job() {
    let baseline = host("hook-pf-sweep-cutover-base");
    assert_eq!(
        Spy::new(&baseline).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    let end = ends(&baseline);
    for k in 1..=points {
        let rig = host(&format!("hook-pf-sweep-cutover-{k}"));
        rig.kill.arm(k);
        assert!(killable(|| Spy::new(&rig).cutover(&rig)).is_none(), "{k}");
        rig.kill.disarm();
        let rerun = Spy::new(&rig);
        let done = rerun.cutover(&rig);
        assert_eq!(done.outcome, Outcome::Success, "killed at point {k}");
        assert_eq!(ends(&rig), end, "killed at point {k}");
        // A rerun that does the pf job's acts does all three, in order, and then starts the agent.
        let acts = rerun.act_names();
        if let Some(at) = acts.iter().position(|act| *act == stop(MAC_PF)) {
            let mut want = pf_acts(&rig).to_vec();
            want.push(start(MAC_UNIT));
            assert_eq!(acts[at..], want, "killed at point {k}");
        }
    }
}

#[test]
fn every_step_refuses_a_pf_job_on_a_systemd_host_before_it_does_anything() {
    let mut rig = Rig::new("hook-pf-systemd");
    rig.params.settings.pf_unit = Some(MAC_PF.to_owned());
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
            Some("pf_unit \"js-agent.pf-pool\" is set, and only a launchd host has a pf job")
        );
    }
    assert_eq!(spy.log(), Vec::<String>::new());
    assert!(rig.record().is_none());
}
