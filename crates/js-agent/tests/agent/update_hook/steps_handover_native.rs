//! An update on a Mac (launchd, the native runtime) that hands the agent's native runs over to
//! the new agent instead of draining the host: `prepare` decides it when the previous and the
//! staged release's agents both list `native-handover` on their `stops:` line, the plist is the
//! same and the host's flags do not turn the VM backend on; `cutover` and `rollback` then write
//! the handover file before they boot the agent out and never wait for the host to be idle.
//! Every other Mac update drains as before, and the outcome names the cause.

use std::path::PathBuf;

use js_agent::lifecycle::handover::HANDOVER_FILE;
use js_agent::update_hook::{Class, Manager, Outcome, Reason, agent_in};

use super::step_rig::{MAC_UNIT, Rig, Spy};
use super::steps_handover::NO_NATIVE_LEAVE;

/// What a Mac agent of this release prints to `--version`.
const LISTS: &str = "js-agent 1.4.0\nruntimes: native\nstops: drain native-handover\n";

/// What a Mac agent of the release before prints: no `stops:` line.
const BEFORE: &str = "js-agent 1.3.0\nruntimes: native\n";

const REQUEST: &str = "handover.request";
const WITHDRAW: &str = "handover.withdraw";

const HANDOVER: &str =
    "the cutover hands the running jobs over to the new agent: no cordon and no drain";
const DRAINS: &str = "the cutover drains the host first: ";
const NO_NATIVE_ADOPT: &str = "the staged release's agent does not adopt the native jobs \
     (no `native-handover` on its `stops:` line)";
const VM_BACKEND: &str = "the host's flags turn the VM backend on (`--vm-driver`), or its flags \
     file cannot be read, and an agent never adopts a VM run";

fn stop() -> String {
    format!("units.stop({MAC_UNIT})")
}

fn start() -> String {
    format!("units.start({MAC_UNIT})")
}

/// The handover file of the agent's state root.
fn file(rig: &Rig) -> PathBuf {
    rig.state_root().path().join(HANDOVER_FILE)
}

/// A spy whose agents, the previous release's and the staged one's, both list the word.
fn spy(rig: &Rig) -> Spy<'_> {
    let spy = Spy::new(rig);
    *spy.version.borrow_mut() = Ok(LISTS.to_owned());
    spy
}

/// A busy Mac (two leases) as `prepare` leaves it for an update that hands over.
fn handing(name: &str) -> Rig {
    let rig = Rig::mac(name);
    rig.world.seed_unit(&rig.unit, "active", "running");
    rig.set_leases(2);
    let done = spy(&rig).prepare(&rig);
    assert_eq!(
        done.outcome,
        Outcome::Success,
        "prepare of the setup: {done:?}"
    );
    assert!(rig.record().unwrap().handover);
    rig.kill.disarm();
    rig
}

