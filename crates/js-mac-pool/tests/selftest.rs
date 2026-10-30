//! The start-up self-test: its steps, what it refuses to build, and how it judges the
//! exit statuses. The test fails closed: a readable canary, a broken control, a blocked
//! toolchain step or a missing result all refuse the assurance.

mod common;

use std::ffi::OsString;

use common::{Scratch, canaries, fixture, path, tree, xcode_host};
use js_bundle::ReadGrantKind;
use js_mac_pool::seatbelt::{
    AbsPath, Class, Failure, MacosMajor, Outcome, PolicyError, ReadGrant, Role, SANDBOX_EXEC,
    SelfTest, SelfTestError, Step, Why,
};

const STORE: &str = "/Volumes/work/store";

fn grants_with_xcode(scratch: &Scratch) -> Vec<ReadGrant> {
    vec![
        ReadGrant::Tree(tree(STORE, 'a')),
        ReadGrant::Xcode(xcode_host(scratch, "Xcode_15.4.app")),
        ReadGrant::Kind(ReadGrantKind::Timezone),
    ]
}

fn selftest(grants: Vec<ReadGrant>) -> SelfTest {
    SelfTest::new(MacosMajor::Macos14, fixture(grants)).expect("steps")
}

fn names(test: &SelfTest) -> Vec<(&'static str, Role)> {
    test.steps()
        .iter()
        .map(|step| (step.class().as_str(), step.role()))
        .collect()
}

fn right(test: &SelfTest) -> Vec<Outcome> {
    test.steps()
        .iter()
        .map(|step| match step.role() {
            Role::Denied => Outcome::Refused,
            Role::Control | Role::Allowed => Outcome::Success,
        })
        .collect()
}

#[test]
fn each_denied_class_has_a_control_then_a_denied_read_and_the_toolchain_follows() {
    let scratch = Scratch::new("selftest-steps");
    let test = selftest(grants_with_xcode(&scratch));
    let denied = [
        "read-other-home",
        "read-input-store",
        "read-bundle-tree-not-pinned",
        "read-users-shared",
        "read-private-var-tmp",
        "read-system-volumes-data",
        "read-sibling-run",
        "read-agent-state",
        "read-helper-state",
        "read-keychain",
        "read-setuid-binary",
        "list-denied-dir",
        "list-ancestor",
        "stat-canary",
        "xattr-canary",
    ];
    let mut expected: Vec<(&str, Role)> = Vec::new();
    for class in denied {
        expected.push((class, Role::Control));
        expected.push((class, Role::Denied));
    }
    expected.extend([
        ("granted-tree-read", Role::Allowed),
        ("run-true", Role::Allowed),
        ("compile", Role::Allowed),
        ("codesign", Role::Allowed),
    ]);
    assert_eq!(names(&test), expected);
}

#[test]
fn without_an_xcode_there_is_no_compile_or_codesign_step() {
    let test = selftest(vec![ReadGrant::Tree(tree(STORE, 'a'))]);
    let classes: Vec<&str> = names(&test).iter().map(|(class, _)| *class).collect();
    assert!(classes.contains(&"run-true"));
    assert!(!classes.contains(&"compile"));
    assert!(!classes.contains(&"codesign"));
    assert_eq!(test.steps().len(), 32);
}

#[test]
fn controls_run_unsandboxed_and_the_rest_under_the_allow_profile() {
    let scratch = Scratch::new("selftest-argv");
    let test = selftest(grants_with_xcode(&scratch));
    for step in test.steps() {
        assert_eq!(step.cwd(), "/Volumes/work/slot1/run", "{}", step.class());
        let argv = step.argv();
        match step.role() {
            Role::Control => {
                let programs = ["/bin/cat", "/bin/ls", "/usr/bin/stat", "/usr/bin/xattr"];
                assert!(programs.iter().any(|p| argv[0] == *p), "{}", step.class());
            }
            Role::Denied | Role::Allowed => {
                assert_eq!(argv[0], OsString::from(SANDBOX_EXEC), "{}", step.class());
                let text = argv
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("(deny file-read*)"), "{}", step.class());
            }
        }
    }
    let first = &test.steps()[0];
    assert_eq!(
        first.argv(),
        [
            OsString::from("/bin/cat"),
            OsString::from("/Users/other/secret")
        ]
    );
    let (program, args) = first.command();
    assert_eq!(program, "/bin/cat");
    assert_eq!(args, [OsString::from("/Users/other/secret")]);
    assert!(first.env().is_empty());
}

