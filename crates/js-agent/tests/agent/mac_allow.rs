//! The allow-list read model's decisions that need no Mac (`js_agent::mac_allow`): the host's
//! macOS version, the fixture of the start-up self-test and what it makes on disk, the command
//! the supervisor runs for each step, how each step's end is read and judged, and the read
//! grants of a run that pins a bundle. Every path is a string or lies in the test's scratch
//! directory; nothing here touches the host's own places.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use js_agent::mac_allow::{
    CANARY, CONTROL_PROFILE, GRANTED, Places, Plant, STUB, command_of, grants, major_of, outcome,
    place, plan, remove, verdict, xcode_of,
};
use js_agent::mac_supervisor::{Ended, Step as SetupStep};
use js_bundle::{BundleConfig, ReadGrantKind};
use js_mac_pool::seatbelt::{
    AbsPath, Class, MacosMajor, Outcome, PinnedTree, ReadGrant, Role, SelfTest, scan_privileged,
};

use crate::support::{FakeCommands, Scratch};
use crate::xcode_survey::host_xcode;

const SYSCTL: &str = "/usr/sbin/sysctl -n kern.osproductversion";
const HOME: &str = "/Users/kbf-lease-pool-3";
const FIXTURE: &str = "/Users/kbf-lease-pool-3/self-test";
const STORE_TREES: &str = "/Users/kbf-lease-pool-3/self-test/store/v1/objects/trees";
const PINNED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const UNPINNED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TWO_TREE: &str = include_str!("../../../js-bundle/tests/fixtures/two_tree.canonical.json");

fn places(home: &str) -> Places {
    Places {
        home: PathBuf::from(home),
        run: Path::new(home).join("exec"),
        shared: PathBuf::from("/Users/Shared"),
        var_tmp: PathBuf::from("/private/var/tmp"),
        tag: "7".to_owned(),
    }
}

fn path(text: &str) -> AbsPath {
    AbsPath::parse(text).unwrap()
}

fn denies() -> Vec<AbsPath> {
    vec![path("/private/var/js/state")]
}

/// What each step did when every step did what its role says.
fn right(test: &SelfTest) -> Vec<Outcome> {
    test.steps()
        .iter()
        .map(|step| match step.role() {
            Role::Denied => Outcome::Refused,
            Role::Control | Role::Allowed => Outcome::Success,
        })
        .collect()
}

fn selftest(xcode: Option<js_mac_pool::xcode::HostXcode>) -> SelfTest {
    let plan = plan(&places(HOME), &denies(), xcode, scan_privileged(&[])).unwrap();
    SelfTest::new(MacosMajor::Macos15, plan.fixture).unwrap()
}

/// The host's version is read with `sysctl`, and only a version with a base set is taken: an
/// older one, and a `sysctl` that cannot run, refuse the model with the reason. It catches a
/// host whose version has no base set given another version's, which would test and grant a
/// profile made for a different macOS.
#[test]
fn the_macos_version_is_read_and_one_without_a_base_set_is_refused() {
    let runner = FakeCommands::default().with(SYSCTL, Ok("15.8.2\n"));
    assert_eq!(major_of(&runner), Ok(MacosMajor::Macos15));
    assert_eq!(runner.ran.borrow().as_slice(), [SYSCTL]);
    let old = FakeCommands::default().with(SYSCTL, Ok("13.6\n"));
    assert_eq!(
        major_of(&old).unwrap_err(),
        "macOS \"13.6\" has no Seatbelt base set (there are sets for macOS 14, 15, 26, 27): \
         the allow read model is refused on this host"
    );
    let none = FakeCommands::default();
    assert_eq!(major_of(&none).unwrap_err(), "/usr/sbin/sysctl: not found");
}