/// Asserts that `prepare` of a busy Mac whose previous and staged agents both list
/// `native-handover` records a hand-over (not in place: launchd only boots a job out and
/// bootstraps it), places no cordon, waits for nothing and says so, having read both agents'
/// `--version`; then that the cutover writes the request before it boots the agent out, removes
/// it once the agent is down, starts the staged agent and leaves the jobs running; and that the
/// rollback, after a verify, hands the runs back the same way, to the previous agent. Catches a
/// Mac update that still cordons a busy host and waits for its jobs, one that hands over without
/// asking the staged agent whether it adopts native runs, and a boot-out with no request, which
/// the agent takes as a drain.
#[test]
fn a_mac_update_between_agents_that_both_list_native_handover_hands_the_runs_over_and_back() {
    let rig = Rig::mac("hook-native-handover");
    rig.world.seed_unit(&rig.unit, "active", "running");
    rig.set_leases(2);
    let prepare = spy(&rig);
    let done = prepare.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail.as_deref(), Some(HANDOVER));
    let record = rig.record().unwrap();
    assert!(record.handover);
    assert!(!record.in_place);
    assert!(!record.cordon.placed);
    assert_eq!(rig.cordon_expiry(), None);
    assert_eq!(rig.world.ticks(), 0, "nothing is waited for");
    for release in [
        rig.params.previous_release.clone().unwrap(),
        rig.params.staged_release.clone(),
    ] {
        let agent = agent_in(Manager::Launchd, &release);
        let asked = format!("run({} --version)", agent.display());
        assert!(
            prepare.log().contains(&asked),
            "{asked}: {:?}",
            prepare.log()
        );
    }
    assert!(
        prepare
            .log()
            .iter()
            .all(|call| !call.starts_with("cordon("))
    );
    assert!(prepare.log().iter().all(|call| !call.contains("systemctl")));
    rig.kill.disarm();

    let cutover = spy(&rig);
    let done = cutover.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(cutover.act_names(), [REQUEST, &stop(), WITHDRAW, &start()]);
    assert!(!file(&rig).exists());
    assert_eq!(rig.host().leases, 2, "the jobs run on");
    assert_eq!(rig.world.ticks(), 0, "nothing is waited for");
    assert!(
        cutover
            .log()
            .iter()
            .all(|call| !call.starts_with("cordon("))
    );

    assert_eq!(spy(&rig).verify(&rig).outcome, Outcome::Success);
    let rollback = spy(&rig);
    let done = rollback.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rollback.act_names(), [REQUEST, &stop(), WITHDRAW, &start()]);
    assert!(
        rollback
            .log()
            .iter()
            .all(|call| !call.starts_with("cordon("))
    );
    assert!(!file(&rig).exists());
    assert_eq!(rig.host().leases, 2, "the jobs run on");
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    assert!(rig.record().is_none(), "the rollback finished");
}

