//! `cutover` and `verify` on a Mac, over the real `launchctl` port and a fake launchd: how a
//! start that goes wrong is classed. The steps ask the unit port whether the agent ran and
//! died; on Linux `failed` says so, and on a Mac the port has to say it from what `launchctl
//! print` shows, or a broken release is read as a fault of the host and deferred for ever.

use std::path::Path;

use js_agent::update_hook::step::{self, Env};
use js_agent::update_hook::{Class, Done, LaunchUnits, Outcome, Reason};
use js_testkit::dir::TestDir;

use super::step_rig::{MAC_UNIT, Rig, Spy, as_root, running};
use super::unit_conformance::{Exit, Launchctl, Service, Then};

/// A Mac whose agent runs under the fake launchd.
fn mac(name: &str) -> (Rig, Launchctl) {
    let rig = running(Rig::mac(name));
    let tool = Launchctl::default();
    tool.load(MAC_UNIT, Service::running());
    (rig, tool)
}

/// Runs a step with the unit port over `tool`, the token file at `gate`.
fn run_over(rig: &Rig, tool: &Launchctl, gate: &Path, run: impl FnOnce(&Env<'_>) -> Done) -> Done {
    let spy = Spy::new(rig);
    let units = LaunchUnits::new(tool).with_clock(&spy).with_gate(gate);
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
fn an_agent_that_launchd_leaves_idle_after_the_start_is_a_failure_of_the_host() {
    // The token file is there; launchd bootstraps the plist and does not run the agent.
    let (rig, tool) = mac("hook-launchd-left-idle");
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
