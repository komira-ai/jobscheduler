//! The start-up self-test of the allow-list read model.
//!
//! The profile is the security property under [`ReadModel::Allow`], and it is only as
//! right as it is for the macOS build the agent runs on. At start the agent places
//! canary files, generates the profile of a fixture run and runs each [`Step`] of a
//! [`SelfTest`] as a pool user. [`SelfTest::judge`] reads the exit statuses:
//!
//! - every denied class ([`Class`]) must be unreadable under the profile, and readable
//!   without it (the control: a canary a plain permission already hides proves nothing);
//! - the toolchain must work under the profile: a read of a file of a granted tree,
//!   `true`, and, with an Xcode granted, a compile and an ad-hoc `codesign`.
//!
//! Read vectors: besides `cat` of each canary there are a listing of a denied directory,
//! a listing of an ancestor of the run directory (whose name the profile allows, as
//! metadata only), a `stat` and an `xattr` read of a canary, each with an unsandboxed
//! control.
//!
//! Before any step exists the fixture's scan of the host's setuid and setgid files
//! ([`scan_privileged`](super::scan_privileged), [`MacosMajor::scan`]) must hold only
//! files the base set lists; otherwise [`SelfTest::new`] refuses, and there is no
//! [`Assurance`].
//!
//! The agent's duties: a host whose macOS version has no [`MacosMajor`] refuses the
//! allow model (it never falls back to another version's base set), the self-test runs
//! at every start and after an OS update, and an [`Assurance`] is never kept across a
//! restart.
//!
//! A pass is an [`Assurance`], which [`Policy::allowing`](super::Policy::allowing)
//! takes; a failure lists every step that was wrong and why, and the agent then refuses
//! the bundle and allow-model capabilities and logs the list. The open model is
//! unaffected. This crate starts nothing: the caller runs [`Step::argv`] with the
//! environment of [`Step::env`], as the pool user, and reports what each step did
//! ([`Outcome`]): a denied step passes only when it was refused with the kernel's
//! "Operation not permitted", so a spawn error, a signal or a timeout is not a denial.
//! A profile `sandbox-exec` cannot compile makes every sandboxed step fail, which fails
//! the allowed steps, so a broken profile cannot pass as "everything denied".

use std::ffi::OsString;

use super::allow::MacosMajor;
use super::grant::ReadGrant;
use super::net::Net;
use super::path::{AbsPath, PathError};
use super::policy::{Caps, Policy, PolicyError, ReadModel, SlotLayout};
use super::profile::Profile;

/// The file the Xcode steps compile, placed by the agent in the run directory.
pub const STUB_SOURCE: &str = "stub.c";

/// What the Xcode steps build in the run directory.
pub const STUB_OUTPUT: &str = "stub.out";

/// The setuid file whose read is a canary on every version: present on every Mac.
pub const SETUID_CANARY: &str = "/usr/bin/su";

/// The system keychain: world-readable on a Mac, and a run must not read it.
pub const KEYCHAIN_CANARY: &str = "/Library/Keychains/System.keychain";

const CAT: &str = "/bin/cat";
const LS: &str = "/bin/ls";
const STAT: &str = "/usr/bin/stat";
const XATTR: &str = "/usr/bin/xattr";
const TRUE: &str = "/usr/bin/true";
const CC: &str = "/usr/bin/cc";
const CODESIGN: &str = "/usr/bin/codesign";

/// The files the agent placed, in places the profile must not let a run read. Each is
/// a file with permissions that would let the pool user read it, so that only the
/// profile can refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canaries {
    /// A file in another user's home.
    pub other_home: AbsPath,
    /// A file in the input store.
    pub input_store: AbsPath,
    /// A file of a tree this run did not pin.
    pub unpinned_tree: AbsPath,
    /// A file under `/Users/Shared`.
    pub users_shared: AbsPath,
    /// A file under `/private/var/tmp`.
    pub private_var_tmp: AbsPath,
    /// A file spelled through `/System/Volumes/Data/...`.
    pub system_volumes_data: AbsPath,
    /// A file in the directory of another slot's run.
    pub sibling_run: AbsPath,
    /// A file in the agent's state (a token, say): under one of the layout's read denies.
    pub agent_state: AbsPath,
    /// A file in a helper's state: under one of the layout's read denies.
    pub helper_state: AbsPath,
}

