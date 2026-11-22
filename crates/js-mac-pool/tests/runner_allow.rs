//! The allow-list read model under the real `sandbox-exec` on a macOS runner, as the
//! runner's own user: the start-up self-test against a fixture store and the host's
//! Xcode, a toolchain run of a bundle entrypoint, the open model as the control that the
//! canaries are readable, and the setuid golden list against the host's image. The
//! parts that need a pool user are in `privileged.rs`.

#![cfg(target_os = "macos")]

mod common;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use common::mac::{DATA, Mac, describe, host_major, outcomes, run_steps};
use common::{Scratch, run};
use js_bundle::ReadGrantKind;
use js_mac_pool::seatbelt::{
    Caps, Class, MacosMajor, Net, Outcome, Policy, Profile, ReadGrant, ReadModel, Role, SelfTest,
    ToolPath, Why,
};

fn mac(tag: &str) -> Mac {
    let scratch = Scratch::new(tag);
    let temp = Command::new("/usr/bin/getconf")
        .arg("DARWIN_USER_TEMP_DIR")
        .output()
        .expect("getconf");
    assert!(temp.status.success(), "getconf DARWIN_USER_TEMP_DIR");
    let temp = PathBuf::from(String::from_utf8(temp.stdout).expect("utf8").trim());
    let entry = temp.canonicalize().expect("the user temp directory exists");
    let uid_entry = entry.parent().expect("the user entry").to_owned();
    Mac::build(scratch, uid_entry, tag, common::owner())
}

fn major() -> MacosMajor {
    host_major()
}

#[test]
fn the_self_test_passes_on_this_host_with_a_fixture_tree_and_the_real_xcode() {
    let fixture = mac("selftest");
    let test = SelfTest::new(major(), fixture.fixture(major().scan())).expect("steps");
    let ran = run_steps(&test, None);
    let results = outcomes(&ran);
    if let Err(failures) = test.judge(&results) {
        let shown: Vec<String> = failures.iter().map(ToString::to_string).collect();
        panic!("{}\n{}", shown.join("\n"), describe(&test, &ran));
    }
    let classes: Vec<Class> = test.steps().iter().map(|step| step.class()).collect();
    assert!(classes.contains(&Class::Compile));
    assert!(classes.contains(&Class::Codesign));
}

/// The control of the controls: under the open model every canary is readable, so the
/// self-test refuses an open profile in every class.
#[test]
fn the_open_model_leaves_every_canary_readable_and_the_self_test_refuses_it() {
    let fixture = mac("open");
    let test = SelfTest::new(major(), fixture.fixture(major().scan())).expect("steps");
    let open = Policy::base(
        fixture.layout(),
        Caps {
            net: Net::Off,
            read_model: ReadModel::Open,
        },
    )
    .expect("policy");
    let open = Profile::for_policy(&open).expect("profile");
    let results: Vec<Outcome> = test
        .steps()
        .iter()
        .map(|step| match step.role() {
            Role::Denied => {
                let (program, args) = step.command();
                let argv = open.argv(program, args);
                run(&argv, step.env(), Some(step.cwd()), None).outcome()
            }
            Role::Control | Role::Allowed => {
                run(&step.argv(), step.env(), Some(step.cwd()), None).outcome()
            }
        })
        .collect();
    let failures = test
        .judge(&results)
        .expect_err("an open profile is refused");
    let readable: Vec<Class> = failures
        .iter()
        .filter(|failure| failure.why == Why::Readable)
        .map(|failure| failure.class)
        .collect();
    assert_eq!(
        readable,
        [
            Class::ReadOtherHome,
            Class::ReadInputStore,
            Class::ReadBundleTreeNotPinned,
            Class::ReadUsersShared,
            Class::ReadPrivateVarTmp,
            Class::ReadSystemVolumesData,
            Class::ReadSiblingRun,
            Class::ReadKeychain,
            Class::ReadSetuidBinary,
            Class::ListDeniedDir,
            Class::ListAncestor,
            Class::StatCanary,
            Class::XattrCanary,
        ]
    );
}

#[test]
fn a_bundle_entrypoint_runs_under_the_allow_profile_and_reads_only_its_pinned_tree() {
    let fixture = mac("toolchain");
    let test = SelfTest::new(major(), fixture.fixture(major().scan())).expect("steps");
    let ran = run_steps(&test, None);
    let results = outcomes(&ran);
    let assurance = test
        .judge(&results)
        .unwrap_or_else(|failures| panic!("{failures:?}\n{}", describe(&test, &ran)));

    let mut grants = fixture.grants();
    grants.push(ReadGrant::Kind(ReadGrantKind::Timezone));
    let policy = Policy::base(
        fixture.layout(),
        Caps {
            net: Net::Off,
            read_model: ReadModel::Allow,
        },
    )
    .expect("policy")
    .allowing(&assurance, grants)
    .expect("granted");
    let profile = Profile::for_policy(&policy).expect("profile");

    let pinned = vec![fixture.pinned.clone()];
    let entrypoints = BTreeMap::from([("read".to_owned(), fixture.pinned_file("bin/read"))]);
    let tool = ToolPath::from_entrypoint("@bundle/read", &entrypoints, &pinned).expect("tool");

    let data = vec![fixture.pinned_file("data.txt").into_os_string()];
    let cwd = fixture.run.to_str().expect("text");
    let under = run(&profile.argv(tool.path(), &data), &[], Some(cwd), None);
    assert!(under.ok, "{}", under.stderr);
    assert_eq!(under.stdout, DATA);
    let control = run(
        &[OsString::from(tool.path().as_str()), data[0].clone()],
        &[],
        Some(cwd),
        None,
    );
    assert_eq!(control.stdout, DATA);

    let secret = vec![fixture.unpinned_file().into_os_string()];
    let control = run(
        &[OsString::from(tool.path().as_str()), secret[0].clone()],
        &[],
        Some(cwd),
        None,
    );
    assert!(
        control.ok,
        "the control reads the unpinned file: {}",
        control.stderr
    );
    let refused = run(&profile.argv(tool.path(), &secret), &[], Some(cwd), None);
    assert!(
        !refused.ok,
        "the allow profile let a run read an unpinned tree"
    );
}

/// Every setuid and setgid file in the places the base set grants wholesale is in the
/// golden list, or a pool user could run it. The self-test refuses the host otherwise.
#[test]
fn the_setuid_golden_list_covers_the_privileged_files_of_this_host() {
    let major = major();
    let listed = major.base().setuid();
    let found = major.scan();
    let missing: Vec<&String> = found
        .paths()
        .iter()
        .filter(|path| !listed.contains(&path.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "privileged files not in the golden list: {missing:?}\nfound: {found:?}"
    );
}