/// The fixture lies in the lent account's home, outside its run directory: a store with the
/// pinned tree and one more, a canary per denied class, the two state canaries under denies
/// the layout adds to the agent's own, and the two canaries outside the home named for this
/// start. Every directory is made in order, each once, so each gets its mode. The self-test
/// accepts it, with every kind granted and no Xcode step without an Xcode. It catches a canary
/// inside a granted place (the self-test refuses it), a state canary outside every deny, a
/// directory left to the umask, and a canary outside the home that is never removed.
#[test]
fn the_fixture_is_laid_out_in_the_lent_home_and_the_self_test_takes_it() {
    let plan = plan(&places(HOME), &denies(), None, scan_privileged(&[])).unwrap();
    let canaries = &plan.fixture.canaries;
    let canary = |rest: &str| format!("{FIXTURE}/{rest}/canary");
    assert_eq!(canaries.other_home.as_str(), canary("home"));
    assert_eq!(canaries.input_store.as_str(), canary("inputs"));
    assert_eq!(
        canaries.unpinned_tree.as_str(),
        format!("{STORE_TREES}/{UNPINNED}/canary")
    );
    assert_eq!(
        canaries.users_shared.as_str(),
        "/Users/Shared/js-agent-self-test-7"
    );
    assert_eq!(
        canaries.private_var_tmp.as_str(),
        "/private/var/tmp/js-agent-self-test-7"
    );
    assert_eq!(
        canaries.system_volumes_data.as_str(),
        format!("/System/Volumes/Data{}", canary("data"))
    );
    assert_eq!(canaries.sibling_run.as_str(), canary("sibling/exec"));
    assert_eq!(canaries.agent_state.as_str(), canary("agent-state"));
    assert_eq!(canaries.helper_state.as_str(), canary("helper-state"));

    let layout = &plan.fixture.layout;
    assert_eq!(layout.run.as_str(), format!("{HOME}/exec"));
    assert_eq!(layout.uid_entry, None);
    assert_eq!(
        layout.read_denies,
        [
            path("/private/var/js/state"),
            path(&format!("{FIXTURE}/agent-state")),
            path(&format!("{FIXTURE}/helper-state")),
        ]
    );
    let store = path(&format!("{FIXTURE}/store"));
    let pinned = PinnedTree::new(
        &store,
        &js_bundle::DiffId::parse(&format!("sha256:{PINNED}")).unwrap(),
    )
    .unwrap();
    let mut want = vec![ReadGrant::Tree(pinned)];
    want.extend(ReadGrantKind::ALL.map(ReadGrant::Kind));
    assert_eq!(plan.fixture.grants, want);
    assert_eq!(
        plan.fixture.granted_file.as_str(),
        format!("{STORE_TREES}/{PINNED}/granted")
    );

    let dir = |rest: &str| Plant::Dir(PathBuf::from(format!("{FIXTURE}{rest}")));
    let file = |text: &str, held: &'static str| Plant::File(PathBuf::from(text), held);
    let expected = vec![
        dir(""),
        dir("/home"),
        dir("/inputs"),
        dir("/data"),
        dir("/sibling"),
        dir("/sibling/exec"),
        dir("/agent-state"),
        dir("/helper-state"),
        dir("/store"),
        dir("/store/v1"),
        dir("/store/v1/objects"),
        dir("/store/v1/objects/trees"),
        dir(&format!("/store/v1/objects/trees/{PINNED}")),
        dir(&format!("/store/v1/objects/trees/{UNPINNED}")),
        file(&canary("home"), CANARY),
        file(&canary("inputs"), CANARY),
        file(&format!("{STORE_TREES}/{UNPINNED}/canary"), CANARY),
        file(&canary("sibling/exec"), CANARY),
        file(&canary("agent-state"), CANARY),
        file(&canary("helper-state"), CANARY),
        file("/Users/Shared/js-agent-self-test-7", CANARY),
        file("/private/var/tmp/js-agent-self-test-7", CANARY),
        file(&canary("data"), CANARY),
        file(&format!("{STORE_TREES}/{PINNED}/granted"), GRANTED),
    ];
    assert_eq!(plan.plants, expected);
    assert_eq!(
        plan.outside,
        [
            PathBuf::from("/Users/Shared/js-agent-self-test-7"),
            PathBuf::from("/private/var/tmp/js-agent-self-test-7"),
        ]
    );

    let test = SelfTest::new(MacosMajor::Macos15, plan.fixture).unwrap();
    let classes: Vec<Class> = test.steps().iter().map(|step| step.class()).collect();
    assert_eq!(test.steps().len(), 32);
    assert!(!classes.contains(&Class::Compile));
}

/// With a ready Xcode the fixture grants it, places the stub the compile step builds in the
/// run directory, and the self-test compiles and signs. It catches a self-test that never
/// checks the toolchain an Xcode run needs under the profile.
#[test]
fn with_an_xcode_the_fixture_grants_it_and_places_the_stub() {
    let scratch = Scratch::new("mac-allow-xcode");
    let xcode = host_xcode(&scratch, "16C5032a");
    let plan = plan(
        &places(HOME),
        &denies(),
        Some(xcode.clone()),
        scan_privileged(&[]),
    )
    .unwrap();
    assert_eq!(plan.fixture.grants.last(), Some(&ReadGrant::Xcode(xcode)));
    assert_eq!(
        plan.plants.last(),
        Some(&Plant::File(
            PathBuf::from(format!("{HOME}/exec/stub.c")),
            STUB
        ))
    );
    let test = SelfTest::new(MacosMajor::Macos15, plan.fixture).unwrap();
    let classes: Vec<Class> = test.steps().iter().map(|step| step.class()).collect();
    assert_eq!(
        classes[classes.len() - 2..],
        [Class::Compile, Class::Codesign]
    );
}

