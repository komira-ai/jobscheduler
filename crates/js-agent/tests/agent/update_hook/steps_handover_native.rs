//! An update on a Mac (launchd, the native runtime) that hands the agent's native runs over to
//! the new agent instead of draining the host: `prepare` decides it when the previous and the
//! staged release's agents both list `native-handover` on their `stops:` line, the plist is the
//! same and the host's flags do not turn the VM backend on; `cutover` and `rollback` then write
//! the handover file before they boot the agent out and never wait for the host to be idle.
//! Every other Mac update drains as before, and the outcome names the cause.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use js_agent::lifecycle::handover::HANDOVER_FILE;
use js_agent::update_hook::{
    Class, DrainParams, Host, Invocation, MaintainInvocation, Manager, Outcome, Params, Reason,
    Step, VERB, agent_in, run,
};

use super::step_rig::{MAC_HELPER, MAC_UNIT, Rig, Spy};
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

/// The boot-out of an earlier release's helper job, which every Mac cutover makes before the
/// agent's start (`step/retired.rs`).
fn retire() -> String {
    format!("units.stop({MAC_HELPER})")
}

/// The handover file of the agent's state root.
fn file_of(rig: &Rig) -> PathBuf {
    rig.state_root().path().join(HANDOVER_FILE)
}

/// A spy whose agents, the previous release's and the staged one's, both list the word.
fn spy(rig: &Rig) -> Spy<'_> {
    let spy = Spy::new(rig);
    *spy.version.borrow_mut() = Ok(LISTS.to_owned());
    spy
}

/// A spy whose staged agent lists the word and whose previous agent is of the release before.
fn with_before(rig: &Rig) -> Spy<'_> {
    let spy = spy(rig);
    *spy.previous_version.borrow_mut() = Some(Ok(BEFORE.to_owned()));
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
    assert_eq!(
        cutover.act_names(),
        [REQUEST, &stop(), WITHDRAW, &retire(), &start()]
    );
    assert!(!file_of(&rig).exists());
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
    assert!(!file_of(&rig).exists());
    assert_eq!(rig.host().leases, 2, "the jobs run on");
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    assert!(rig.record().is_none(), "the rollback finished");
}

