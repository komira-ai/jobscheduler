//! A job start that names an Xcode build on the native runtime: refused before a user is made
//! when the runtime does not hold the build, and otherwise run with `DEVELOPER_DIR` set to the
//! `Contents/Developer` of that Xcode's app, for a plain run and for a bundle run, and never
//! through `xcode-select`.

use std::path::PathBuf;

use js_agent::lifecycle::{NativeRefusal, Runtime};
use js_agent::native::NativeSettings;
use js_agent::xcode::Xcode;
use js_agent_wire::refusal;

use super::{Rig, native};

fn xcode(build: &str, app: &str) -> Xcode {
    Xcode {
        build: build.to_owned(),
        app: PathBuf::from(app),
    }
}

/// Two installed Xcodes.
fn two() -> Vec<Xcode> {
    vec![
        xcode("16C5032a", "/Applications/Xcode-16.2.app"),
        xcode("16F6", "/Applications/Xcode.app"),
    ]
}

/// Asserts that a start naming a held build is admitted and runs with exactly one
/// `DEVELOPER_DIR`, the `Contents/Developer` of that build's app (the other Xcode's app is
/// named nowhere), after `TMPDIR`; and that the helper ran no `xcode-select` and no program
/// of the run's command line is one. It catches a job run with the other Xcode, with the
/// host's selected one, or after a global selection was changed.
#[test]
fn a_start_naming_a_held_xcode_runs_with_that_apps_developer_dir_and_selects_nothing() {
    let mut rig = Rig::new("xcode-held").holding(two());
    assert_eq!(rig.rt.check_xcode("16F6"), Ok(()));
    assert_eq!(rig.rt.check_xcode("16C5032a"), Ok(()));
    let mut spec = rig.spec("r1", native("/bin/true", &[]));
    spec.xcode = Some("16F6".to_owned());
    rig.rt.create_and_start(&spec).unwrap();
    let run = rig.fake.run_of("7.0");
    let tmp = rig.fake.home_of("7.0").join("tmp").display().to_string();
    assert_eq!(
        run.env,
        [
            ("TMPDIR".to_owned(), tmp),
            (
                "DEVELOPER_DIR".to_owned(),
                "/Applications/Xcode.app/Contents/Developer".to_owned()
            ),
        ]
    );
    let named: Vec<&(String, String)> = run
        .env
        .iter()
        .filter(|(_, value)| value.contains("Xcode-16.2.app"))
        .collect();
    assert!(named.is_empty(), "{named:?}");
    assert_eq!(
        rig.fake.verbs(),
        ["boot-reset", "user-create 7.0", "run 7.0"]
    );
    assert!(run.argv.iter().all(|arg| !arg.contains("xcode-select")));
}

/// Asserts that a start naming no build runs with no `DEVELOPER_DIR`, on a runtime that holds
/// Xcodes. It catches an Xcode selected for a job that asked for none.
#[test]
fn a_start_naming_no_xcode_runs_without_a_developer_dir() {
    let mut rig = Rig::new("xcode-none").holding(two());
    let spec = rig.spec("r1", native("/bin/true", &[]));
    rig.rt.create_and_start(&spec).unwrap();
    let run = rig.fake.run_of("7.0");
    assert!(run.env.iter().all(|(name, _)| name != "DEVELOPER_DIR"));
}

/// Asserts that a build the runtime does not hold is refused `xcode-not-held`, naming the
/// build, by a runtime that holds other Xcodes and by one that holds none, and that the
/// helper was asked nothing but its boot reset (no user was made). It catches a job started
/// on an Xcode the host lacks, and a user made for a start that was bound to fail.
#[test]
fn a_start_naming_an_xcode_the_runtime_lacks_is_refused_before_a_user_is_made() {
    let rig = Rig::new("xcode-lacks").holding(two());
    let refused = NativeRefusal {
        code: refusal::XCODE_NOT_HELD,
        why: "this agent holds no Xcode of build 99Z9".to_owned(),
    };
    assert_eq!(rig.rt.check_xcode("99Z9"), Err(refused.clone()));
    let none = Rig::new("xcode-holds-none");
    assert_eq!(none.rt.check_xcode("99Z9"), Err(refused));
    assert_eq!(
        none.rt.check_xcode("16F6").unwrap_err().code,
        "xcode-not-held"
    );
    assert_eq!(rig.fake.verbs(), ["boot-reset"]);
    assert_eq!(none.fake.verbs(), ["boot-reset"]);
}

/// Asserts that a run whose spec names a build the runtime does not hold (admission refused
/// such a start already) fails to start with the build named, before a user is made. It
/// catches a job run without the Xcode it names.
#[test]
fn a_run_naming_an_unheld_xcode_fails_before_a_user_is_made() {
    let mut rig = Rig::new("xcode-unheld-run").holding(two());
    let mut spec = rig.spec("r1", native("/bin/true", &[]));
    spec.xcode = Some("99Z9".to_owned());
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(error.0.contains("99Z9"), "{}", error.0);
    assert_eq!(rig.fake.verbs(), ["boot-reset"]);
}

/// Asserts that the settings of the native runtime hold no Xcode until given some, and then
/// the ones given. It catches Xcodes that do not reach the runtime the agent makes from them.
#[test]
fn the_native_settings_carry_the_xcodes_the_hello_listed() {
    let dir = super::Scratch::new("native-xcode-settings");
    let homes = dir.path("homes");
    std::fs::create_dir(&homes).unwrap();
    let socket = dir.path("socket");
    let flags = js_agent::native::NativeFlags {
        socket: Some(&socket),
        homes: Some(&homes),
        network: None,
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: js_agent::native::PfFlags::default(),
    };
    let settings = NativeSettings::from_flags(&flags, &[], std::path::Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
    assert!(settings.xcodes().is_empty());
    assert_eq!(settings.with_xcodes(two()).xcodes(), two());
}