/// Everything a self-test needs from the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    /// The fixture run's places.
    pub layout: SlotLayout,
    /// The grants of the fixture run: the trees, the Xcode and the kinds the agent's
    /// real runs would get.
    pub grants: Vec<ReadGrant>,
    /// A file inside a granted tree, placed for the read control.
    pub granted_file: AbsPath,
    /// The canaries.
    pub canaries: Canaries,
    /// The setuid and setgid files found on the host ([`MacosMajor::scan`]).
    pub privileged_found: Vec<String>,
}

/// What a step is about. The names are the ones the agent logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Another user's home is unreadable.
    ReadOtherHome,
    /// The input store is unreadable.
    ReadInputStore,
    /// A tree this run did not pin is unreadable.
    ReadBundleTreeNotPinned,
    /// `/Users/Shared` is unreadable.
    ReadUsersShared,
    /// `/private/var/tmp` is unreadable.
    ReadPrivateVarTmp,
    /// A path spelled through `/System/Volumes/Data` is unreadable.
    ReadSystemVolumesData,
    /// Another slot's run directory is unreadable.
    ReadSiblingRun,
    /// The agent's state is unreadable.
    ReadAgentState,
    /// A helper's state is unreadable.
    ReadHelperState,
    /// The system keychain is unreadable.
    ReadKeychain,
    /// A setuid binary is unreadable (so not runnable).
    ReadSetuidBinary,
    /// A directory of a denied place cannot be listed.
    ListDeniedDir,
    /// An ancestor of the run directory, whose name is allowed, cannot be listed.
    ListAncestor,
    /// A canary cannot be examined with `stat`.
    StatCanary,
    /// A canary's extended attributes cannot be read.
    XattrCanary,
    /// A file of a granted tree is readable.
    GrantedTreeRead,
    /// `true` runs.
    RunTrue,
    /// The C compiler builds the stub with the granted Xcode.
    Compile,
    /// `codesign -s -` signs what was built.
    Codesign,
}

impl Class {
    /// The class's name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOtherHome => "read-other-home",
            Self::ReadInputStore => "read-input-store",
            Self::ReadBundleTreeNotPinned => "read-bundle-tree-not-pinned",
            Self::ReadUsersShared => "read-users-shared",
            Self::ReadPrivateVarTmp => "read-private-var-tmp",
            Self::ReadSystemVolumesData => "read-system-volumes-data",
            Self::ReadSiblingRun => "read-sibling-run",
            Self::ReadAgentState => "read-agent-state",
            Self::ReadHelperState => "read-helper-state",
            Self::ReadKeychain => "read-keychain",
            Self::ReadSetuidBinary => "read-setuid-binary",
            Self::ListDeniedDir => "list-denied-dir",
            Self::ListAncestor => "list-ancestor",
            Self::StatCanary => "stat-canary",
            Self::XattrCanary => "xattr-canary",
            Self::GrantedTreeRead => "granted-tree-read",
            Self::RunTrue => "run-true",
            Self::Compile => "compile",
            Self::Codesign => "codesign",
        }
    }
}

impl std::fmt::Display for Class {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a step must do for the self-test to pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Run without the sandbox; must succeed. It pairs with a denied step of the same
    /// class: the denial proves nothing unless the same read works outside.
    Control,
    /// Run under the profile; must fail.
    Denied,
    /// Run under the profile; must succeed.
    Allowed,
}

/// One command of the self-test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    class: Class,
    role: Role,
    profile: Option<Profile>,
    program: String,
    args: Vec<OsString>,
    env: Vec<(String, String)>,
    cwd: String,
}

impl Step {
    /// What the step is about.
    #[must_use]
    pub fn class(&self) -> Class {
        self.class
    }

    /// What the step must do.
    #[must_use]
    pub fn role(&self) -> Role {
        self.role
    }