/// A home the profile cannot name refuses the fixture, naming the path; so does a home so
/// long that a tree's directory below it passes the profile's path limit. It catches a
/// fixture built from a path no profile parameter may hold.
#[test]
fn a_home_the_profile_cannot_name_refuses_the_fixture() {
    let scan = || scan_privileged(&[]);
    let why = plan(&places("Users/relative"), &denies(), None, scan()).unwrap_err();
    assert_eq!(why, "Users/relative/self-test/store: path is not absolute");
    let long = format!("/Users/{}", "x".repeat(990));
    let why = plan(&places(&long), &denies(), None, scan()).unwrap_err();
    assert_eq!(why, format!("sha256:{PINNED}: path is too long"));
}

/// The plants are made in order, directories `0755` and files `0644` with their text, and the
/// first that cannot be made stops the placing, naming it. It catches a canary a pool account
/// could not read without the profile (its control would fail) and a fixture half made in
/// silence.
#[test]
fn the_plants_are_made_readable_and_unchangeable_and_a_failure_names_its_path() {
    let scratch = Scratch::new("mac-allow-place");
    let top = scratch.path("f");
    let plants = [
        Plant::Dir(top.clone()),
        Plant::Dir(top.join("d")),
        Plant::File(top.join("d/canary"), CANARY),
    ];
    place(&plants).unwrap();
    let mode = |at: &Path| fs::metadata(at).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&top), 0o755);
    assert_eq!(mode(&top.join("d")), 0o755);
    assert_eq!(mode(&top.join("d/canary")), 0o644);
    assert_eq!(fs::read_to_string(top.join("d/canary")).unwrap(), CANARY);

    let missing = scratch.path("none/d");
    let why = place(&[Plant::Dir(missing.clone())]).unwrap_err();
    assert!(
        why.starts_with(&format!("{}: ", missing.display())),
        "{why}"
    );
    let file = scratch.path("none/canary");
    let why = place(&[Plant::File(file.clone(), CANARY)]).unwrap_err();
    assert!(why.starts_with(&format!("{}: ", file.display())), "{why}");
}

/// The canaries outside the home are removed; one already gone is no complaint, and one that
/// cannot be removed is named. It catches a canary left in a shared place after every start.
#[test]
fn the_canaries_outside_the_home_are_removed_and_one_that_stays_is_named() {
    let scratch = Scratch::new("mac-allow-remove");
    let there = scratch.write("there", CANARY, 0o644);
    let gone = scratch.path("gone");
    let dir = scratch.path("a-dir");
    fs::create_dir(&dir).unwrap();
    let left = remove(&[there.clone(), gone, dir.clone()]);
    assert!(!there.exists());
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(
        left[0].starts_with(&format!("{}: ", dir.display())),
        "{left:?}"
    );
}

/// A control step runs under the profile that denies nothing, and every other step under its
/// own profile and parameters, each with its program and arguments as given. It catches a
/// control run under the allow profile (the denial would then prove nothing) and a denied
/// read run with no profile at all.
#[test]
fn a_control_runs_under_the_profile_that_denies_nothing_and_the_rest_under_their_own() {
    let test = selftest(None);
    for step in test.steps() {
        let command = command_of(step);
        let (program, args) = step.command();
        assert_eq!(command.program, program);
        let given: Vec<String> = args
            .iter()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect();
        assert_eq!(command.args, given);
        match step.profile() {
            None => {
                assert_eq!(step.role(), Role::Control);
                assert_eq!(command.profile, CONTROL_PROFILE);
                assert!(command.params.is_empty());
            }
            Some(profile) => {
                assert_ne!(step.role(), Role::Control);
                assert_eq!(command.profile, profile.text());
                assert_eq!(command.params, profile.params());
                assert!(command.profile.contains("(deny file-read*)"));
            }
        }
    }
    let first = command_of(&test.steps()[0]);
    assert_eq!(first.program, "/bin/cat");
    assert_eq!(first.args, [format!("{FIXTURE}/home/canary")]);
}

