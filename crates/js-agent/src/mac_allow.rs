//! The allow-list read model of native (macOS) runs, `--read-model allow`: the parts that need
//! no Mac to decide, built and tested on every platform; the native runtime
//! (`native/allow.rs`) calls them.
//!
//! Under the model the profile of a run that pins a bundle is an allow-list
//! ([`ReadModel::Allow`](js_mac_pool::seatbelt::ReadModel::Allow)): it reads the base set of the
//! host's macOS version, its own run directory, its mounts and run files, and the grants of
//! [`grants`] (the bundle's trees, the Xcode the start names, the kinds the bundle declares),
//! and nothing else. A run that pins no bundle keeps the open model. The profile is the
//! security property under this model and is only as right as it is for the macOS build that
//! runs it, so the runtime builds one only with the [`Assurance`] of the start-up self-test
//! ([`SelfTest`]), which it runs at every start:
//!
//! - [`major_of`] reads the host's macOS version; a version without a base set refuses the
//!   model, and the agent does not start.
//! - [`plan`] lays the fixture out in the home of an account the pool lent for the self-test,
//!   outside its run directory: a store with a tree the fixture run pins and one it does not,
//!   and a canary for each denied class. The homes are below `/Users` and the account's home
//!   is emptied when the account is retired, so nothing of the fixture outlives it but the
//!   two canaries in `/Users/Shared` and `/private/var/tmp` ([`Plan::outside`]), which the
//!   runtime removes.
//! - Each step runs as that account through the supervisor, as a job does, so it holds what a
//!   job holds (the agent's privacy grant, which a disclaimed process would not): a denial the
//!   privacy controls make for a disclaimed process cannot pass for the profile's. A control
//!   runs under [`CONTROL_PROFILE`], which denies nothing, and every other step under its own
//!   profile ([`command_of`]).
//! - [`outcome`] reads each step's end and standard error, and [`verdict`] gives the
//!   assurance or names every step that was wrong.
//!
//! The canaries of the agent's and the helper's state lie in two directories of the fixture
//! that its layout adds to the agent's read denies: the agent's own roots are closed to a pool
//! account by their modes, so a canary there would fail its control and prove nothing.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use js_bundle::{BundleConfig, DiffId, ReadGrantKind};
use js_mac_pool::seatbelt::{
    AbsPath, Assurance, Canaries, Fixture, MacosMajor, Outcome, PinnedTree, PrivilegedScan,
    ReadGrant, STUB_SOURCE, SelfTest, SlotLayout, Step,
};
use js_mac_pool::xcode::HostXcode;

use crate::facts::CommandRunner;
use crate::mac_supervisor::Ended;

/// The profile a control step runs under: it denies nothing, so the step reads what the
/// account may read and nothing else differs from the same step under the allow profile.
pub const CONTROL_PROFILE: &str = "(version 1)\n(allow default)\n";

/// The directory of the fixture, in the home of the account lent for the self-test.
pub const FIXTURE_DIR: &str = "self-test";

/// What every canary holds.
pub const CANARY: &str = "canary\n";

/// The name of the file of the pinned tree that the granted step reads.
pub const GRANTED_FILE: &str = "granted";

/// What the granted file holds.
pub const GRANTED: &str = "granted\n";

/// What the Xcode steps compile.
pub const STUB: &str = "int main(void) { return 0; }\n";

/// The prefix of the names of the canaries in the system's shared places.
pub const OUTSIDE_PREFIX: &str = "js-agent-self-test-";

/// Where every path of the data volume has a second spelling.
const DATA_VOLUME: &str = "/System/Volumes/Data";

/// The program that prints the product version of macOS.
const SYSCTL: &str = "/usr/sbin/sysctl";

/// The tree the fixture run pins, and the one it does not.
const PINNED: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const UNPINNED: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// The places the fixture is laid out in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Places {
    /// The home of the account lent for the self-test, below the pool's homes (`/Users`).
    pub home: PathBuf,
    /// The account's run directory, `<home>/exec`: the fixture run's.
    pub run: PathBuf,
    /// `/Users/Shared` on a Mac.
    pub shared: PathBuf,
    /// `/private/var/tmp` on a Mac.
    pub var_tmp: PathBuf,
    /// A name of this start's own (the runtime's term), so the canaries in the shared places
    /// are this start's.
    pub tag: String,
}

/// One thing the fixture needs on disk, made by root: a directory (`0755`; its parent is
/// there) or a file (`0644`) with its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plant {
    /// A directory, made in a parent that is there.
    Dir(PathBuf),
    /// A file and what it holds.
    File(PathBuf, &'static str),
}

/// The fixture of one self-test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// What [`SelfTest::new`] takes.
    pub fixture: Fixture,
    /// What to make before the steps run, in order: a directory before what it holds.
    pub plants: Vec<Plant>,
    /// The canaries outside the account's home, removed after the steps.
    pub outside: Vec<PathBuf>,
}