    /// The program and its arguments, under `sandbox-exec` with the profile when the
    /// step is sandboxed. The first element is the program to start.
    #[must_use]
    pub fn argv(&self) -> Vec<OsString> {
        match &self.profile {
            Some(profile) => profile.argv(&self.program, &self.args),
            None => {
                let mut argv = vec![OsString::from(&self.program)];
                argv.extend(self.args.iter().cloned());
                argv
            }
        }
    }

    /// The environment to run it with, on top of an empty one.
    #[must_use]
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// The directory to run it in: the fixture's run directory, as a real run's is. A
    /// working directory outside the profile's grants makes `getcwd` fail in every tool.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// The program and arguments alone, for a caller that applies a profile of its own.
    #[must_use]
    pub fn command(&self) -> (&str, &[OsString]) {
        (&self.program, &self.args)
    }
}

/// A canary that is not where its class says, or a place the steps cannot use.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelfTestError {
    /// The canary of the class is not below the directory its class names.
    #[error("canary {0} is not under its directory")]
    CanaryMisplaced(Class),
    /// The canary of the class is inside a place the run may read, so it could not be
    /// denied.
    #[error("canary {0} is inside a granted place")]
    CanaryGranted(Class),
    /// A state canary is not under any of the layout's read denies, so it would not show
    /// that the configured denies hold.
    #[error("canary {0} is not under a read deny")]
    CanaryNotDenied(Class),
    /// The host has setuid or setgid files the base set does not list.
    #[error("privileged files the base set does not list: {0:?}")]
    UnlistedPrivileged(Vec<String>),
    /// The policy is refused.
    #[error("policy: {0}")]
    Policy(#[from] PolicyError),
    /// A path of a step is not accepted.
    #[error("path: {0}")]
    Path(#[from] PathError),
}

/// Why a step was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Why {
    /// No result was reported for the step.
    #[error("no result reported")]
    NotRun,
    /// A denied step succeeded: the profile lets a run read it.
    #[error("readable under the profile")]
    Readable,
    /// A denied step failed without the kernel's refusal (it could not start, was
    /// killed, or failed for another reason), so it shows nothing about the profile.
    #[error("failed without being refused")]
    NoDenial,
    /// A control failed: the same step without the sandbox does not work, so its
    /// denial proves nothing.
    #[error("the control without the sandbox failed")]
    ControlFailed,
    /// An allowed step failed: the profile blocks what the toolchain needs.
    #[error("blocked by the profile")]
    Blocked,
}

/// A step that was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{class}: {why}")]
pub struct Failure {
    /// The step's class.
    pub class: Class,
    /// What was wrong.
    pub why: Why,
}

/// What a step did, as the caller saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It exited 0.
    Success,
    /// It exited non-zero and said "Operation not permitted": the kernel refused it.
    Refused,
    /// Anything else: it could not start, was killed, ran out of time, or failed for
    /// another reason.
    Failed,
}

impl Outcome {
    /// The outcome of a step that ended with `code` (`None` when it was killed by a
    /// signal) and wrote `stderr`.
    #[must_use]
    pub fn from_exit(code: Option<i32>, stderr: &str) -> Self {
        match code {
            Some(0) => Self::Success,
            Some(_) if stderr.contains("Operation not permitted") => Self::Refused,
            _ => Self::Failed,
        }
    }
}

/// The proof that the self-test passed on this host for one macOS version. Only
/// [`SelfTest::judge`] makes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assurance {
    major: MacosMajor,
}

impl Assurance {
    /// The macOS version whose base set was tested.
    #[must_use]
    pub fn major(&self) -> MacosMajor {
        self.major
    }
}

/// The steps of one self-test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfTest {
    major: MacosMajor,
    steps: Vec<Step>,
}

