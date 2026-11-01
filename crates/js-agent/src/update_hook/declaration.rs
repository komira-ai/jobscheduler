//! The hook's declaration, `/etc/updater.d/js-agent.toml` (host-updates design, 2.4 and 4.2):
//! the part of it the hook checks its record against.
//!
//! ```toml
//! state_dir = "/var/lib/js-agent-update"
//!
//! [hook]
//! units       = ["js-agent-crun.service"]
//! write_paths = ["/opt/js-agent", "/var/lib/js-agent-update"]
//!
//! # The runtime the unit's agent must have: `crun`, `docker` or `native`. Left out, it is the
//! # runtime of the platform the hook is built for (`crun` on Linux, `native` on macOS).
//! # runtime   = "crun"
//!
//! [hook.timeout_secs]
//! prepare = 3900
//! # cutover, verify, rollback, cleanup likewise
//!
//! [hook.params]
//! release_link = "/opt/js-agent/current"   # inside a write path, never one itself
//! ```
//!
//! Keys the hook does not read are ignored: the file is the updater's, and it adds keys.
//! The release directories are not read from the declaration: the updater passes the staged
//! and the previous release in the params table (`updater.staged_release` and
//! `updater.previous_release`), and the record is checked against those.

use std::io;
use std::path::{Path, PathBuf};

use js_types::ExecMode;
use serde::Deserialize;

use super::plain::is_plain;

/// The runtime a declaration that names none requires: what the unit of this platform runs jobs
/// with.
pub const DEFAULT_RUNTIME: &str = if cfg!(target_os = "macos") {
    "native"
} else {
    "crun"
};

/// The runtimes an agent can list on the second line of `--version`.
const RUNTIMES: [&str; 3] = ["crun", "docker", "native"];

/// Where the declaration is installed.
pub const DECLARATION_PATH: &str = "/etc/updater.d/js-agent.toml";

/// Why a declaration was refused.
#[derive(Debug, thiserror::Error)]
pub enum DeclarationError {
    /// The file could not be read.
    #[error("cannot read the declaration: {0}")]
    Read(#[from] io::Error),
    /// The file is not TOML, or lacks a key the hook needs, or has one of the wrong type.
    #[error("the declaration is not usable: {0}")]
    Parse(#[from] toml::de::Error),
    /// `state_dir` or a write path is not absolute and plain (no `.`, `..`, doubled or
    /// trailing slash).
    #[error("state_dir and every write path must be absolute paths of plain components")]
    NotPlain,
    /// `state_dir` is not one of `write_paths`: the hook could not write its own record.
    #[error("state_dir is not listed in hook.write_paths")]
    StateDirNotListed,
    /// `state_dir` lies inside another write path, or another lies inside it: a release or
    /// the agent could then reach the record.
    #[error("state_dir and the other write paths must not lie inside one another")]
    StateDirNested,
    /// `hook.runtime` is not one of the runtimes an agent has.
    #[error("hook.runtime {0:?} is not one of crun, docker, native")]
    UnknownRuntime(String),
    /// `hook.units` is empty: no unit could ever match the record's.
    #[error("hook.units is empty")]
    NoUnits,
    /// `verify_deadline_secs` is shorter than the `verify` bound plus the `rollback` bound: the
    /// updater would roll back a verify that is still allowed to run.
    #[error(
        "verify_deadline_secs {deadline} is less than the verify bound {verify} plus the rollback bound {rollback}"
    )]
    VerifyDeadline {
        /// `verify_deadline_secs`.
        deadline: u64,
        /// `[hook.timeout_secs] verify`.
        verify: u64,
        /// `[hook.timeout_secs] rollback`.
        rollback: u64,
    },
    /// `hook.units` names more than one unit: the hook manages one agent.
    #[error("hook.units names more than one unit")]
    ManyUnits,
}

/// The `[hook.timeout_secs]` table: the updater's bound on each step, in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct Timeouts {
    /// `prepare`.
    pub prepare: u64,
    /// `cutover`.
    pub cutover: u64,
    /// `verify`.
    pub verify: u64,
    /// `rollback`.
    pub rollback: u64,
    /// `cleanup`.
    pub cleanup: u64,
}

#[derive(Deserialize)]
struct Wire {
    state_dir: PathBuf,
    #[allow(dead_code)]
    verify_deadline_secs: Option<u64>,
    hook: WireHook,
}

#[derive(Deserialize)]
struct WireHook {
    units: Vec<String>,
    write_paths: Vec<PathBuf>,
    timeout_secs: Timeouts,
    runtime: Option<String>,
}

/// What the hook reads of its declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// The directory of the hook record: `root:root`, mode 0700.
    pub state_dir: PathBuf,
    /// The units the hook may manage; a record's unit must be one of them.
    pub units: Vec<String>,
    /// Every directory the hook may write, the state directory among them.
    pub write_paths: Vec<PathBuf>,
    /// The bound on each step.
    pub timeouts: Timeouts,
    /// The runtime the unit's agent must have: the staged agent must list it, and the agent
    /// that says hello after the cutover runs jobs the way it does.
    pub runtime: String,
}

impl Declaration {
    /// The execution mode the hello of an agent with the declared runtime names (`runtime` of
    /// the host, in the lease server's reading): `sandbox-exec` for `native`, `container` for
    /// the runtimes that run containers.
    #[must_use]
    pub fn hello_runtime(&self) -> &'static str {
        match self.runtime.as_str() {
            "native" => ExecMode::SandboxExec.name(),
            _ => ExecMode::Container.name(),
        }
    }

    /// Reads the declaration at `path`.
    ///
    /// # Errors
    /// [`DeclarationError`]: unreadable, not a usable declaration, or a path that is not
    /// absolute.
    pub fn read(path: &Path) -> Result<Declaration, DeclarationError> {
        Declaration::parse(&std::fs::read_to_string(path)?)
    }

    /// Reads the text of a declaration.
    ///
    /// # Errors
    /// [`DeclarationError`]: see [`Declaration::read`].
    pub fn parse(text: &str) -> Result<Declaration, DeclarationError> {
        let wire: Wire = toml::from_str(text)?;
        let declaration = Declaration {
            state_dir: wire.state_dir,
            units: wire.hook.units,
            write_paths: wire.hook.write_paths,
            timeouts: wire.hook.timeout_secs,
            runtime: wire
                .hook
                .runtime
                .unwrap_or_else(|| DEFAULT_RUNTIME.to_owned()),
        };
        let state_dir = &declaration.state_dir;
        let paths = &declaration.write_paths;
        if !is_plain(state_dir) || !paths.iter().all(|path| is_plain(path)) {
            return Err(DeclarationError::NotPlain);
        }
        if !paths.contains(state_dir) {
            return Err(DeclarationError::StateDirNotListed);
        }
        let nested = paths
            .iter()
            .filter(|path| *path != state_dir)
            .any(|path| path.starts_with(state_dir) || state_dir.starts_with(path));
        if nested {
            return Err(DeclarationError::StateDirNested);
        }
        if !RUNTIMES.contains(&declaration.runtime.as_str()) {
            return Err(DeclarationError::UnknownRuntime(declaration.runtime));
        }
        if declaration.units.is_empty() {
            return Err(DeclarationError::NoUnits);
        }
        if declaration.units.len() > 1 {
            return Err(DeclarationError::ManyUnits);
        }
        Ok(declaration)
    }
}
