//! A job that runs a program on the host with no image: [`NativeSpec`], a legacy form.
//!
//! A start carries an `image` (a container job, or with a [`crate::RunSpec`] the one job
//! definition of both platforms) or a `native` body, never both. An agent that predates this
//! body reads the empty `image` of a native start as `invalid-image` and runs nothing, so a
//! native job cannot run as a container anywhere. The body is read and checked as before
//! for this release, by the checks it shares with [`crate::RunSpec`]; a later release removes
//! it, and a server sends it only to a host that did not report the `one-job` capability
//! ([`crate::JobSpec::downgrade_for`]). Every enum here decodes only the values this version
//! knows: an unknown one is a decode error, which inside a reply reads as
//! [`crate::Command::Unknown`], so an agent never runs a job whose confinement it cannot
//! read.

use std::collections::BTreeMap;

use js_types::PortMap;
use serde::{Deserialize, Serialize};

use crate::command::{Isolation, JobSpec};
use crate::error::{Rule, ValidationError};
use crate::network;
use crate::refusal;
use crate::run::{self, RunSpec};

/// The prefix of a program that names an entrypoint of the job's bundle.
pub const BUNDLE_PROGRAM_PREFIX: &str = "@bundle/";

/// The most bytes an entrypoint name has.
pub const BUNDLE_ENTRYPOINT_MAX: usize = 64;

/// How much network a native job asks for. An absent value is [`NetworkLevel::Off`]. It is
/// not the level of a container job ([`js_types::JobNetwork`], `JobSpec::network`), which a
/// native job leaves `none`, but three of its values are that level on a Mac (design 5.8):
/// `off` is `none`, `on` is `allow-list` and `open` is `open`.
///
/// A Mac agent refuses a start that asks for more than its `--native-network` allows
/// (`network-above-ceiling`) and never runs it with less.
///
/// An agent from before [`NetworkLevel::Open`] reads `open` as a value it does not know: the
/// start fails to decode (inside a reply it reads as [`crate::Command::Unknown`]) and runs
/// nothing, so it never runs with no network or with a filter it was not asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkLevel {
    /// No network.
    #[default]
    Off,
    /// The loopback interface only.
    Loopback,
    /// The network, limited to the endpoints of the pool's packet filter (`allow-list`).
    On,
    /// The network with no filter (`open`): any address the Mac can reach.
    Open,
}

/// Whether an input is changed by the job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// The job reads the input and cannot change it.
    #[default]
    ReadOnly,
    /// The job gets its own writable copy.
    Writable,
}

/// What a declared output is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    /// One regular file.
    #[default]
    File,
    /// A directory and what is below it.
    Dir,
}

/// One input made visible inside a native job's run directory, at `dest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputRef {
    /// A tree the agent admitted earlier, named by its content digest.
    Stage {
        /// `sha256:` and 64 lower case hex digits.
        stage_id: String,
        /// Where it appears: relative, below the exec root, as a mount's `rel`.
        dest: String,
        /// Whether the job may change it. Absent means read-only.
        #[serde(default)]
        placement: Placement,
    },
    /// A directory below one of the agent's configured mount roots, read-only.
    Mount {
        /// The root's name, as [`crate::Mount::root_name`].
        root_name: String,
        /// The path below the root, as [`crate::Mount::rel`].
        rel: String,
        /// Where it appears, as for [`InputRef::Stage`].
        dest: String,
    },
}

/// One path the agent collects when the job ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSpec {
    /// Relative to the exec root, as a mount's `rel`.
    pub path: String,
    /// A file or a directory. Absent means a file.
    #[serde(default)]
    pub kind: OutputKind,
}

