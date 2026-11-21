//! The host setup of a Mac run as the updater runs it: `js-agent update-hook prepare <staged>
//! --params <file>`, with a params file that names `socket_group` and `scratch_root`, through
//! [`run`] and the launchd ports of the spy. The setup's own cases are in steps_host_setup.rs.

use std::ffi::OsString;
use std::fs;
use std::process::ExitCode;

use js_agent::update_hook::step::{self, DSEDITGROUP};
use js_agent::update_hook::{
    Class, Done, DrainParams, Host, Invocation, MaintainInvocation, Outcome, Params, Reason,
    Report, Step, VERB, run,
};

use super::fake_mac::{FIRST_GID, FakeMac, GROUP, mac_rig, spy_on};
use super::step_rig::{RETRY_AFTER_SECS, Rig, Spy, as_root};
use super::steps_host_setup::{assert_no_table, first_changes};

/// How the hook ended: its exit status, its last line and what it told a person.
struct Ran {
    status: String,
    out: String,
    err: String,
}

/// The maintenance steps of an update: none runs.
fn no_maintain(_: &MaintainInvocation, _: &DrainParams) -> Done {
    unreachable!("a step of an update ran the maintenance steps")
}

/// Writes the rig's params text to a params file and runs `update-hook prepare` on it, the
/// staged release as its artifact, with the step's ports those of `spy`.
fn prepare_by_verb(rig: &Rig, spy: &Spy<'_>) -> Ran {
    let file = rig.tmp.path().join("params.toml");
    fs::write(&file, &rig.params_text).unwrap();
    let step = |invocation: &Invocation, params: &Params| {
        assert_eq!(invocation.step, Step::Prepare);
        assert_eq!(*params, rig.params);
        step::prepare(&spy.env(rig, &as_root), params)
    };
    let args: [OsString; 6] = [
        "js-agent".into(),
        VERB.into(),
        "prepare".into(),
        rig.params.staged_release.clone().into(),
        "--params".into(),
        file.into(),
    ];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let host = Host {
        step: &step,
        maintain: &no_maintain,
    };
    let code = run(&args, &mut out, &mut err, &host);
    Ran {
        status: format!("{code:?}"),
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

/// Asserts that a first install run by the verb on a params file naming the socket group and
/// the scratch root makes the group, the scratch volume, its mount and its ownership, writes no
/// file system table, writes the record, and exits 0 with a last line that names no reason.
#[test]
fn prepare_by_the_verb_sets_up_the_mac_its_params_file_names() {
    let rig = mac_rig("hook-setup-verb", false);
    let mac = FakeMac::new();
    let spy = spy_on(&rig, &mac);
    let ran = prepare_by_verb(&rig, &spy);
    assert_eq!(
        ran.status,
        format!("{:?}", ExitCode::from(0)),
        "{}",
        ran.err
    );
    let line: serde_json::Value = serde_json::from_str(ran.out.trim_end()).unwrap();
    assert!(line.get("reason").is_none(), "{}", ran.out);
    assert!(!ran.err.contains("deferred"), "{}", ran.err);
    assert_eq!(mac.groups.borrow().get(GROUP), Some(&FIRST_GID));
    assert_eq!(mac.changes(), first_changes(&rig));
    assert_no_table(&rig);
    assert!(rig.params.state_root.is_dir());
    assert!(rig.record().is_some());
}

/// Asserts that a setup that fails under the verb exits 75 (deferred), prints a last line with
/// `host_setup_missing`, the environment class, the retry hint and the detail, tells a person
/// the same reason and detail, and writes no record.
#[test]
fn prepare_by_the_verb_defers_host_setup_missing_when_the_group_cannot_be_made() {
    let rig = mac_rig("hook-setup-verb-deferred", false);
    let mac = FakeMac::new();
    let create = format!("{DSEDITGROUP} -o create {GROUP}");
    mac.answer(&create, Err("eDSPermissionError"));
    let spy = spy_on(&rig, &mac);
    let ran = prepare_by_verb(&rig, &spy);
    let detail = format!("{create}: eDSPermissionError");
    let report = Report {
        class: Class::Environment,
        reason: Some(Reason::HostSetupMissing),
        retry_after_secs: Some(RETRY_AFTER_SECS),
        detail: Some(detail.clone()),
        ..Report::default()
    };
    assert_eq!(
        ran.status,
        format!("{:?}", ExitCode::from(Outcome::Deferred.code()))
    );
    assert_eq!(Outcome::Deferred.code(), 75);
    assert_eq!(ran.out, format!("{}\n", report.to_line()));
    let line: serde_json::Value = serde_json::from_str(ran.out.trim_end()).unwrap();
    assert_eq!(line["reason"], "host_setup_missing");
    assert_eq!(
        ran.err,
        format!("js-agent update-hook: prepare deferred: host_setup_missing: {detail}\n")
    );
    assert!(rig.record().is_none());
    assert!(mac.changes().is_empty());
}

/// Asserts that a setup call the system refuses under the verb exits 1 (failed, so the order
/// fails and an operator is told) rather than 75, prints a last line with `host_setup_refused`,
/// the environment class, no retry hint and the path and error, tells a person the same, and
/// writes no record.
#[test]
fn prepare_by_the_verb_fails_host_setup_refused_when_the_system_refuses_a_call() {
    let rig = mac_rig("hook-setup-verb-refused", false);
    let mac = FakeMac::new();
    let state = rig.params.state_root.clone();
    mac.look_refused.borrow_mut().insert(state.clone());
    let spy = spy_on(&rig, &mac);
    let ran = prepare_by_verb(&rig, &spy);
    let detail = format!("{}: Operation not permitted (os error 1)", state.display());
    let report = Report {
        class: Class::Environment,
        ..Report::failure(Reason::HostSetupRefused, detail.clone())
    };
    assert_eq!(Outcome::Failure.code(), 1);
    assert_eq!(
        ran.status,
        format!("{:?}", ExitCode::from(Outcome::Failure.code()))
    );
    assert_eq!(ran.out, format!("{}\n", report.to_line()));
    let line: serde_json::Value = serde_json::from_str(ran.out.trim_end()).unwrap();
    assert_eq!(line["reason"], "host_setup_refused");
    assert!(line.get("retry_after_secs").is_none(), "{}", ran.out);
    assert_eq!(
        ran.err,
        format!("js-agent update-hook: prepare failed: host_setup_refused: {detail}\n")
    );
    assert!(rig.record().is_none());
}
