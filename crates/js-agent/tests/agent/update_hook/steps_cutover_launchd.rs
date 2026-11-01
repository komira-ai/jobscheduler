//! `cutover` and `verify` on a Mac, over the real `launchctl` port and a fake launchd: how a
//! start that goes wrong is classed. The steps ask the unit port whether the agent ran and
//! died; on Linux `failed` says so, and on a Mac the port has to say it from what `launchctl
//! print` shows, or a broken release is read as a fault of the host and deferred for ever.

use std::path::Path;

use js_agent::update_hook::step::{self, Env};
use js_agent::update_hook::{
    Class, Done, LaunchUnits, Link, Outcome, PortError, Reason, UnitState, Units,
};
use js_testkit::dir::TestDir;
use serde_json::{Value, json};

use super::step_rig::{MAC_UNIT, Rig, Spy, as_root, end_state, killable, running};
use super::unit_conformance::{Exit, Launchctl, Service, Then};

/// A Mac whose agent runs under the fake launchd.
fn mac(name: &str) -> (Rig, Launchctl) {
    let rig = running(Rig::mac(name));
    // Time passes between the placing of the cordon and the start of the new agent, so that
    // the sessions of the agents the server shows differ.
    rig.advance_clock(5);
    let tool = Launchctl::default();
    tool.load(MAC_UNIT, Service::running());
    (rig, tool)
}

/// The launchd port, and the hello the agent it starts says once it runs: the version of the
/// release the link names.
struct Saying<'a> {
    launchd: LaunchUnits<'a>,
    rig: &'a Rig,
}

impl Units for Saying<'_> {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        self.launchd.show(unit)
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        self.launchd.stop_no_block(unit)
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        self.launchd.start(unit)?;
        if self.launchd.is_active(unit)? {
            let linked = self.rig.world.target(&self.rig.link)?;
            let version = if linked == self.rig.params.previous_release {
                "1.3.0"
            } else {
                "1.4.0"
            };
            self.rig
                .world
                .hello(version)
                .map_err(|why| PortError::Io(why.to_string()))?;
        }
        Ok(())
    }

    fn is_active(&self, unit: &str) -> Result<bool, PortError> {
        self.launchd.is_active(unit)
    }
}

/// Runs a step with the unit port over `tool`, the token file at `gate`.
fn run_over(rig: &Rig, tool: &Launchctl, gate: &Path, run: impl FnOnce(&Env<'_>) -> Done) -> Done {
    let spy = Spy::new(rig);
    let units = Saying {
        launchd: LaunchUnits::new(tool).with_clock(&spy).with_gate(gate),
        rig,
    };
    let mut env = spy.env(rig, &as_root);
    env.ports.units = &units;
    run(&env)
}

fn cutover(rig: &Rig, tool: &Launchctl, gate: &Path) -> Done {
    run_over(rig, tool, gate, |env| step::cutover(env, &rig.params))
}

fn verify(rig: &Rig, tool: &Launchctl, gate: &Path) -> Done {
    run_over(rig, tool, gate, |env| step::verify(env, &rig.params))
}

fn rollback(rig: &Rig, tool: &Launchctl, gate: &Path) -> Done {
    run_over(rig, tool, gate, |env| step::rollback(env, &rig.params))
}

/// A token file that exists.
fn with_token(tmp: &TestDir) -> std::path::PathBuf {
    let gate = tmp.path().join("token");
    std::fs::write(&gate, "token").unwrap();
    gate
}

#[test]
fn a_mac_cutover_stops_boots_out_links_and_starts_the_agent_again() {
    let (rig, tool) = mac("hook-launchd-clean");
    let gate = with_token(&rig.tmp);
    let done = cutover(&rig, &tool, &gate);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(tool.launches.borrow().get(MAC_UNIT), Some(&1));
    assert_eq!(tool.loaded.borrow()[MAC_UNIT].state, "running");
}

#[test]
fn a_mac_agent_that_starts_and_dies_is_a_failure_of_the_release() {
    for exit in [Exit::Code(1), Exit::Code(78), Exit::Signal] {
        let (rig, tool) = mac(&format!("hook-launchd-dies-{exit:?}"));
        let gate = with_token(&rig.tmp);
        tool.next_start.set(Then::Dies(exit));
        let done = cutover(&rig, &tool, &gate);
        assert_eq!(done.outcome, Outcome::Failure, "{exit:?}");
        assert_eq!(done.report.reason, Some(Reason::StartFailed), "{exit:?}");
        assert_eq!(done.report.class, Class::Release, "{exit:?}");
        assert!(!done.report.reversible);
    }
}

#[test]
fn a_mac_agent_that_launchd_is_slow_to_start_is_waited_for() {
    let (rig, tool) = mac("hook-launchd-slow");
    let gate = with_token(&rig.tmp);
    tool.settle.set(4);
    let done = cutover(&rig, &tool, &gate);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}

#[test]
fn a_plist_launchd_cannot_load_is_a_failure_of_the_host_and_not_of_the_release() {
    let (rig, tool) = mac("hook-launchd-bad-plist");
    let gate = with_token(&rig.tmp);
    // Bootout unloads the service; the plist is gone from the directory: bootstrap fails.
    tool.plists.borrow_mut().remove(MAC_UNIT);
    let done = cutover(&rig, &tool, &gate);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
}

#[test]
fn an_agent_launchd_holds_back_for_want_of_its_token_is_a_failure_of_the_host() {
    let (rig, tool) = mac("hook-launchd-no-token");
    let gate = with_token(&rig.tmp);
    tool.next_start.set(Then::Idle);
    let done = cutover(&rig, &tool, &gate);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
}

#[test]
fn a_mac_agent_that_dies_after_cutover_fails_verify_as_a_start_failure() {
    let (rig, tool) = mac("hook-launchd-verify-dies");
    let gate = with_token(&rig.tmp);
    assert_eq!(cutover(&rig, &tool, &gate).outcome, Outcome::Success);
    tool.loaded.borrow_mut().insert(
        MAC_UNIT.to_owned(),
        Service::new("not running", 2, Exit::Code(1)),
    );
    let done = verify(&rig, &tool, &gate);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Release);
}

