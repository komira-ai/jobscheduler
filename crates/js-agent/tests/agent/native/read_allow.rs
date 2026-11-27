//! The allow-list read model in the native runtime, over the real account pool on a fake host
//! and the fake launcher: what the self-test refuses before it runs a step, and that it leaves
//! no lease, no lent account and no canary behind. The fake applies no profile, so the steps
//! themselves are run under the real supervisor and profile in `mac.rs`.

use js_agent::mac_allow::{Places, plan, verdict};
use js_agent::native::{Journal, ReadAllow};
use js_mac_pool::seatbelt::{
    Assurance, MacosMajor, Outcome, ReadModel, Role, SelfTest, scan_privileged,
};
use std::path::PathBuf;

use super::Rig;

/// The assurance of a self-test whose every step did what its role says, on a fixture whose
/// paths are strings: what a runtime holds after its self-test passed.
pub fn assured() -> Assurance {
    let home = PathBuf::from("/Users/kbf-lease-pool-9");
    let places = Places {
        run: home.join("exec"),
        home,
        shared: PathBuf::from("/Users/Shared"),
        var_tmp: PathBuf::from("/private/var/tmp"),
        tag: "9".to_owned(),
    };
    let plan = plan(&places, &[], None, scan_privileged(&[])).unwrap();
    let test = SelfTest::new(MacosMajor::Macos15, plan.fixture).unwrap();
    let right: Vec<Outcome> = test
        .steps()
        .iter()
        .map(|step| match step.role() {
            Role::Denied => Outcome::Refused,
            Role::Control | Role::Allowed => Outcome::Success,
        })
        .collect();
    verdict(&test, &right).unwrap()
}

fn allow_rig(name: &str) -> Rig {
    Rig::with(name, |config| config.read_model = ReadModel::Allow)
}

/// Asserts that a runtime asked for the allow read model and given nothing of the host fails
/// its self-test after the run of `/usr/bin/true`, and holds no assurance.
///
/// Catches: an agent started under `--read-model allow` that never tests the profile, and
/// then runs bundles under one no self-test vouched for.
#[test]
fn the_allow_read_model_without_the_hosts_facts_fails_the_self_test() {
    let mut rig = allow_rig("allow-unset");
    let why = rig.rt.self_test().unwrap_err();
    assert_eq!(
        why.0,
        "--read-model allow: the runtime was given nothing of the host for the allow read model"
    );
    assert!(!rig.rt.read_allow_assured());
    assert_eq!(rig.launcher.launched().len(), 1, "the run of /usr/bin/true");
}

/// Asserts that a fixture the self-test refuses (here the canaries of the shared places are
/// not where their classes say) fails the self-test with the refusal before any step runs,
/// removes those canaries, retires the lent account and leaves no live lease.
///
/// Catches: an assurance given by a self-test that never ran its steps, and a self-test that
/// leaves its canaries in the shared places or an account lent.
#[test]
fn a_refused_fixture_fails_the_self_test_and_leaves_nothing() {
    let mut rig = allow_rig("allow-misplaced");
    let (shared, var_tmp) = (rig.dir.path("users-shared"), rig.dir.path("var-tmp"));
    std::fs::create_dir(&shared).unwrap();
    std::fs::create_dir(&var_tmp).unwrap();
    let allow = ReadAllow {
        major: MacosMajor::Macos15,
        xcodes: Vec::new(),
        scan: scan_privileged(&[]),
        shared: shared.clone(),
        var_tmp: var_tmp.clone(),
    };
    rig.rt = rig.rt.with_read_allow(allow);
    let why = rig.rt.self_test().unwrap_err();
    assert!(
        why.0
            .starts_with("the allow read model's self-test: canary read-")
            && why.0.ends_with(" is not under its directory"),
        "{}",
        why.0
    );
    assert!(!rig.rt.read_allow_assured());
    assert_eq!(rig.launcher.launched().len(), 1, "no step ran");
    for dir in [&shared, &var_tmp] {
        assert_eq!(
            std::fs::read_dir(dir).unwrap().count(),
            0,
            "{}",
            dir.display()
        );
    }
    assert!(rig.pool.held().is_empty(), "{:?}", rig.pool.held());
    let live = Journal::new(&rig.dir.path("native-leases")).live().unwrap();
    assert_eq!(live, Vec::<String>::new());
}