/// The macOS version of this host whose base set the profiles get, read with `sysctl -n
/// kern.osproductversion`.
///
/// # Errors
///
/// `sysctl` cannot be run, or the version has no base set (the text names the versions that
/// have one): the host refuses the allow model.
pub fn major_of(runner: &dyn CommandRunner) -> Result<MacosMajor, String> {
    let text = runner.run(SYSCTL, &["-n", "kern.osproductversion"])?;
    MacosMajor::from_product_version(&text).map_err(|why| why.to_string())
}

/// The fixture of the self-test in `places`, for runs whose read denies are `read_denies`
/// (the agent's), granting the fixture run its pinned tree, every kind of the closed
/// vocabulary (the widest grant a bundle can ask, so each canary is tested against it) and
/// `xcode` when the host has one (the self-test then compiles and signs a stub, placed in the
/// run directory). `scan` is the host's privileged files ([`MacosMajor::scan`], run as root).
///
/// # Errors
///
/// A place the profile cannot name.
pub fn plan(
    places: &Places,
    read_denies: &[AbsPath],
    xcode: Option<HostXcode>,
    scan: PrivilegedScan,
) -> Result<Plan, String> {
    let abs = |path: &Path| AbsPath::new(path).map_err(|why| format!("{}: {why}", path.display()));
    let fixture = places.home.join(FIXTURE_DIR);
    let store = abs(&fixture.join("store"))?;
    let pinned = tree(&store, PINNED)?;
    let unpinned = tree(&store, UNPINNED)?;
    let pinned_dir = PathBuf::from(pinned.path().as_str());
    let unpinned_dir = PathBuf::from(unpinned.path().as_str());
    let canary = |dir: &str| fixture.join(dir).join("canary");
    let (agent_state, helper_state) = (fixture.join("agent-state"), fixture.join("helper-state"));
    let shared = places
        .shared
        .join(format!("{OUTSIDE_PREFIX}{}", places.tag));
    let var_tmp = places
        .var_tmp
        .join(format!("{OUTSIDE_PREFIX}{}", places.tag));
    let on_data = canary("data");
    let spelled = format!("{DATA_VOLUME}{}", on_data.display());
    let canaries = Canaries {
        other_home: abs(&canary("home"))?,
        input_store: abs(&canary("inputs"))?,
        unpinned_tree: abs(&unpinned_dir.join("canary"))?,
        users_shared: abs(&shared)?,
        private_var_tmp: abs(&var_tmp)?,
        system_volumes_data: abs(Path::new(&spelled))?,
        sibling_run: abs(&fixture.join("sibling/exec/canary"))?,
        agent_state: abs(&agent_state.join("canary"))?,
        helper_state: abs(&helper_state.join("canary"))?,
    };
    let mut denies = read_denies.to_vec();
    denies.push(abs(&agent_state)?);
    denies.push(abs(&helper_state)?);
    let layout = SlotLayout {
        run: abs(&places.run)?,
        uid_entry: None,
        inputs: Vec::new(),
        read_denies: denies,
        mounts: Vec::new(),
        trees: None,
    };
    let leaves = [
        "home",
        "inputs",
        "data",
        "sibling/exec",
        "agent-state",
        "helper-state",
    ]
    .map(|dir| fixture.join(dir));
    let mut plants: Vec<Plant> = Vec::new();
    for leaf in leaves.iter().chain([&pinned_dir, &unpinned_dir]) {
        for dir in below(&places.home, leaf) {
            if !plants.contains(&Plant::Dir(dir.clone())) {
                plants.push(Plant::Dir(dir));
            }
        }
    }
    let files = [
        &canaries.other_home,
        &canaries.input_store,
        &canaries.unpinned_tree,
        &canaries.sibling_run,
        &canaries.agent_state,
        &canaries.helper_state,
        &canaries.users_shared,
        &canaries.private_var_tmp,
    ];
    plants.extend(files.map(|path| Plant::File(PathBuf::from(path.as_str()), CANARY)));
    plants.push(Plant::File(on_data, CANARY));
    let granted = pinned_dir.join(GRANTED_FILE);
    plants.push(Plant::File(granted.clone(), GRANTED));
    let mut grants = vec![ReadGrant::Tree(pinned)];
    grants.extend(ReadGrantKind::ALL.map(ReadGrant::Kind));
    if let Some(xcode) = xcode {
        plants.push(Plant::File(places.run.join(STUB_SOURCE), STUB));
        grants.push(ReadGrant::Xcode(xcode));
    }
    Ok(Plan {
        fixture: Fixture {
            layout,
            grants,
            granted_file: abs(&granted)?,
            canaries,
            privileged_found: scan,
        },
        plants,
        outside: vec![shared, var_tmp],
    })
}

/// The directories from just below `top` down to `leaf`, in that order: each one the fixture
/// makes, so each gets its mode whatever the umask.
fn below(top: &Path, leaf: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = leaf
        .ancestors()
        .take_while(|dir| *dir != top)
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    dirs
}

/// The tree of `digest` in the fixture's store.
fn tree(store: &AbsPath, digest: &str) -> Result<PinnedTree, String> {
    // The digests are the fixture's constants, so parsing them does not fail; the conversion
    // is a function, not a closure, so no code is left that no input can reach.
    DiffId::parse(digest)
        .map_err(ToString::to_string)
        .and_then(|diff_id| PinnedTree::new(store, &diff_id).map_err(|why| why.to_string()))
        .map_err(|why| format!("{digest}: {why}"))
}