/// Asserts that every Mac update that cannot hand over drains (the record says no hand-over and
/// the cordon is placed), and that the detail names the one condition that made it drain, in
/// full: a previous agent of the release before (no `stops:` line), one that lists only Linux's
/// `handover`, one that cannot be run, a staged agent without the word, flags that turn the VM
/// backend on or cannot be read, a plist that changes, a runtime that is not the native one, and
/// both of the last two (the plist's note first, then the runtime's cause);
/// and that the systemd conditions are not looked at: flags that set an egress ceiling, which a
/// Mac does not have, still hand over. Catches a hand-over asked of an agent that would drain on
/// it, one to an agent that would end the runs it finds, a VM run destroyed by the agent that
/// starts, and a drain whose journal line leaves the operator guessing why.
#[test]
fn every_mac_update_that_cannot_hand_over_drains_and_says_why() {
    type Setup = fn(&Spy<'_>);
    let unit = "the unit changes (js-agent.plist): cutover starts the agent on the new unit";
    // Each case: its name, what it sets, the detail of the success, and whether it hands over.
    let cases: [(&str, Setup, Option<String>, bool); 10] = [
        (
            "previous-before",
            |spy| *spy.previous_version.borrow_mut() = Some(Ok(BEFORE.to_owned())),
            Some(format!("{DRAINS}{NO_NATIVE_LEAVE}")),
            false,
        ),
        (
            "previous-linux-word",
            |spy| {
                *spy.previous_version.borrow_mut() = Some(Ok(
                    "js-agent 1.3.0\nruntimes: native\nstops: drain handover\n".to_owned(),
                ));
            },
            Some(format!("{DRAINS}{NO_NATIVE_LEAVE}")),
            false,
        ),
        (
            "previous-fails",
            |spy| *spy.previous_version.borrow_mut() = Some(Err("exec failed".to_owned())),
            Some(format!("{DRAINS}{NO_NATIVE_LEAVE}")),
            false,
        ),
        (
            "staged-without",
            |spy| {
                *spy.version.borrow_mut() =
                    Ok("js-agent 1.4.0\nruntimes: native\nstops: drain\n".to_owned());
                *spy.previous_version.borrow_mut() = Some(Ok(LISTS.to_owned()));
            },
            Some(format!("{DRAINS}{NO_NATIVE_ADOPT}")),
            false,
        ),
        (
            "vms",
            |spy| spy.vms.set(true),
            Some(format!("{DRAINS}{VM_BACKEND}")),
            false,
        ),
        ("plist-changed", |_| {}, Some(unit.to_owned()), false),
        (
            "not-native",
            |spy| {
                *spy.version.borrow_mut() =
                    Ok("js-agent 1.4.0\nstops: drain native-handover\n".to_owned());
            },
            Some(format!("{DRAINS}the jobs do not run on the native runtime")),
            false,
        ),
        (
            "egress-ignored",
            |spy| spy.egress.set(true),
            Some(HANDOVER.to_owned()),
            true,
        ),
        (
            "not-native-plist-changed",
            |spy| {
                *spy.version.borrow_mut() =
                    Ok("js-agent 1.4.0\nstops: drain native-handover\n".to_owned());
            },
            Some(format!(
                "{unit}; {DRAINS}the jobs do not run on the native runtime"
            )),
            false,
        ),
        ("handover", |_| {}, Some(HANDOVER.to_owned()), true),
    ];
    for (name, setup, detail, hands_over) in cases {
        let mut rig = Rig::mac(&format!("hook-native-not-{name}"));
        if name.ends_with("plist-changed") {
            rig.stage_changed_unit();
        }
        if name.starts_with("not-native") {
            rig.params.settings.runtime = "crun".to_owned();
        }
        rig.world.seed_unit(&rig.unit, "active", "running");
        let spy = Spy::new(&rig);
        *spy.version.borrow_mut() = Ok(LISTS.to_owned());
        setup(&spy);
        let done = spy.prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{name}: {done:?}");
        assert_eq!(done.report.detail, detail, "{name}");
        let record = rig.record().unwrap();
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
    assert_eq!(cutover.act_names(), [stop(), retire(), start()]);
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
    assert!(!file_of(&rig).exists());

    rig.set_leases(0);
    let rollback = with_before(&rig);
    assert_eq!(rollback.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(rollback.act_names(), [stop(), start()]);
    assert!(!file_of(&rig).exists());
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

/// Runs `step` of the hook as hostkeeper does, through its command line (`js-agent update-hook
/// <step> [<staged>] --params <file>`), the step's body over `body`: the exit status, what the
/// hook prints on its standard output (the report line) and on its standard error (the journal
/// line).
fn command(
    rig: &Rig,
    file: &std::path::Path,
    step: &str,
    body: &dyn Fn(&Invocation, &Params) -> js_agent::update_hook::Done,
) -> (String, serde_json::Value, String) {
    let maintain = |_: &MaintainInvocation, _: &DrainParams| -> js_agent::update_hook::Done {
        unreachable!("an update step ran a maintenance step")
    };
    let staged = rig.params.staged_release.clone();
    let mut args: Vec<OsString> = vec!["js-agent".into(), VERB.into(), step.into()];
    if step == "prepare" {
        args.push(staged.into_os_string());
    }
    args.extend(["--params".into(), file.as_os_str().to_owned()]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        &args,
        &mut out,
        &mut err,
        &Host {
            step: body,
            maintain: &maintain,
        },
    );
    let out = String::from_utf8(out).unwrap();
    let report = serde_json::from_str(out.trim_end()).unwrap();
    (format!("{code:?}"), report, String::from_utf8(err).unwrap())
}

/// Asserts a Mac hand-over driven through the hook's command line, as hostkeeper runs it, over
/// the launchd fakes: `prepare`, `cutover`, `verify` and `rollback` each exit 0 with the report
/// line hostkeeper reads (class, reversibility and the hand-over note for `prepare`), and
/// journal the same; the params the hook parsed from its file name the native runtime; the
/// record on disk says `handover` and no cordon after `prepare`; the cutover and the rollback
/// each write the request, boot the agent out, take the request back and start the other agent,
/// with the jobs running throughout and the previous agent back at the end. Catches a hand-over
/// that holds when a step is called directly but not through the entry hostkeeper runs (the
/// params read from the file, the step chosen from the command line, the report written).
#[test]
fn a_mac_hand_over_driven_through_the_hooks_command_line_exits_0_and_reports_each_step() {
    let rig = Rig::mac("hook-native-command");
    rig.world.seed_unit(&rig.unit, "active", "running");
    rig.set_leases(2);
    let file = rig.tmp.path().join("params-for-the-verb.toml");
    std::fs::write(&file, &rig.params_text).unwrap();
    let acts = RefCell::new(Vec::new());
    let body = |invocation: &Invocation, params: &Params| {
        assert_eq!(params.settings.runtime, "native");
        assert_eq!(params.settings.unit, MAC_UNIT);
        let spy = spy(&rig);
        let done = match invocation.step {
            Step::Prepare => spy.prepare(&rig),
            Step::Cutover => spy.cutover(&rig),
            Step::Verify => spy.verify(&rig),
            Step::Rollback => spy.rollback(&rig),
            other => unreachable!("{other:?} is not a step of this update"),
        };
        rig.kill.disarm();
        acts.borrow_mut().push(spy.act_names());
        done
    };
    let success = format!("{:?}", ExitCode::from(0));

    let (code, report, journal) = command(&rig, &file, "prepare", &body);
    assert_eq!(code, success);
    assert_eq!(report["detail"], HANDOVER, "{report}");
    assert_eq!(report["reversible"], true, "{report}");
    assert!(report.get("reason").is_none(), "{report}");
    assert_eq!(
        journal,
        format!("js-agent update-hook: prepare succeeded: {HANDOVER}\n")
    );
    let record = rig.record().unwrap();
    assert!(record.handover);
    assert!(!record.in_place);
    assert!(!record.cordon.placed);
    assert_eq!(rig.cordon_expiry(), None);

    let (code, report, _) = command(&rig, &file, "cutover", &body);
    assert_eq!(code, success, "{report}");
    assert_eq!(report["reversible"], false, "the boot-out is the commit");
    assert_eq!(
        acts.borrow().last().unwrap(),
        &[REQUEST, &stop(), WITHDRAW, &retire(), &start()]
    );
    assert!(!file_of(&rig).exists());
    assert_eq!(rig.host().leases, 2, "the jobs run on");

    let (code, report, _) = command(&rig, &file, "verify", &body);
    assert_eq!(code, success, "{report}");
    let (code, report, _) = command(&rig, &file, "rollback", &body);
    assert_eq!(code, success, "{report}");
    assert_eq!(
        acts.borrow().last().unwrap(),
        &[REQUEST, &stop(), WITHDRAW, &start()]
    );
    assert!(!file_of(&rig).exists());
    assert_eq!(rig.host().leases, 2, "the jobs run on");
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    assert!(rig.record().is_none(), "the rollback finished");
    assert_eq!(rig.cordon_expiry(), None, "no cordon at any step");
}
