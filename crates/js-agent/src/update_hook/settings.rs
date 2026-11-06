//! What the hook's package says about how it runs: the unit it manages, where its record is,
//! the runtime its agent must have and the bound on each step. The values are the package
//! manifest's `[params]` table (see [`params`](super::params)); this module holds the checked
//! form the steps and the record's rules use.
//!
//! The release directories are not settings: the updater passes the staged and the previous
//! release in the table's reserved keys (`updater.staged_release` and
//! `updater.previous_release`), and the record is checked against those.

use std::path::PathBuf;

use js_types::ExecMode;

/// The runtimes an agent can list on the second line of `--version`.
pub(super) const RUNTIMES: [&str; 3] = ["crun", "docker", "native"];

/// The bound on each step, in seconds: the manifest's `[hook.timeout_secs]`, repeated in the
/// `[params]` table so that the hook knows what the updater will do to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// The checked settings of a hook run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The directory of the hook record: `root:root`, mode 0700, an absolute path of plain
    /// components.
    pub state_dir: PathBuf,
    /// The unit that runs the agent (a systemd unit name or a launchd label); a record's unit
    /// must be this one.
    pub unit: String,
    /// The runtime the unit's agent must have (`crun`, `docker` or `native`): the staged agent
    /// must list it, and the agent that says hello after the cutover runs jobs the way it
    /// does.
    pub runtime: String,
    /// The bound on each step.
    pub timeouts: Timeouts,
    /// The time the updater allows from the start of `verify` to the end of a `rollback` that
    /// follows it; never less than the two bounds.
    pub verify_deadline_secs: u64,
}

impl Settings {
    /// The execution mode the hello of an agent with the required runtime names (`runtime` of
    /// the host, in the lease server's reading): `sandbox-exec` for `native`, `container` for
    /// the runtimes that run containers.
    #[must_use]
    pub fn hello_runtime(&self) -> &'static str {
        match self.runtime.as_str() {
            "native" => ExecMode::SandboxExec.name(),
            _ => ExecMode::Container.name(),
        }
    }
}