/// A step that exited is read by its status and its words: 0 is a success, the tool's
/// "Operation not permitted" the kernel's refusal, anything else a failure; a signal and a
/// supervisor whose setup failed are failures, never a refusal. It catches a profile that does
/// not apply (the setup fails) passing as every read denied.
#[test]
fn a_steps_end_is_read_by_its_status_and_a_setup_that_failed_is_no_refusal() {
    let refused = "cat: /x: Operation not permitted\n";
    assert_eq!(outcome(&Ended::Exited(0), ""), Outcome::Success);
    assert_eq!(outcome(&Ended::Exited(1), refused), Outcome::Refused);
    assert_eq!(
        outcome(&Ended::Exited(1), "cat: /x: No such file\n"),
        Outcome::Failed
    );
    assert_eq!(outcome(&Ended::Signalled(9), refused), Outcome::Failed);
    let setup = Ended::SetupFailed {
        step: SetupStep::Profile,
        why: "sandbox_init: Operation not permitted".to_owned(),
    };
    assert_eq!(outcome(&setup, refused), Outcome::Failed);
}

/// A self-test whose every step did what its role says gives the assurance for its version;
/// one readable canary is named with why, and no results name every step. It catches an
/// assurance given while a denied place was readable.
#[test]
fn the_verdict_is_the_assurance_or_every_step_that_was_wrong() {
    let test = selftest(None);
    let assurance = verdict(&test, &right(&test)).unwrap();
    assert_eq!(assurance.major(), MacosMajor::Macos15);
    let mut ran = right(&test);
    ran[1] = Outcome::Success;
    assert_eq!(
        verdict(&test, &ran).unwrap_err(),
        "the allow read model's self-test failed: read-other-home: readable under the profile"
    );
    let why = verdict(&test, &[]).unwrap_err();
    assert!(
        why.starts_with(
            "the allow read model's self-test failed: read-other-home: no result reported; \
             read-other-home: no result reported; read-input-store: no result reported"
        ),
        "{why}"
    );
}

/// A start that names no Xcode is granted none, one that names a ready build is granted that
/// Xcode, and one that names a build that is not ready is refused, naming it. It catches a run
/// granted an Xcode it did not name, and one run with a `DEVELOPER_DIR` its profile does not
/// let it read.
#[test]
fn the_xcode_a_start_names_is_granted_only_when_it_is_ready() {
    let scratch = Scratch::new("mac-allow-xcode-of");
    let ready = host_xcode(&scratch, "16C5032a");
    let hosts = [ready.clone()];
    assert_eq!(xcode_of(&hosts, None), Ok(None));
    assert_eq!(xcode_of(&hosts, Some("16C5032a")), Ok(Some(ready)));
    assert_eq!(
        xcode_of(&hosts, Some("16F6")),
        Err("the Xcode of build 16F6 is not ready for the allow read model's grants".to_owned())
    );
}

/// A run that pins a bundle is granted each of the bundle's trees in the store, each kind its
/// config declares, in its order, and the Xcode its start names; a store the profile cannot
/// name, and one whose trees would pass the path limit, refuse it. It catches a run granted a
/// directory of the store that holds other trees, or not granted a tree it runs from.
#[test]
fn a_bundle_run_is_granted_its_trees_its_kinds_and_its_xcode() {
    let config = BundleConfig::parse(TWO_TREE.as_bytes()).unwrap();
    let store = path("/private/var/js/bundles");
    let trees: Vec<ReadGrant> = config
        .layers
        .iter()
        .map(|layer| ReadGrant::Tree(PinnedTree::new(&store, &layer.diff_id).unwrap()))
        .collect();
    assert_eq!(trees.len(), 2);
    let mut want = trees.clone();
    want.extend([
        ReadGrant::Kind(ReadGrantKind::SystemCerts),
        ReadGrant::Kind(ReadGrantKind::Timezone),
    ]);
    let root = Path::new("/private/var/js/bundles");
    assert_eq!(grants(root, &config, None), Ok(want.clone()));

    let scratch = Scratch::new("mac-allow-grants");
    let xcode = host_xcode(&scratch, "16C5032a");
    want.push(ReadGrant::Xcode(xcode.clone()));
    assert_eq!(grants(root, &config, Some(xcode)), Ok(want));

    let why = grants(Path::new("bundles"), &config, None).unwrap_err();
    assert_eq!(why, "the bundle store bundles: path is not absolute");
    let long = PathBuf::from(format!("/{}", "s".repeat(960)));
    let why = grants(&long, &config, None).unwrap_err();
    let first = config.layers[0].tree.as_str();
    assert_eq!(why, format!("the tree {first}: path is too long"));
}