#[test]
fn the_xcode_steps_run_with_the_granted_developer_dir_and_the_run_directory() {
    let scratch = Scratch::new("selftest-env");
    let grants = grants_with_xcode(&scratch);
    let developer_dir = match &grants[1] {
        ReadGrant::Xcode(xcode) => xcode.developer_dir().as_str().to_owned(),
        other => panic!("{other:?}"),
    };
    let test = selftest(grants);
    let steps = test.steps();
    let compile = steps
        .iter()
        .find(|step| step.class() == Class::Compile)
        .expect("compile");
    let expected_env = vec![
        ("DEVELOPER_DIR".to_owned(), developer_dir),
        ("HOME".to_owned(), "/Volumes/work/slot1/run".to_owned()),
        ("TMPDIR".to_owned(), "/Volumes/work/slot1/run".to_owned()),
    ];
    assert_eq!(compile.env(), expected_env.as_slice());
    let (program, args) = compile.command();
    assert_eq!(program, "/usr/bin/cc");
    assert_eq!(
        args,
        [
            OsString::from("/Volumes/work/slot1/run/stub.c"),
            OsString::from("-o"),
            OsString::from("/Volumes/work/slot1/run/stub.out"),
        ]
    );
    let sign = steps
        .iter()
        .find(|step| step.class() == Class::Codesign)
        .expect("codesign");
    let (program, args) = sign.command();
    assert_eq!(program, "/usr/bin/codesign");
    assert_eq!(args.len(), 4);
    assert_eq!(sign.env(), expected_env.as_slice());
}

#[test]
fn a_run_where_every_step_does_what_its_role_says_earns_the_assurance() {
    let test = selftest(vec![]);
    let assurance = test.judge(&right(&test)).expect("passes");
    assert_eq!(assurance.major(), MacosMajor::Macos14);
}

#[test]
fn a_readable_canary_refuses_the_assurance() {
    let test = selftest(vec![]);
    let mut ran = right(&test);
    let index = test
        .steps()
        .iter()
        .position(|step| step.class() == Class::ReadUsersShared && step.role() == Role::Denied)
        .expect("a denied step");
    ran[index] = Outcome::Success;
    assert_eq!(
        test.judge(&ran),
        Err(vec![Failure {
            class: Class::ReadUsersShared,
            why: Why::Readable
        }])
    );
}

#[test]
fn a_broken_control_refuses_the_assurance_even_when_the_denial_held() {
    let test = selftest(vec![]);
    let mut ran = right(&test);
    let index = test
        .steps()
        .iter()
        .position(|step| step.class() == Class::ReadSetuidBinary && step.role() == Role::Control)
        .expect("a control");
    ran[index] = Outcome::Failed;
    assert_eq!(
        test.judge(&ran),
        Err(vec![Failure {
            class: Class::ReadSetuidBinary,
            why: Why::ControlFailed
        }])
    );
}

#[test]
fn a_blocked_toolchain_step_refuses_the_assurance() {
    let scratch = Scratch::new("selftest-blocked");
    let test = selftest(grants_with_xcode(&scratch));
    let allowed: Vec<usize> = test
        .steps()
        .iter()
        .enumerate()
        .filter(|(_, step)| step.role() == Role::Allowed)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(allowed.len(), 4);
    for index in allowed {
        let mut ran = right(&test);
        ran[index] = Outcome::Refused;
        let failures = test.judge(&ran).expect_err("refused");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].why, Why::Blocked);
        assert_eq!(failures[0].class, test.steps()[index].class());
    }
}

/// A profile `sandbox-exec` cannot compile fails every sandboxed step: a denied step
/// that failed without the kernel's refusal shows nothing, and the allowed ones are
/// blocked.
#[test]
fn a_profile_that_fails_to_start_cannot_pass_as_everything_denied() {
    let test = selftest(vec![]);
    let ran: Vec<Outcome> = test
        .steps()
        .iter()
        .map(|step| match step.role() {
            Role::Control => Outcome::Success,
            Role::Denied | Role::Allowed => Outcome::Failed,
        })
        .collect();
    let failures = test.judge(&ran).expect_err("refused");
    let no_denial = failures.iter().filter(|f| f.why == Why::NoDenial).count();
    assert_eq!(no_denial, 15);
    let blocked: Vec<Class> = failures
        .iter()
        .filter(|f| f.why == Why::Blocked)
        .map(|f| f.class)
        .collect();
    assert_eq!(blocked, [Class::GrantedTreeRead, Class::RunTrue]);
    assert_eq!(failures.len(), 17);
}