/// Everything a step leaves behind: the rig's record, link, cordon, and the daemon launchd holds.
fn left_behind(rig: &Rig, tool: &Launchctl) -> Value {
    let service = tool
        .loaded
        .borrow()
        .get(MAC_UNIT)
        .map(|service| (service.state, service.runs));
    json!({
        "end": end_state(rig),
        "service": service,
        "launches": tool.launches.borrow().get(MAC_UNIT),
    })
}

/// A Mac whose new agent serves, after a `cutover`: what a `rollback` finds when a `verify`
/// failed.
fn serving_new(name: &str) -> (Rig, Launchctl) {
    let (rig, tool) = mac(name);
    let gate = with_token(&rig.tmp);
    assert_eq!(cutover(&rig, &tool, &gate).outcome, Outcome::Success);
    rig.kill.disarm();
    (rig, tool)
}

/// Runs `run` of `build`'s host to its end, then again from a fresh host once for each kill
/// point it passes, killed there and run again, and requires the same end every time, in the
/// record, the link, the cordon and the daemon launchd holds.
fn sweep(
    label: &str,
    build: fn(&str) -> (Rig, Launchctl),
    run: fn(&Rig, &Launchctl, &Path) -> Done,
) {
    let (baseline, tool) = build(&format!("{label}-base"));
    let gate = with_token(&baseline.tmp);
    assert_eq!(run(&baseline, &tool, &gate).outcome, Outcome::Success);
    let points = baseline.kill.points();
    assert!(
        points >= 5,
        "the marks and the acts pass kill points: {points}"
    );
    let end = left_behind(&baseline, &tool);
    for k in 1..=points {
        let (rig, tool) = build(&format!("{label}-{k}"));
        let gate = with_token(&rig.tmp);
        rig.kill.arm(k);
        assert!(
            killable(|| run(&rig, &tool, &gate)).is_none(),
            "kill point {k} of {points} is reached"
        );
        rig.kill.disarm();
        let running = tool
            .loaded
            .borrow()
            .get(MAC_UNIT)
            .is_some_and(|service| service.state == "running");
        rig.started_agent_speaks(running);
        let done = run(&rig, &tool, &gate);
        assert_eq!(
            done.outcome,
            Outcome::Success,
            "killed at point {k}: {done:?}"
        );
        assert_eq!(left_behind(&rig, &tool), end, "killed at point {k}");
    }
}

#[test]
fn a_mac_cutover_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    sweep("hook-launchd-sweep-cutover", mac, cutover);
}

#[test]
fn a_mac_rollback_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    sweep("hook-launchd-sweep-rollback", serving_new, rollback);
}

#[test]
fn a_mac_rollback_boots_the_new_agent_out_and_the_old_one_in_over_launchd() {
    let (rig, tool) = serving_new("hook-launchd-rollback");
    let gate = with_token(&rig.tmp);
    let done = rollback(&rig, &tool, &gate);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    assert_eq!(tool.loaded.borrow()[MAC_UNIT].state, "running");
    // One bootstrap for the cutover's start, one for the rollback's.
    assert_eq!(tool.launches.borrow().get(MAC_UNIT), Some(&2));
}
