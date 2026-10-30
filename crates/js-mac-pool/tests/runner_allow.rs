//! The allow-list read model under the real `sandbox-exec` on a macOS runner, as the
//! runner's own user: the start-up self-test against a fixture store and the host's
//! Xcode, a toolchain run of a bundle entrypoint, the open model as the control that the
//! canaries are readable, and the setuid golden list against the host's image. The
//! parts that need a pool user are in `privileged.rs`.

#![cfg(target_os = "macos")]

mod common;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::mac::{DATA, Mac, describe, run_steps};
use common::{Scratch, run};
use js_bundle::ReadGrantKind;
use js_mac_pool::seatbelt::{
    Caps, Class, MacosMajor, Net, Policy, Profile, ReadGrant, ReadModel, Role, SelfTest, ToolPath,
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
    Mac::build(scratch, uid_entry, tag)
}

fn major() -> MacosMajor {
    let out = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .expect("sw_vers");
    let version = String::from_utf8(out.stdout).expect("utf8");
    MacosMajor::from_product_version(&version)
        .unwrap_or_else(|| panic!("macOS {version} has no base set"))
}

fn argv_ok(argv: &[OsString]) -> bool {
    run(argv, &[], None).ok
}

#[test]
fn the_self_test_passes_on_this_host_with_a_fixture_tree_and_the_real_xcode() {
    let fixture = mac("selftest");
    let test = SelfTest::new(major(), fixture.fixture()).expect("steps");
    let ran = run_steps(&test, None);
    let results: Vec<bool> = ran.iter().map(|ran| ran.ok).collect();
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
    let test = SelfTest::new(major(), fixture.fixture()).expect("steps");
    let open = Policy::base(
        fixture.layout(),
        Caps {
            net: Net::Off,
            read_model: ReadModel::Open,
        },
    )
    .expect("policy");
    let open = Profile::for_policy(&open).expect("profile");
    let results: Vec<bool> = test
        .steps()
        .iter()
        .map(|step| match step.role() {
            Role::Denied => {
                let (program, args) = step.command();
                argv_ok(&open.argv(program, args))
            }
            Role::Control | Role::Allowed => argv_ok(&step.argv()),
        })
        .collect();
    let failures = test
        .judge(&results)
        .expect_err("an open profile is refused");
    let readable: Vec<Class> = failures.iter().map(|failure| failure.class).collect();
    assert_eq!(
        readable,
        [
            Class::ReadOtherHome,
            Class::ReadInputStore,
            Class::ReadBundleTreeNotPinned,
            Class::ReadUsersShared,
            Class::ReadPrivateVarTmp,
            Class::ReadSystemVolumesData,
            Class::ReadSetuidBinary,
        ]
    );
}

#[test]
fn a_bundle_entrypoint_runs_under_the_allow_profile_and_reads_only_its_pinned_tree() {
    let fixture = mac("toolchain");
    let test = SelfTest::new(major(), fixture.fixture()).expect("steps");
    let ran = run_steps(&test, None);
    let results: Vec<bool> = ran.iter().map(|ran| ran.ok).collect();
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
    let under = run(&profile.argv(tool.path(), &data), &[], None);
    assert!(under.ok, "{}", under.stderr);
    assert_eq!(under.stdout, DATA);
    let control = run(
        &[OsString::from(tool.path().as_str()), data[0].clone()],
        &[],
        None,
    );
    assert_eq!(control.stdout, DATA);

    let secret = vec![fixture.unpinned_file().into_os_string()];
    let control = run(
        &[OsString::from(tool.path().as_str()), secret[0].clone()],
        &[],
        None,
    );
    assert!(
        control.ok,
        "the control reads the unpinned file: {}",
        control.stderr
    );
    let refused = run(&profile.argv(tool.path(), &secret), &[], None);
    assert!(
        !refused.ok,
        "the allow profile let a run read an unpinned tree"
    );
}

/// Every setuid file in the places the base set grants wholesale is in the golden list,
/// or a pool user could run it.
#[test]
fn the_setuid_golden_list_covers_the_setuid_files_of_this_host() {
    let major = major();
    let listed: Vec<&str> = major.base().setuid().to_vec();
    let mut stack: Vec<PathBuf> = major
        .base()
        .reads()
        .iter()
        .map(|(_, path)| PathBuf::from(path))
        .filter(|path| path != Path::new("/") && !path.starts_with("/dev"))
        .collect();
    let mut found: Vec<String> = Vec::new();
    while let Some(path) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if meta.is_file() && meta.mode() & 0o4000 != 0 {
            found.push(path.to_string_lossy().into_owned());
        }
    }
    found.sort();
    let missing: Vec<&String> = found
        .iter()
        .filter(|path| !listed.contains(&path.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "setuid files not in the golden list: {missing:?}\nfound: {found:?}"
    );
}
