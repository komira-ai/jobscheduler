//! A job that runs a program on the host with no image: [`NativeSpec`].
//!
//! A start carries either an `image` (a container job) or a `native` body, never both. An
//! agent that predates this body reads the empty `image` of a native start as
//! `invalid-image` and runs nothing, so a native job cannot run as a container anywhere.
//! Every enum here decodes only the values this version knows: an unknown one is a decode
//! error, which inside a reply reads as [`crate::Command::Unknown`], so an agent never
//! runs a job whose confinement it cannot read.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::check::{self, At};
use crate::command::{Isolation, JobSpec, Mount};
use crate::error::{Rule, ValidationError};
use crate::limits;
use crate::refusal;

/// How much network a native job may use. The agent clamps it to what its own flag
/// allows. An absent value is [`NetworkLevel::Off`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkLevel {
    /// No network.
    #[default]
    Off,
    /// The loopback interface only.
    Loopback,
    /// The network.
    On,
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
        /// The root's name, as [`Mount::root_name`].
        root_name: String,
        /// The path below the root, as [`Mount::rel`].
        rel: String,
        /// Where it appears, as for [`InputRef::Stage`].
        dest: String,
    },
}

impl InputRef {
    fn dest(&self) -> &str {
        match self {
            Self::Stage { dest, .. } | Self::Mount { dest, .. } => dest,
        }
    }

    fn check(&self) -> Result<(), ValidationError> {
        let at = At::new("native.inputs", refusal::INVALID_JOB);
        rel_path(at, self.dest(), false)?;
        match self {
            Self::Stage { stage_id, .. } => stage_digest(at, stage_id),
            Self::Mount { root_name, rel, .. } => Mount {
                root_name: root_name.clone(),
                rel: rel.clone(),
                ro: true,
            }
            .validate(),
        }
    }
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
    /// [`limits::MAX_PROGRAM_LEN`] bytes. The agent also requires it to resolve under one
    /// of its configured tool roots.
    pub program: String,
    /// The arguments after the program: at most [`limits::MAX_ARGS`] entries, no NUL, bounded
    /// as the `args` of a container job are. The `args` of the job itself must be empty.
    #[serde(default)]
    pub args: Vec<String>,
    /// The environment, literal and not secret: at most [`limits::MAX_ENV`] entries. A name
    /// is `[A-Za-z_][A-Za-z0-9_]*` and does not start `DYLD_`, `LD_`, `SANDBOX_`,
    /// `JS_AGENT_`, `__XPC_`, `MALLOC`, `Malloc` or `OBJC_`; a value holds no NUL. A value
    /// may hold a newline or other control characters, so whatever logs the environment
    /// must escape them.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The working directory: relative to the exec root, as a mount's `rel`, or empty for
    /// the exec root itself.
    #[serde(default)]
    pub cwd: String,
    /// What appears in the run directory: at most [`limits::MAX_INPUTS`], no two at one
    /// `dest` and none below another's `dest`, compared without regard to case.
    #[serde(default)]
    pub inputs: Vec<InputRef>,
    /// What to collect at the end: at most [`limits::MAX_OUTPUTS`], none twice, none below
    /// or above another output or an input `dest`, compared without regard to case.
    #[serde(default)]
    pub outputs: Vec<OutputSpec>,
    /// The most network the job may use. Absent means none.
    #[serde(default)]
    pub network: NetworkLevel,
}

/// Name prefixes the agent keeps for itself and the dynamic loader and sandbox read.
/// `MALLOC` covers `MALLOC_*`; `Malloc` covers `MallocStackLogging` and `MallocLogFile`,
/// which make the allocator write files. `__XPC_` is launchd's own, `OBJC_` the Objective-C
/// runtime's debug switches.
const RESERVED_ENV_PREFIXES: [&str; 8] = [
    "DYLD_",
    "LD_",
    "SANDBOX_",
    "JS_AGENT_",
    "__XPC_",
    "MALLOC",
    "Malloc",
    "OBJC_",
];

