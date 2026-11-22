//! The job file of `__mac-job`: what to run, for which operating system, with what.
//!
//! ```json
//! {
//!   "id": "c-hello-1",
//!   "os": "macos",
//!   "arch": "arm64",
//!   "program": "/bin/sh",
//!   "args": ["build.sh"],
//!   "env": {"LANG": "C"},
//!   "inputs": "/Users/adi/bringup/70-c-hello.files/src",
//!   "outputs": ["out"],
//!   "network": {"loopback": true, "outbound_ports": [8980]},
//!   "wall_limit_secs": 600
//! }
//! ```
//!
//! The job names its operating system (`os`, required) and may name its architecture; the
//! scheduler placed it, so the agent only refuses one that cannot run here at all. An unknown
//! field is an error, so a job never runs with a part of it ignored.

use std::collections::BTreeMap;
use std::path::Path;

use js_types::{Arch, Os};
use serde::Deserialize;
use thiserror::Error;

/// The most a job's wall limit may be: a day.
pub const WALL_LIMIT_MAX_SECS: u64 = 86_400;

/// The longest job id.
pub const ID_MAX: usize = 63;

/// The environment variables the agent sets itself, which a job may not set.
pub const RESERVED_ENV: [&str; 9] = [
    "HOME",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "SHELL",
    "XDG_CACHE_HOME",
    "CFFIXED_USER_HOME",
    "CLANG_MODULE_CACHE_PATH",
    "PATH",
];

/// A job, as its file holds it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobFile {
    /// The job's id: 1 to [`ID_MAX`] of `[a-z0-9-]`, the first a letter or digit. It names the
    /// job folder.
    pub id: String,
    /// The operating system the job is for.
    pub os: Os,
    /// The architecture it is for, if it names one.
    #[serde(default)]
    pub arch: Option<Arch>,
    /// The program: an absolute path.
    pub program: String,
    /// Its arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Its environment, on top of what the agent sets ([`RESERVED_ENV`]).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// A folder whose contents are copied into the job's `work` folder before the start.
    #[serde(default)]
    pub inputs: Option<String>,
    /// Paths under `work` to copy to the output folder at the end.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// The network the job may use.
    #[serde(default)]
    pub network: Network,
    /// The wall limit, in seconds.
    pub wall_limit_secs: u64,
}

/// The network a job may use. By default, none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Listening on and connecting to the loopback address.
    #[serde(default)]
    pub loopback: bool,
    /// TCP ports the job may connect to on any address.
    #[serde(default)]
    pub outbound_ports: Vec<u16>,
}

/// Why a job file is refused.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SpecError {
    /// The file is not a job.
    #[error("the job file does not decode: {0}")]
    Decode(String),
    /// The id breaks its rule.
    #[error("id {0:?}: 1 to 63 of [a-z0-9-], the first a letter or digit")]
    Id(String),
    /// The job is for another operating system.
    #[error("the job is for {0}; this host runs macos jobs")]
    Os(Os),
    /// The job is for another architecture.
    #[error("the job is for {wanted}; this host is {here}")]
    Arch {
        /// What the job names.
        wanted: Arch,
        /// What this host is.
        here: Arch,
    },
    /// The program is not an absolute path.
    #[error("program {0:?} is not an absolute path")]
    Program(String),
    /// A string holds a NUL.
    #[error("{0} holds a NUL")]
    Nul(&'static str),
    /// An environment name is malformed or reserved.
    #[error("env {0:?}: not [A-Za-z_][A-Za-z0-9_]*, or set by the agent")]
    Env(String),
    /// The inputs folder is not an absolute path.
    #[error("inputs {0:?} is not an absolute path")]
    Inputs(String),
    /// An output is not a plain relative path.
    #[error("output {0:?} is not a relative path of plain parts")]
    Output(String),
    /// Port 0 cannot be connected to.
    #[error("outbound port 0")]
    Port,
    /// The wall limit is 0 or more than a day.
    #[error("wall_limit_secs {0}: 1 to 86400")]
    WallLimit(u64),
}

impl JobFile {
    /// Decodes and checks a job file, for a host of architecture `here`.
    pub fn parse(text: &str, here: Arch) -> Result<Self, SpecError> {
        let job: Self =
            serde_json::from_str(text).map_err(|why| SpecError::Decode(why.to_string()))?;
        job.check(here)?;
        Ok(job)
    }

    /// The checks of a decoded job.
    fn check(&self, here: Arch) -> Result<(), SpecError> {
        if !id_ok(&self.id) {
            return Err(SpecError::Id(self.id.clone()));
        }
        if self.os != Os::Macos {
            return Err(SpecError::Os(self.os));
        }
        if let Some(wanted) = self.arch.filter(|&wanted| wanted != here) {
            return Err(SpecError::Arch { wanted, here });
        }
        if self.program.contains('\0') {
            return Err(SpecError::Nul("program"));
        }
        if !Path::new(&self.program).is_absolute() {
            return Err(SpecError::Program(self.program.clone()));
        }
        if self.args.iter().any(|a| a.contains('\0')) {
            return Err(SpecError::Nul("args"));
        }
        if let Some(name) = self.env.keys().find(|name| !env_name_ok(name)) {
            return Err(SpecError::Env(name.clone()));
        }
        if self.env.values().any(|v| v.contains('\0')) {
            return Err(SpecError::Nul("env"));
        }
        if let Some(inputs) = &self.inputs {
            if inputs.contains('\0') {
                return Err(SpecError::Nul("inputs"));
            }
            if !Path::new(inputs).is_absolute() {
                return Err(SpecError::Inputs(inputs.clone()));
            }
        }
        if let Some(output) = self.outputs.iter().find(|o| !relative_ok(o)) {
            return Err(SpecError::Output(output.clone()));
        }
        if self.network.outbound_ports.contains(&0) {
            return Err(SpecError::Port);
        }
        if !(1..=WALL_LIMIT_MAX_SECS).contains(&self.wall_limit_secs) {
            return Err(SpecError::WallLimit(self.wall_limit_secs));
        }
        Ok(())
    }
}

/// Whether `id` is 1 to [`ID_MAX`] of `[a-z0-9-]`, the first a letter or digit.
fn id_ok(id: &str) -> bool {
    let bytes = id.as_bytes();
    let first_ok = bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    first_ok
        && bytes.len() <= ID_MAX
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Whether `name` is `[A-Za-z_][A-Za-z0-9_]*` and not one the agent sets.
fn env_name_ok(name: &str) -> bool {
    let bytes = name.as_bytes();
    let first_ok = bytes
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_');
    first_ok
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        && !RESERVED_ENV.contains(&name)
}

/// Whether `path` is relative and made of plain parts only (no `.`, `..`, root or empty part).
fn relative_ok(path: &str) -> bool {
    !path.contains('\0') && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;