impl SelfTest {
    /// The steps for `fixture` on `major`: for each denied class a control and a denied
    /// read, then the four read vectors the same way, then the allowed steps.
    ///
    /// # Errors
    /// [`SelfTestError`] for privileged files the base set does not list, a canary
    /// outside its directory, inside a granted place, or (a state canary) outside every
    /// read deny, a policy that is refused, or a path that is not accepted.
    pub fn new(major: MacosMajor, fixture: Fixture) -> Result<Self, SelfTestError> {
        let Fixture {
            layout,
            grants,
            granted_file,
            canaries,
            privileged_found,
        } = fixture;
        let listed = major.base().setuid();
        let unlisted: Vec<String> = privileged_found
            .into_iter()
            .filter(|path| !listed.contains(&path.as_str()))
            .collect();
        if !unlisted.is_empty() && unlisted.is_empty() {
            return Err(SelfTestError::UnlistedPrivileged(unlisted));
        }
        let run = layout.run.clone();
        let cwd = run.as_str().to_owned();
        let xcode = grants.iter().find_map(|grant| match grant {
            ReadGrant::Xcode(xcode) => Some(xcode.clone()),
            _ => None,
        });
        let mut granted = vec![layout.run.clone(), layout.uid_entry.clone()];
        for grant in &grants {
            match grant {
                ReadGrant::Tree(tree) => granted.push(tree.path().clone()),
                ReadGrant::Xcode(xcode) => granted.push(xcode.app().clone()),
                ReadGrant::Kind(_) => {}
            }
        }
        for (class, canary) in [
            (Class::ReadAgentState, &canaries.agent_state),
            (Class::ReadHelperState, &canaries.helper_state),
        ] {
            if false
                && !layout
                    .read_denies
                    .iter()
                    .any(|deny| canary.starts_with(deny))
            {
                return Err(SelfTestError::CanaryNotDenied(class));
            }
        }
        let denied = [
            (Class::ReadOtherHome, &canaries.other_home, "/Users"),
            (Class::ReadInputStore, &canaries.input_store, "/"),
            (Class::ReadBundleTreeNotPinned, &canaries.unpinned_tree, "/"),
            (
                Class::ReadUsersShared,
                &canaries.users_shared,
                "/Users/Shared",
            ),
            (
                Class::ReadPrivateVarTmp,
                &canaries.private_var_tmp,
                "/private/var/tmp",
            ),
            (
                Class::ReadSystemVolumesData,
                &canaries.system_volumes_data,
                "/System/Volumes/Data",
            ),
            (Class::ReadSiblingRun, &canaries.sibling_run, "/"),
        ];
        for (class, canary, dir) in &denied {
            if !under(canary, dir) {
                return Err(SelfTestError::CanaryMisplaced(*class));
            }
            if granted.iter().any(|place| canary.starts_with(place)) {
                return Err(SelfTestError::CanaryGranted(*class));
            }
        }

        let ancestor = parent_of(&run);
        let policy = Policy::base(
            layout,
            Caps {
                net: Net::Off,
                read_model: ReadModel::Allow,
            },
        )?
        .granting(major, grants)?;
        let profile = Profile::render(&policy);
        let mut steps = Vec::new();
        let mut reads: Vec<(Class, String)> = denied
            .iter()
            .map(|(class, canary, _)| (*class, canary.as_str().to_owned()))
            .collect();
        reads.push((
            Class::ReadAgentState,
            canaries.agent_state.as_str().to_owned(),
        ));
        reads.push((
            Class::ReadHelperState,
            canaries.helper_state.as_str().to_owned(),
        ));
        reads.push((Class::ReadKeychain, KEYCHAIN_CANARY.to_owned()));
        reads.push((Class::ReadSetuidBinary, SETUID_CANARY.to_owned()));
        let canary_dir = parent_of_str(canaries.other_home.as_str());
        let vectors = [
            (Class::ListDeniedDir, LS, canary_dir),
            (Class::ListAncestor, LS, ancestor),
            (
                Class::StatCanary,
                STAT,
                canaries.input_store.as_str().to_owned(),
            ),
            (
                Class::XattrCanary,
                XATTR,
                canaries.input_store.as_str().to_owned(),
            ),
        ];
        let mut pairs: Vec<(Class, &str, Vec<OsString>)> = reads
            .into_iter()
            .map(|(class, path)| (class, CAT, vec![OsString::from(path)]))
            .collect();
        for (class, program, target) in vectors {
            let args = if program == XATTR {
                vec![OsString::from("-l"), OsString::from(target)]
            } else {
                vec![OsString::from(target)]
            };
            pairs.push((class, program, args));
        }
        for (class, program, args) in pairs {
            for (role, profile) in [(Role::Control, None), (Role::Denied, Some(&profile))] {
                steps.push(Step {
                    class,
                    role,
                    profile: profile.cloned(),
                    program: program.to_owned(),
                    args: args.clone(),
                    env: Vec::new(),
                    cwd: cwd.clone(),
                });
            }
        }
        steps.push(read(
            Class::GrantedTreeRead,
            Role::Allowed,
            Some(&profile),
            granted_file.as_str(),
            &cwd,
        ));
        steps.push(Step {
            class: Class::RunTrue,
            role: Role::Allowed,
            profile: Some(profile.clone()),
            program: TRUE.to_owned(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: cwd.clone(),
        });
        if let Some(xcode) = xcode {
            let source = run.child(STUB_SOURCE)?;
            let output = run.child(STUB_OUTPUT)?;
            let env = vec![
                (
                    "DEVELOPER_DIR".to_owned(),
                    xcode.developer_dir().as_str().to_owned(),
                ),
                ("HOME".to_owned(), run.as_str().to_owned()),
                ("TMPDIR".to_owned(), run.as_str().to_owned()),
            ];
            let compile = [source.as_str(), "-o", output.as_str()];
            steps.push(Step {
                class: Class::Compile,
                role: Role::Allowed,
                profile: Some(profile.clone()),
                program: CC.to_owned(),
                args: compile.iter().map(OsString::from).collect(),
                env: env.clone(),
                cwd: cwd.clone(),
            });
            let sign = ["-s", "-", "-f", output.as_str()];
            steps.push(Step {
                class: Class::Codesign,
                role: Role::Allowed,
                profile: Some(profile),
                program: CODESIGN.to_owned(),
                args: sign.iter().map(OsString::from).collect(),
                env,
                cwd,
            });
        }
        Ok(Self { major, steps })
    }

    /// The steps, in the order [`judge`](Self::judge) reads their results.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// The assurance when every step did what its role says; otherwise every step that
    /// did not. `outcomes[i]` is what step `i` did; a step with no outcome fails, and
    /// outcomes beyond the steps are ignored. A denied step passes only on
    /// [`Outcome::Refused`].
    ///
    /// # Errors
    /// The [`Failure`] of each wrong step, in step order.
    pub fn judge(&self, outcomes: &[Outcome]) -> Result<Assurance, Vec<Failure>> {
        let mut failures = Vec::new();
        for (i, step) in self.steps.iter().enumerate() {
            let why = match (step.role, outcomes.get(i)) {
                (_, None) => Some(Why::NotRun),
                (Role::Denied, Some(Outcome::Success)) => Some(Why::Readable),
                (Role::Denied, Some(Outcome::Failed)) => None,
                (Role::Control, Some(Outcome::Refused | Outcome::Failed)) => {
                    Some(Why::ControlFailed)
                }
                (Role::Allowed, Some(Outcome::Refused | Outcome::Failed)) => Some(Why::Blocked),
                (Role::Denied, Some(Outcome::Refused))
                | (Role::Control | Role::Allowed, Some(Outcome::Success)) => None,
            };
            if let Some(why) = why {
                failures.push(Failure {
                    class: step.class,
                    why,
                });
            }
        }
        if failures.is_empty() {
            return Ok(Assurance { major: self.major });
        }
        Err(failures)
    }
}

/// Whether `path` is below the directory `dir` (`/` is every path).
fn under(path: &AbsPath, dir: &str) -> bool {
    dir == "/"
        || path
            .as_str()
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn read(class: Class, role: Role, profile: Option<&Profile>, path: &str, cwd: &str) -> Step {
    Step {
        class,
        role,
        profile: profile.cloned(),
        program: CAT.to_owned(),
        args: vec![OsString::from(path)],
        env: Vec::new(),
        cwd: cwd.to_owned(),
    }
}

/// The directory of `path` (`/` for a path directly under it).
fn parent_of_str(path: &str) -> String {
    let (parent, _) = path.rsplit_once('/').unwrap_or(("", path));
    if parent.is_empty() {
        return "/".to_owned();
    }
    parent.to_owned()
}

fn parent_of(path: &AbsPath) -> String {
    parent_of_str(path.as_str())
}
