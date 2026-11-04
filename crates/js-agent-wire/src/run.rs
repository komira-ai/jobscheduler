//! What runs from a job's image, on either platform: [`RunSpec`].
//!
//! A start that carries a `run` names its job by the image alone (`JobSpec::image`, never
//! empty beside a run). A macOS host with a bundle store runs the program in the bundle the
//! image resolves to; a Linux host is meant to run it in the image's container, but no
//! container agent of this release does. A container agent refuses a start with a `run`
//! (`run-unsupported`), and a Mac agent refuses a start named by its image that has no
//! `run`, or has no bundle store (`image-unsupported-here`); a server sends a `run` only to a host that reported `one-job`
//! and writes the legacy form for any other ([`crate::JobSpec::downgrade_for`]). The body holds the fields of the legacy
//! [`NativeSpec`] with the same bounds, and both are checked by the one implementation
//! here; only the field names a refusal reports differ (`run.*` and `native.*`).
//!
//! Every enum here decodes only the values this version knows: an unknown one is a decode
//! error, which inside a reply reads as [`crate::Command::Unknown`], so an agent never runs a
//! job whose confinement it cannot read.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::check::{self, At};
use crate::command::Mount;
use crate::error::{Rule, ValidationError};
use crate::limits;
use crate::native::{
    BUNDLE_ENTRYPOINT_MAX, BUNDLE_PROGRAM_PREFIX, InputRef, NativeSpec, NetworkLevel, OutputSpec,
};
use crate::refusal;

/// The program a job's image runs, and what it is given: the fields of [`NativeSpec`] beside
/// a non-empty image. Everything but `program` may be left out on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSpec {
    /// The program: an absolute path with no empty, `.` or `..` part, at most
    /// [`limits::MAX_PROGRAM_LEN`] bytes. Or `@bundle/<entrypoint>`, the name of an
    /// entrypoint (`[A-Za-z0-9._+-]`, the first a letter or digit, at most
    /// [`BUNDLE_ENTRYPOINT_MAX`] bytes) of the bundle the job's image resolves to; no
    /// `bundle` field names it.
    pub program: String,
    /// The arguments after the program: at most [`limits::MAX_ARGS`] entries, no NUL, bounded
    /// as the `args` of a container job are. The `args` of the job itself must be empty.
    #[serde(default)]
    pub args: Vec<String>,
    /// The environment, literal and not secret, as [`NativeSpec::env`]: at most
    /// [`limits::MAX_ENV`] entries, a name `[A-Za-z_][A-Za-z0-9_]*` that is not reserved
    /// ([`js_types::env::env_name_reserved`]), a value with no NUL.
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

/// The field names the checks of a body report under.
pub(crate) struct Names {
    program: &'static str,
    args: &'static str,
    env: &'static str,
    cwd: &'static str,
    inputs: &'static str,
    outputs: &'static str,
}

/// The names of a [`RunSpec`]'s fields.
const RUN: Names = Names {
    program: "run.program",
    args: "run.args",
    env: "run.env",
    cwd: "run.cwd",
    inputs: "run.inputs",
    outputs: "run.outputs",
};

/// The names of a [`NativeSpec`]'s fields.
pub(crate) const NATIVE: Names = Names {
    program: "native.program",
    args: "native.args",
    env: "native.env",
    cwd: "native.cwd",
    inputs: "native.inputs",
    outputs: "native.outputs",
};

impl RunSpec {
    /// Checks the body alone, without the job around it.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check(&RUN)
    }

    /// The checks of a body, reporting under `names`: those of a run, and of the legacy
    /// native body ([`NativeSpec::validate`]).
    pub(crate) fn check(&self, names: &Names) -> Result<(), ValidationError> {
        self.check_program(At::new(names.program, refusal::INVALID_PROGRAM))?;
        check::args(At::new(names.args, refusal::INVALID_JOB), &self.args)?;
        crate::env::check_env(names.env, &self.env)?;
        rel_path(At::new(names.cwd, refusal::INVALID_JOB), &self.cwd, true)?;
        self.check_inputs(At::new(names.inputs, refusal::INVALID_JOB))?;
        self.check_outputs(At::new(names.outputs, refusal::INVALID_JOB))
    }

    fn check_program(&self, at: At) -> Result<(), ValidationError> {
        let program = &self.program;
        if let Some(entrypoint) = program.strip_prefix(BUNDLE_PROGRAM_PREFIX) {
            return check::token(at, entrypoint, BUNDLE_ENTRYPOINT_MAX, entrypoint_byte);
        }
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

    fn check_inputs(&self, at: At) -> Result<(), ValidationError> {
        check::count(at, self.inputs.len(), limits::MAX_INPUTS)?;
        for input in &self.inputs {
            check_input(at, input)?;
        }
        let dests: Vec<&str> = self.inputs.iter().map(dest).collect();
        disjoint(at, &dests)
    }

    fn check_outputs(&self, at: At) -> Result<(), ValidationError> {
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
            .map(dest)
            .chain(self.outputs.iter().map(|output| output.path.as_str()))
            .collect::<Vec<_>>();
        disjoint(at, &paths)
    }
}

/// The run of a native body. Its declared ports are not kept: a run has no field for them,
/// and [`NativeSpec::validate`] checks them on its own.
impl From<NativeSpec> for RunSpec {
    fn from(native: NativeSpec) -> Self {
        Self {
            program: native.program,
            args: native.args,
            env: native.env,
            cwd: native.cwd,
            inputs: native.inputs,
            outputs: native.outputs,
            network: native.network,
        }
    }
}

impl From<RunSpec> for NativeSpec {
    fn from(run: RunSpec) -> Self {
        Self {
            program: run.program,
            args: run.args,
            env: run.env,
            cwd: run.cwd,
            inputs: run.inputs,
            outputs: run.outputs,
            network: run.network,
            // A run declares no ports: the one job definition has no field for them yet.
            ports: Vec::new(),
        }
    }
}

/// Where an input appears.
fn dest(input: &InputRef) -> &str {
    match input {
        InputRef::Stage { dest, .. } | InputRef::Mount { dest, .. } => dest,
    }
}

fn check_input(at: At, input: &InputRef) -> Result<(), ValidationError> {
    rel_path(at, dest(input), false)?;
    match input {
        InputRef::Stage { stage_id, .. } => stage_digest(at, stage_id),
        InputRef::Mount { root_name, rel, .. } => Mount {
            root_name: root_name.clone(),
            rel: rel.clone(),
            ro: true,
        }
        .validate(),
    }
}

/// `[A-Za-z0-9._+-]`: the bytes of an entrypoint name.
fn entrypoint_byte(byte: u8) -> bool {
    check::ident_byte(byte) || byte == b'+'
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