impl NativeSpec {
    /// Checks the body alone, without the job around it.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check_program()?;
        check::args(At::new("native.args", refusal::INVALID_JOB), &self.args)?;
        self.check_env()?;
        rel_path(At::new("native.cwd", refusal::INVALID_JOB), &self.cwd, true)?;
        self.check_inputs()?;
        self.check_outputs()
    }

    fn check_program(&self) -> Result<(), ValidationError> {
        let at = At::new("native.program", refusal::INVALID_PROGRAM);
        let program = &self.program;
        if program.is_empty() {
            return Err(at.fail(Rule::Empty));
        }
        if program.len() > limits::MAX_PROGRAM_LEN {
            return Err(at.fail(Rule::TooLong));
        }
        if program.chars().any(check::hidden) {
            return Err(at.fail(Rule::BadChars));
        }
        let Some(below) = program.strip_prefix('/') else {
            return Err(at.fail(Rule::Escapes));
        };
        if below.split('/').any(|part| part == "..") {
            return Err(at.fail(Rule::Escapes));
        }
        if below.split('/').any(|part| part.is_empty() || part == ".") {
            return Err(at.fail(Rule::Empty));
        }
        Ok(())
    }

    fn check_env(&self) -> Result<(), ValidationError> {
        let at = At::new("native.env", refusal::INVALID_ENV);
        check::count(at, self.env.len(), limits::MAX_ENV)?;
        let mut total = 0usize;
        for (key, value) in &self.env {
            env_name(at, key)?;
            if value.len() > limits::MAX_ENV_VALUE_LEN {
                return Err(at.fail(Rule::TooLong));
            }
            if value.contains('\0') {
                return Err(at.fail(Rule::BadChars));
            }
            total += key.len() + value.len();
        }
        if total > limits::MAX_ENV_TOTAL_BYTES {
            return Err(at.fail(Rule::TooLong));
        }
        Ok(())
    }

    fn check_inputs(&self) -> Result<(), ValidationError> {
        let at = At::new("native.inputs", refusal::INVALID_JOB);
        check::count(at, self.inputs.len(), limits::MAX_INPUTS)?;
        for input in &self.inputs {
            input.check()?;
        }
        let dests: Vec<&str> = self.inputs.iter().map(InputRef::dest).collect();
        disjoint(at, &dests)?;
        Ok(())
    }

    fn check_outputs(&self) -> Result<(), ValidationError> {
        let at = At::new("native.outputs", refusal::INVALID_JOB);
        check::count(at, self.outputs.len(), limits::MAX_OUTPUTS)?;
        for output in &self.outputs {
            rel_path(at, &output.path, false)?;
        }
        // An output may not repeat, nest in or contain another output, or share ground
        // with an input `dest`: the job would write into a staged input, or two collection
        // rules would claim one path.
        let paths = self
            .inputs
            .iter()
            .map(InputRef::dest)
            .chain(self.outputs.iter().map(|output| output.path.as_str()))
            .collect::<Vec<_>>();
        disjoint(at, &paths)
    }
}

/// What a native start must also hold of the job around it: the container-only fields are
/// unused, and a native job is always contained (it is confined by user and profile and
/// does not taint the host).
pub(crate) fn check_job(job: &JobSpec) -> Result<(), ValidationError> {
    let clashes = [
        ("isolation", job.isolation == Isolation::Host),
        ("args", !job.args.is_empty()),
        ("gpu_indices", !job.gpu_indices.is_empty()),
        ("mounts", !job.mounts.is_empty()),
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

fn env_name(at: At, key: &str) -> Result<(), ValidationError> {
    if key.is_empty() {
        return Err(at.fail(Rule::Empty));
    }
    if key.len() > limits::MAX_ENV_KEY_LEN {
        return Err(at.fail(Rule::TooLong));
    }
    let lead = key.as_bytes()[0];
    let leads = lead.is_ascii_alphabetic() || lead == b'_';
    let inside = key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if ![leads, inside].into_iter().all(|ok| ok) {
        return Err(at.fail(Rule::BadChars));
    }
    if RESERVED_ENV_PREFIXES.iter().any(|p| key.starts_with(p)) {
        return Err(at.fail(Rule::Reserved));
    }
    Ok(())
}

/// A relative path as a mount's `rel` is, or the empty path when `allow_empty`, and ASCII
/// only. The agent runs on case-insensitive APFS, where `A` and `a`, or `\u{e9}` composed
/// and decomposed, are one name; ASCII keeps the overlap checks exact with a lower-case
/// fold and needs no Unicode tables. Mount sources (`rel` of a mount input) are not
/// compared and keep the mount rules.
///
/// A `..` that reaches outside through a symlink is not visible here: refusing it is the
/// staging code's duty, when it resolves the path under the run directory.
fn rel_path(at: At, value: &str, allow_empty: bool) -> Result<(), ValidationError> {
    if value.is_empty() {
        return if allow_empty {
            Ok(())
        } else {
            Err(at.fail(Rule::Empty))
        };
    }
    if value.len() > limits::MAX_REL_LEN {
        return Err(at.fail(Rule::TooLong));
    }
    if value
        .chars()
        .any(|c| check::hidden(c) || !c.is_ascii() || matches!(c, ':' | ',' | '\\'))
    {
        return Err(at.fail(Rule::BadChars));
    }
    if value.starts_with('/') || value.split('/').any(|part| part == "..") {
        return Err(at.fail(Rule::Escapes));
    }
    if value.split('/').any(|part| part.is_empty() || part == ".") {
        return Err(at.fail(Rule::Empty));
    }
    Ok(())
}

/// `sha256:` and 64 lower case hex digits.
fn stage_digest(at: At, value: &str) -> Result<(), ValidationError> {
    let hex = value.strip_prefix("sha256:").unwrap_or("");
    let shaped = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    match shaped {
        true => Ok(()),
        false => Err(at.fail(Rule::BadChars)),
    }
}

/// No two paths are equal, and none is below another, once folded to lower case.
fn disjoint(at: At, paths: &[&str]) -> Result<(), ValidationError> {
    let folded: Vec<String> = paths.iter().map(|p| p.to_ascii_lowercase()).collect();
    let below = |inner: &str, outer: &str| {
        inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    for (index, one) in folded.iter().enumerate() {
        for two in &folded[index + 1..] {
            if one == two {
                return Err(at.fail(Rule::Duplicate));
            }
            if [below(one, two), below(two, one)].contains(&true) {
                return Err(at.fail(Rule::Inconsistent));
            }
        }
    }
    Ok(())
}