/// The body of a native start: a program run on the host as an unprivileged user, in a
/// run directory the agent makes. Everything but `program` may be left out on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeSpec {
    /// The program: an absolute path with no empty, `.` or `..` part, at most
    /// [`crate::limits::MAX_PROGRAM_LEN`] bytes. The agent also requires it to resolve
    /// under one of its configured tool roots. Or `@bundle/<entrypoint>`, the name of an entrypoint
    /// of the job's bundle (`[A-Za-z0-9._+-]`, the first a letter or digit, at most
    /// [`BUNDLE_ENTRYPOINT_MAX`] bytes), which the agent resolves from the bundle's
    /// config; the job then has a `bundle` ([`JobSpec::bundle`]), or it is refused.
    pub program: String,
    /// The arguments after the program: at most [`crate::limits::MAX_ARGS`] entries, no
    /// NUL, bounded as the `args` of a container job are. The `args` of the job itself must be empty.
    #[serde(default)]
    pub args: Vec<String>,
    /// The environment, literal and not secret: at most [`crate::limits::MAX_ENV`] entries. A
    /// name is `[A-Za-z_][A-Za-z0-9_]*` and is not reserved
    /// ([`js_types::env::env_name_reserved`]: the loader's, the sandbox's, the agent's
    /// `JS_` prefix, launchd's, the allocator's and the Objective-C runtime's prefixes, and
    /// the names the agent sets itself, `HOME` and `TMPDIR` among them); a value holds no NUL. A value
    /// may hold a newline or other control characters, so whatever logs the environment
    /// must escape them.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The working directory: relative to the exec root, as a mount's `rel`, or empty for
    /// the exec root itself.
    #[serde(default)]
    pub cwd: String,
    /// What appears in the run directory: at most [`crate::limits::MAX_INPUTS`], no two at
    /// one `dest` and none below another's `dest`, compared without regard to case.
    #[serde(default)]
    pub inputs: Vec<InputRef>,
    /// What to collect at the end: at most [`crate::limits::MAX_OUTPUTS`], none twice, none
    /// below or above another output or an input `dest`, compared without regard to case.
    #[serde(default)]
    pub outputs: Vec<OutputSpec>,
    /// The most network the job may use. Absent means none.
    #[serde(default)]
    pub network: NetworkLevel,
    /// The ports the job declares ([`PortMap`]), bounded and checked as the `ports` of a
    /// container job are (at most [`crate::limits::MAX_PORTS`], each port above zero, no
    /// host port twice; refused `invalid-job` under `native.ports`). Left out when there
    /// are none, so a body without ports is the text it was before the field.
    ///
    /// A Mac agent refuses a start that declares any (`ports-not-supported`): it forwards
    /// no port yet. An agent from before the field ignores it, so a server sends a native
    /// body with ports to no host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMap>,
}

impl NativeSpec {
    /// Checks the body alone, without the job around it: the checks of [`RunSpec`], with
    /// the field names `native.*`, then its declared ports.
    pub fn validate(&self) -> Result<(), ValidationError> {
        RunSpec::from(self.clone()).check(&run::NATIVE)?;
        network::check_ports(&network::NATIVE_PORTS, &self.ports)
    }
}

/// What a native start must also hold of the job around it: the container-only fields are
/// unused, and a native job is always contained (it is confined by user and profile and
/// does not taint the host).
pub(crate) fn check_job(job: &JobSpec, native: &NativeSpec) -> Result<(), ValidationError> {
    // A program inside a bundle needs the bundle: with none the agent would look for the
    // tool on the host.
    let wants_bundle = native.program.starts_with(BUNDLE_PROGRAM_PREFIX);
    if wants_bundle && job.bundle.is_none() {
        return Err(ValidationError::new(
            "native.program",
            Rule::Inconsistent,
            refusal::INVALID_PROGRAM,
        ));
    }
    let clashes = [
        ("isolation", job.isolation == Isolation::Host),
        ("args", !job.args.is_empty()),
        ("gpu_indices", !job.gpu_indices.is_empty()),
        ("mounts", !job.mounts.is_empty()),
        ("network", !job.network.is_none()),
        ("ports", !job.ports.is_empty()),
    ];
    match clashes.iter().find(|(_, clash)| *clash) {
        Some((field, _)) => Err(ValidationError::new(
            field,
            Rule::Inconsistent,
            refusal::INVALID_JOB,
        )),
        None => Ok(()),
    }
}