/// Asserts that every Mac update that cannot hand over drains (the record says no hand-over and
/// the cordon is placed), and that the detail names the one condition that made it drain, in
/// full: a previous agent of the release before (no `stops:` line), one that lists only Linux's
/// `handover`, one that cannot be run, a staged agent without the word, flags that turn the VM
/// backend on or cannot be read, a plist that changes, and a runtime that is not the native one;
/// and that the systemd conditions are not looked at: flags that set an egress ceiling, which a
/// Mac does not have, still hand over. Catches a hand-over asked of an agent that would drain on
/// it, one to an agent that would end the runs it finds, a VM run destroyed by the agent that
/// starts, and a drain whose journal line leaves the operator guessing why.
#[test]
fn every_mac_update_that_cannot_hand_over_drains_and_says_why() {
    type Setup = fn(&Spy<'_>);
    let unit = "the unit changes (js-agent.plist): cutover starts the agent on the new unit";
    let cases: [(&str, Setup, Option<String>); 9] = [
        (
            "previous-before",
            |spy| *spy.previous_version.borrow_mut() = Some(Ok(BEFORE.to_owned())),
            Some(format!("{DRAINS}{NO_NATIVE_LEAVE}")),
        ),
        (
            "previous-linux-word",
            |spy| {
                *spy.previous_version.borrow_mut() = Some(Ok(
                    "js-agent 1.3.0\nruntimes: native\nstops: drain handover\n".to_owned(),
                ));
            },
            Some(format!("{DRAINS}{NO_NATIVE_LEAVE}")),
        ),
        (
            "previous-fails",
            |spy| *spy.previous_version.borrow_mut() = Some(Err("exec failed".to_owned())),
            Some(format!("{DRAINS}{NO_NATIVE_LEAVE}")),
        ),
        (
            "staged-without",
            |spy| {
                *spy.version.borrow_mut() =
                    Ok("js-agent 1.4.0\nruntimes: native\nstops: drain\n".to_owned());
                *spy.previous_version.borrow_mut() = Some(Ok(LISTS.to_owned()));
            },
            Some(format!("{DRAINS}{NO_NATIVE_ADOPT}")),
        ),
        (
            "vms",
            |spy| spy.vms.set(true),
            Some(format!("{DRAINS}{VM_BACKEND}")),
        ),
        ("plist-changed", |_| {}, Some(unit.to_owned())),
        (
            "not-native",
            |spy| {
                *spy.version.borrow_mut() =
                    Ok("js-agent 1.4.0\nstops: drain native-handover\n".to_owned());
            },
            Some(format!("{DRAINS}the jobs do not run on the native runtime")),
        ),
        (
            "egress-handover",
            |spy| spy.egress.set(true),
            Some(HANDOVER.to_owned()),
        ),
        ("handover", |_| {}, Some(HANDOVER.to_owned())),
    ];
    for (name, setup, detail) in cases {
        let mut rig = Rig::mac(&format!("hook-native-not-{name}"));
        match name {
            "plist-changed" => rig.stage_changed_unit(),
            "not-native" => rig.params.settings.runtime = "crun".to_owned(),
            _ => {}
        }
        rig.world.seed_unit(&rig.unit, "active", "running");
        let spy = Spy::new(&rig);
        *spy.version.borrow_mut() = Ok(LISTS.to_owned());
        setup(&spy);
        let done = spy.prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{name}: {done:?}");
        assert_eq!(done.report.detail, detail, "{name}");
        let record = rig.record().unwrap();
        let hands_over = name.ends_with("handover");
        assert_eq!(record.handover, hands_over, "{name}");
        assert_eq!(record.cordon.placed, !hands_over, "{name}");
        assert!(!record.in_place, "{name}");
    }
}

/// Asserts that a Mac update from an agent of the release before (no `native-handover`) to one
/// that lists it drains, and so does its rollback, back to the agent without the word: the
/// cutover and the rollback write no request, the rollback does not boot the new agent out
/// while a lease runs (it fails at the drain bound with the agent running, the class an
/// environment matter), and once the host holds no lease it stops and starts the agents with no
/// request. Catches a rollback that asks an agent to hand over runs to one that would end them
/// as gone.
#[test]
fn a_mac_update_from_an_agent_without_the_word_drains_and_so_does_its_rollback() {
    let rig = Rig::mac("hook-native-rollback-drains");
    rig.world.seed_unit(&rig.unit, "active", "running");
    let with_before = |rig: &Rig| {
        let spy = spy(rig);
        *spy.previous_version.borrow_mut() = Some(Ok(BEFORE.to_owned()));
        spy
    };
    let done = with_before(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail,
        Some(format!("{DRAINS}{NO_NATIVE_LEAVE}"))
    );
    assert!(!rig.record().unwrap().handover);
    assert!(rig.record().unwrap().cordon.placed);
    rig.kill.disarm();

    let cutover = with_before(&rig);
    assert_eq!(cutover.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(cutover.act_names(), [stop(), start()]);
    assert_eq!(with_before(&rig).verify(&rig).outcome, Outcome::Success);

    rig.set_leases(1);
    let held = with_before(&rig);
    let done = held.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::DrainBound));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        held.act_names(),
        Vec::<String>::new(),
        "the agent is not booted out while a job runs"
    );
    assert!(!file(&rig).exists());

    rig.set_leases(0);
    let rollback = with_before(&rig);
    assert_eq!(rollback.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(rollback.act_names(), [stop(), start()]);
    assert!(!file(&rig).exists());
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    assert!(rig.record().is_none(), "the rollback finished");
}

/// Asserts that the hand-over a Mac update decided is kept by a rerun of `prepare`, which
/// writes nothing and says the same; and that a rerun once the previous agent no longer lists
/// the word (a release link moved by hand) keeps the hand-over it began with, as on Linux.
/// Catches a rerun that turns a Mac hand-over into a drain halfway, with no cordon placed.
#[test]
fn a_rerun_of_a_mac_prepare_keeps_the_hand_over() {
    let rig = handing("hook-native-rerun");
    let record = rig.record().unwrap();
    let again = spy(&rig);
    let done = again.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail.as_deref(), Some(HANDOVER));
    assert_eq!(rig.record().unwrap(), record);
    rig.kill.disarm();
    let changed = spy(&rig);
    *changed.previous_version.borrow_mut() = Some(Ok(BEFORE.to_owned()));
    let done = changed.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail.as_deref(), Some(HANDOVER));
    assert_eq!(rig.record().unwrap(), record);
    assert_eq!(rig.cordon_expiry(), None);
}