#[test]
fn a_denied_step_passes_only_when_the_kernel_refused_it() {
    let test = selftest(vec![]);
    let index = test
        .steps()
        .iter()
        .position(|step| step.class() == Class::ListAncestor && step.role() == Role::Denied)
        .expect("a denied step");
    for (outcome, why) in [
        (Outcome::Success, Why::Readable),
        (Outcome::Failed, Why::NoDenial),
    ] {
        let mut ran = right(&test);
        ran[index] = outcome;
        assert_eq!(
            test.judge(&ran),
            Err(vec![Failure {
                class: Class::ListAncestor,
                why
            }])
        );
    }
}

#[test]
fn an_outcome_is_read_from_the_exit_code_and_the_kernels_words() {
    let table = [
        (Some(0), "", Outcome::Success),
        (Some(0), "Operation not permitted", Outcome::Success),
        (Some(1), "cat: x: Operation not permitted", Outcome::Refused),
        (
            Some(1),
            "cat: x: No such file or directory",
            Outcome::Failed,
        ),
        (Some(127), "", Outcome::Failed),
        (None, "Operation not permitted", Outcome::Failed),
    ];
    for (code, stderr, expected) in table {
        assert_eq!(
            Outcome::from_exit(code, stderr),
            expected,
            "{code:?} {stderr}"
        );
    }
}

#[test]
fn missing_results_fail_and_extra_results_are_ignored() {
    let test = selftest(vec![]);
    let mut ran = right(&test);
    ran.truncate(ran.len() - 2);
    let failures = test.judge(&ran).expect_err("refused");
    assert_eq!(
        failures,
        [
            Failure {
                class: Class::GrantedTreeRead,
                why: Why::NotRun
            },
            Failure {
                class: Class::RunTrue,
                why: Why::NotRun
            },
        ]
    );
    let mut extra = right(&test);
    extra.extend([Outcome::Success, Outcome::Failed]);
    assert!(test.judge(&extra).is_ok());
    assert!(test.judge(&[]).is_err());
}

#[test]
fn every_failure_is_listed_in_step_order_and_says_why() {
    let test = selftest(vec![]);
    let failures = test
        .judge(&vec![Outcome::Refused; test.steps().len()])
        .expect_err("refused");
    let shown: Vec<String> = failures.iter().map(ToString::to_string).collect();
    assert_eq!(shown.len(), 15 + 2);
    assert_eq!(
        shown[0],
        "read-other-home: the control without the sandbox failed"
    );
    assert_eq!(shown[15], "granted-tree-read: blocked by the profile");
    assert_eq!(Why::NoDenial.to_string(), "failed without being refused");
    assert_eq!(Why::NotRun.to_string(), "no result reported");
    assert_eq!(Why::Readable.to_string(), "readable under the profile");
}

#[test]
fn a_canary_outside_its_directory_is_refused() {
    let cases: [(&str, Class, AbsPath); 4] = [
        (
            "users_shared",
            Class::ReadUsersShared,
            path("/Users/Other/secret"),
        ),
        (
            "private_var_tmp",
            Class::ReadPrivateVarTmp,
            path("/private/var/tmpx/secret"),
        ),
        (
            "system_volumes_data",
            Class::ReadSystemVolumesData,
            path("/Users/other/secret"),
        ),
        (
            "users_shared",
            Class::ReadUsersShared,
            path("/Users/Shared"),
        ),
    ];
    for (field, class, bad) in cases {
        let mut set = canaries();
        match field {
            "users_shared" => set.users_shared = bad,
            "private_var_tmp" => set.private_var_tmp = bad,
            _ => set.system_volumes_data = bad,
        }
        let mut f = fixture(vec![]);
        f.canaries = set;
        assert_eq!(
            SelfTest::new(MacosMajor::Macos14, f),
            Err(SelfTestError::CanaryMisplaced(class)),
            "{field}"
        );
    }
}