/// Makes each of `plants`, in order: directories `0755`, files `0644` (each new: a path that is
/// there, a link included, is refused), owned by the caller (root, in the agent), so the account
/// may read each but change none.
///
/// # Errors
///
/// The first that could not be made, naming it.
pub fn place(plants: &[Plant]) -> Result<(), String> {
    for plant in plants {
        let (path, made) = match plant {
            Plant::Dir(path) => (path, make_dir(path)),
            Plant::File(path, text) => (path, make_file(path, text)),
        };
        made.map_err(|why| format!("{}: {why}", path.display()))?;
    }
    Ok(())
}

fn make_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

/// A new file: one that is there, a link included, is refused rather than followed, since two
/// of the canaries lie in folders every user may write.
fn make_file(path: &Path, text: &str) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.set_permissions(fs::Permissions::from_mode(0o644))
}

/// Removes each of `outside` that is there, and says which could not be removed.
#[must_use]
pub fn remove(outside: &[PathBuf]) -> Vec<String> {
    outside
        .iter()
        .filter_map(|path| match fs::remove_file(path) {
            Err(why) if why.kind() != io::ErrorKind::NotFound => {
                Some(format!("{}: {why}", path.display()))
            }
            _ => None,
        })
        .collect()
}

/// What the supervisor runs for one step: the profile it applies (the step's own, or
/// [`CONTROL_PROFILE`] for a control), with its parameters, then the program and its
/// arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepCommand {
    /// The profile's text.
    pub profile: String,
    /// The profile's parameters, name and value, in order.
    pub params: Vec<(String, String)>,
    /// The program, an absolute path.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
}

/// The command of `step`. Its arguments are the fixture's paths, which are text
/// ([`AbsPath`]), so none is changed.
#[must_use]
pub fn command_of(step: &Step) -> StepCommand {
    let (program, args) = step.command();
    let args = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let (profile, params) = match step.profile() {
        Some(profile) => (profile.text().to_owned(), profile.params().to_vec()),
        None => (CONTROL_PROFILE.to_owned(), Vec::new()),
    };
    StepCommand {
        profile,
        params,
        program: program.to_owned(),
        args,
    }
}

/// What a step did, from how its supervised start `ended` and what it wrote to its standard
/// error: an exit is read as [`Outcome::from_exit`] reads it; a signal, or a setup step of
/// the supervisor that failed (the profile did not apply, the program did not start), is
/// neither a success nor the kernel's refusal.
#[must_use]
pub fn outcome(ended: &Ended, stderr: &str) -> Outcome {
    match ended {
        Ended::Exited(code) => Outcome::from_exit(Some(*code), stderr),
        Ended::Signalled(_) | Ended::SetupFailed { .. } => Outcome::Failed,
    }
}

/// The assurance of `test` when every step did what its role says (`outcomes[i]` is what
/// step `i` did).
///
/// # Errors
///
/// Every step that was wrong, in step order, each as `class: why`.
pub fn verdict(test: &SelfTest, outcomes: &[Outcome]) -> Result<Assurance, String> {
    test.judge(outcomes).map_err(|failures| {
        let each: Vec<String> = failures.iter().map(ToString::to_string).collect();
        format!(
            "the allow read model's self-test failed: {}",
            each.join("; ")
        )
    })
}

/// The ready Xcode of build `build` among `hosts`, for a start that names one; none for a start
/// that names none.
///
/// # Errors
///
/// No ready Xcode of that build is among `hosts`, so the profile could not grant it.
pub fn xcode_of(hosts: &[HostXcode], build: Option<&str>) -> Result<Option<HostXcode>, String> {
    let Some(build) = build else {
        return Ok(None);
    };
    match hosts.iter().find(|host| host.build() == build) {
        Some(host) => Ok(Some(host.clone())),
        None => Err(format!(
            "the Xcode of build {build} is not ready for the allow read model's grants"
        )),
    }
}

/// The read grants of a run that pins the bundle of `config` in the store at `store_root`:
/// each of the bundle's trees, each kind its config declares, and `xcode`.
///
/// # Errors
///
/// A store root or a tree the profile cannot name.
pub fn grants(
    store_root: &Path,
    config: &BundleConfig,
    xcode: Option<HostXcode>,
) -> Result<Vec<ReadGrant>, String> {
    let store = AbsPath::new(store_root)
        .map_err(|why| format!("the bundle store {}: {why}", store_root.display()))?;
    let mut grants = Vec::new();
    for layer in &config.layers {
        let tree = PinnedTree::new(&store, &layer.diff_id)
            .map_err(|why| format!("the tree {}: {why}", layer.tree.as_str()))?;
        grants.push(ReadGrant::Tree(tree));
    }
    grants.extend(config.seatbelt.reads.iter().copied().map(ReadGrant::Kind));
    grants.extend(xcode.map(ReadGrant::Xcode));
    Ok(grants)
}
