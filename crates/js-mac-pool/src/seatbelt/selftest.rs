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
//! A pass is an [`Assurance`], which [`Policy::allowing`](super::Policy::allowing)
//! takes; a failure lists every step that was wrong and why, and the agent then refuses
//! the bundle and allow-model capabilities and logs the list. The open model is
//! unaffected. This crate starts nothing: the caller runs [`Step::argv`] with the
//! environment of [`Step::env`], as the pool user, and reports which steps exited 0.
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

const CAT: &str = "/bin/cat";
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
    /// A setuid binary is unreadable (so not runnable).
    ReadSetuidBinary,
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
            Self::ReadSetuidBinary => "read-setuid-binary",
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
    /// read, then the allowed steps.
    ///
    /// # Errors
    /// [`SelfTestError`] for a canary outside its directory or inside a granted place, a
    /// policy that is refused, or a path that is not accepted.
    pub fn new(major: MacosMajor, fixture: Fixture) -> Result<Self, SelfTestError> {
        let Fixture {
            layout,
            grants,
            granted_file,
            canaries,
        } = fixture;
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
        let denied = [
            (Class::ReadOtherHome, canaries.other_home, "/Users"),
            (Class::ReadInputStore, canaries.input_store, "/"),
            (Class::ReadBundleTreeNotPinned, canaries.unpinned_tree, "/"),
            (
                Class::ReadUsersShared,
                canaries.users_shared,
                "/Users/Shared",
            ),
            (
                Class::ReadPrivateVarTmp,
                canaries.private_var_tmp,
                "/private/var/tmp",
            ),
            (
                Class::ReadSystemVolumesData,
                canaries.system_volumes_data,
                "/System/Volumes/Data",
            ),
        ];
        for (class, canary, dir) in &denied {
            if !under(canary, dir) {
                return Err(SelfTestError::CanaryMisplaced(*class));
            }
            if granted.iter().any(|place| canary.starts_with(place)) {
                return Err(SelfTestError::CanaryGranted(*class));
            }
        }

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
        reads.push((Class::ReadSetuidBinary, SETUID_CANARY.to_owned()));
        for (class, path) in reads {
            steps.push(read(class, Role::Control, None, &path, &cwd));
            steps.push(read(class, Role::Denied, Some(&profile), &path, &cwd));
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
    /// did not. `ran[i]` is whether step `i` exited 0; a step with no result fails, and
    /// results beyond the steps are ignored.
    ///
    /// # Errors
    /// The [`Failure`] of each wrong step, in step order.
    pub fn judge(&self, ran: &[bool]) -> Result<Assurance, Vec<Failure>> {
        let mut failures = Vec::new();
        for (i, step) in self.steps.iter().enumerate() {
            let why = match (step.role, ran.get(i)) {
                (_, None) => Some(Why::NotRun),
                (Role::Denied, Some(true)) => Some(Why::Readable),
                (Role::Control, Some(false)) => Some(Why::ControlFailed),
                (Role::Allowed, Some(false)) => Some(Why::Blocked),
                (Role::Denied, Some(false)) | (Role::Control | Role::Allowed, Some(true)) => None,
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