#[test]
fn a_canary_inside_a_granted_place_is_refused() {
    let scratch = Scratch::new("selftest-granted");
    let xcode_dir = match &grants_with_xcode(&scratch)[1] {
        ReadGrant::Xcode(xcode) => xcode.app().clone(),
        other => panic!("{other:?}"),
    };
    let tree_file = format!("{STORE}/v1/objects/trees/{}/f", "a".repeat(64));
    let cases = [
        (
            "/Volumes/work/slot1/run/secret".to_owned(),
            Class::ReadInputStore,
        ),
        (tree_file, Class::ReadInputStore),
        (
            format!("{}/Contents/secret", xcode_dir.as_str()),
            Class::ReadInputStore,
        ),
        (
            "/private/var/folders/ab/cdef0123/x".to_owned(),
            Class::ReadInputStore,
        ),
    ];
    for (bad, class) in cases {
        let mut f = fixture(grants_with_xcode(&scratch));
        f.canaries.input_store = path(&bad);
        assert_eq!(
            SelfTest::new(MacosMajor::Macos14, f),
            Err(SelfTestError::CanaryGranted(class)),
            "{bad}"
        );
    }
}

#[test]
fn a_policy_that_is_refused_refuses_the_steps() {
    let scratch = Scratch::new("selftest-policy");
    let two = vec![
        ReadGrant::Xcode(xcode_host(&scratch, "Xcode_15.4.app")),
        ReadGrant::Xcode(xcode_host(&scratch, "Xcode_15.3.app")),
    ];
    assert_eq!(
        SelfTest::new(MacosMajor::Macos14, fixture(two)),
        Err(SelfTestError::Policy(PolicyError::TwoXcodes))
    );
    let mut f = fixture(vec![]);
    f.layout.inputs = (0..=64)
        .map(|i| path(&format!("/Volumes/work/slot1/run/in{i}")))
        .collect();
    assert_eq!(
        SelfTest::new(MacosMajor::Macos14, f),
        Err(SelfTestError::Policy(PolicyError::TooMany("inputs")))
    );
    assert_eq!(
        SelfTestError::Policy(PolicyError::TwoXcodes).to_string(),
        "policy: more than one Xcode granted"
    );
}

#[test]
fn a_run_directory_whose_stub_path_is_too_long_refuses_the_steps() {
    let scratch = Scratch::new("selftest-long");
    let mut f = fixture(vec![ReadGrant::Xcode(xcode_host(
        &scratch,
        "Xcode_15.4.app",
    ))]);
    f.layout.run = path(&format!("/{}", "r".repeat(1019)));
    assert!(matches!(
        SelfTest::new(MacosMajor::Macos14, f),
        Err(SelfTestError::Path(_))
    ));
}

#[test]
fn class_names_are_the_logged_ones() {
    let test = selftest(vec![]);
    let seen: Vec<String> = test
        .steps()
        .iter()
        .map(|s: &Step| s.class().to_string())
        .collect();
    assert!(seen.contains(&"read-bundle-tree-not-pinned".to_owned()));
    assert!(seen.contains(&"read-system-volumes-data".to_owned()));
    assert_eq!(Class::Compile.as_str(), "compile");
    assert_eq!(Class::Codesign.as_str(), "codesign");
}

#[test]
fn every_self_test_error_says_what_is_wrong() {
    let table = [
        (
            SelfTestError::CanaryMisplaced(Class::ReadUsersShared),
            "canary read-users-shared is not under its directory",
        ),
        (
            SelfTestError::CanaryGranted(Class::ReadInputStore),
            "canary read-input-store is inside a granted place",
        ),
        (
            SelfTestError::Path(js_mac_pool::seatbelt::PathError::TooLong),
            "path: path is too long",
        ),
    ];
    for (error, text) in table {
        assert_eq!(error.to_string(), text);
    }
}

fn command_of(test: &SelfTest, class: Class) -> (String, Vec<OsString>) {
    let step = test
        .steps()
        .iter()
        .find(|step| step.class() == class && step.role() == Role::Denied)
        .expect("a denied step");
    let (program, args) = step.command();
    (program.to_owned(), args.to_vec())
}

#[test]
fn the_read_vectors_list_stat_and_read_attributes_of_the_denied_places() {
    let test = selftest(vec![]);
    let os = |text: &str| OsString::from(text);
    assert_eq!(
        command_of(&test, Class::ListDeniedDir),
        ("/bin/ls".to_owned(), vec![os("/Users/other")])
    );
    assert_eq!(
        command_of(&test, Class::ListAncestor),
        ("/bin/ls".to_owned(), vec![os("/Volumes/work/slot1")])
    );
    assert_eq!(
        command_of(&test, Class::StatCanary),
        (
            "/usr/bin/stat".to_owned(),
            vec![os("/Volumes/work/inputs/secret")]
        )
    );
    assert_eq!(
        command_of(&test, Class::XattrCanary),
        (
            "/usr/bin/xattr".to_owned(),
            vec![os("-l"), os("/Volumes/work/inputs/secret")]
        )
    );
    assert_eq!(
        command_of(&test, Class::ReadKeychain),
        (
            "/bin/cat".to_owned(),
            vec![os("/Library/Keychains/System.keychain")]
        )
    );
    assert_eq!(
        command_of(&test, Class::ReadSiblingRun),
        (
            "/bin/cat".to_owned(),
            vec![os("/Volumes/work/slot2/run/secret")]
        )
    );
    assert_eq!(
        command_of(&test, Class::ReadAgentState),
        (
            "/bin/cat".to_owned(),
            vec![os("/Volumes/work/state/agent/token")]
        )
    );
    assert_eq!(
        command_of(&test, Class::ReadHelperState),
        (
            "/bin/cat".to_owned(),
            vec![os("/Volumes/work/state/helper/db")]
        )
    );
}

#[test]
fn a_run_directory_under_the_root_has_the_root_as_its_listed_ancestor() {
    let mut f = fixture(vec![]);
    f.layout.run = path("/run");
    let test = SelfTest::new(MacosMajor::Macos14, f).expect("steps");
    assert_eq!(
        command_of(&test, Class::ListAncestor),
        ("/bin/ls".to_owned(), vec![OsString::from("/")])
    );
}

#[test]
fn privileged_files_the_base_set_does_not_list_refuse_the_self_test() {
    let mut f = fixture(vec![]);
    f.privileged_found = vec![
        "/usr/bin/su".to_owned(),
        "/usr/bin/newtool".to_owned(),
        "/sbin/ping".to_owned(),
        "/bin/other".to_owned(),
    ];
    assert_eq!(
        SelfTest::new(MacosMajor::Macos14, f),
        Err(SelfTestError::UnlistedPrivileged(vec![
            "/usr/bin/newtool".to_owned(),
            "/bin/other".to_owned()
        ]))
    );
    let mut listed = fixture(vec![]);
    listed.privileged_found = vec!["/usr/bin/su".to_owned(), "/sbin/ping".to_owned()];
    assert!(SelfTest::new(MacosMajor::Macos14, listed).is_ok());
}

#[test]
fn a_state_canary_outside_every_read_deny_is_refused() {
    let mut agent = fixture(vec![]);
    agent.canaries.agent_state = path("/Volumes/work/other/token");
    assert_eq!(
        SelfTest::new(MacosMajor::Macos14, agent),
        Err(SelfTestError::CanaryNotDenied(Class::ReadAgentState))
    );
    let mut helper = fixture(vec![]);
    helper.canaries.helper_state = path("/Volumes/work/statex/db");
    assert_eq!(
        SelfTest::new(MacosMajor::Macos14, helper),
        Err(SelfTestError::CanaryNotDenied(Class::ReadHelperState))
    );
    let mut none = fixture(vec![]);
    none.layout.read_denies.clear();
    assert_eq!(
        SelfTest::new(MacosMajor::Macos14, none),
        Err(SelfTestError::CanaryNotDenied(Class::ReadAgentState))
    );
}

#[test]
fn a_sibling_run_canary_inside_the_run_directory_is_refused() {
    let mut f = fixture(vec![]);
    f.canaries.sibling_run = path("/Volumes/work/slot1/run/other/secret");
    assert_eq!(
        SelfTest::new(MacosMajor::Macos14, f),
        Err(SelfTestError::CanaryGranted(Class::ReadSiblingRun))
    );
}

#[test]
fn the_new_error_texts_say_what_is_wrong() {
    assert_eq!(
        SelfTestError::CanaryNotDenied(Class::ReadAgentState).to_string(),
        "canary read-agent-state is not under a read deny"
    );
    assert_eq!(
        SelfTestError::UnlistedPrivileged(vec!["/x".to_owned()]).to_string(),
        "privileged files the base set does not list: [\"/x\"]"
    );
}
