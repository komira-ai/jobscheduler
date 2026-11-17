//! The mount providers an agent was given (`--mount-provider NAME=SOCKET` and
//! `--mount-provider-wait SECS`, `docs/design/mount-providers.md`), checked when the agent
//! starts, and the step that gives them to the lifecycle once it is built.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{Event, Lifecycle};
use crate::system::Machine;

/// How many seconds a start waits on its providers when `--mount-provider-wait` is not given.
pub const DEFAULT_PROVIDER_WAIT_SECS: u64 = 120;

/// The most seconds `--mount-provider-wait` takes.
pub const MAX_PROVIDER_WAIT_SECS: u64 = 900;

/// The providers the flags name, each with the socket it serves on, and how long a start may
/// wait on them. No provider is the agent's default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountProviders {
    sockets: Vec<(String, PathBuf)>,
    wait_secs: u64,
}

impl MountProviders {
    /// Checks the flags: each name is a provider's name ([`js_agent_wire::is_provider_name`])
    /// and given once, the directory of each socket exists (a provider that is not running
    /// yet is not waited for: the socket is looked for when a call is made), and the wait is
    /// 1 to [`MAX_PROVIDER_WAIT_SECS`] seconds and given only with a provider.
    ///
    /// # Errors
    ///
    /// The text names the flag at fault.
    pub fn from_flags(given: &[(String, PathBuf)], wait_secs: Option<u64>) -> Result<Self, String> {
        let mut names = BTreeSet::new();
        for (name, socket) in given {
            if !js_agent_wire::is_provider_name(name) {
                return Err(format!(
                    "--mount-provider {name:?}: not a usable provider name"
                ));
            }
            if !names.insert(name.as_str()) {
                return Err(format!("--mount-provider {name}: given twice"));
            }
            // A bare file name is in the directory the agent runs in.
            let dir = socket
                .parent()
                .filter(|dir| !dir.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            if !dir.is_dir() {
                return Err(format!(
                    "--mount-provider {name}: {} is not an existing directory",
                    dir.display()
                ));
            }
        }
        let wait_secs = match (wait_secs, given.is_empty()) {
            (Some(_), true) => {
                return Err("--mount-provider-wait: needs --mount-provider".to_owned());
            }
            (Some(secs), false) if !(1..=MAX_PROVIDER_WAIT_SECS).contains(&secs) => {
                return Err(format!(
                    "--mount-provider-wait: {secs} is not from 1 to {MAX_PROVIDER_WAIT_SECS} seconds"
                ));
            }
            (Some(secs), false) => secs,
            (None, _) => DEFAULT_PROVIDER_WAIT_SECS,
        };
        Ok(Self {
            sockets: given.to_vec(),
            wait_secs,
        })
    }

    /// The providers' names and sockets, in the order given.
    #[must_use]
    pub fn sockets(&self) -> &[(String, PathBuf)] {
        &self.sockets
    }

    /// How long a start may wait on its providers, from its admission, in milliseconds.
    #[must_use]
    pub fn wait_ms(&self) -> u64 {
        self.wait_secs.saturating_mul(1000)
    }

    /// Whether the agent was given no provider.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sockets.is_empty()
    }

    /// Gives `life` the providers, as the `machine` makes their client for the agent that
    /// registered as `host_id`; nothing when the agent was given none. The events are what
    /// attaching found of an earlier process.
    ///
    /// # Errors
    ///
    /// The machine could not make the client; the text says why.
    pub fn attach(
        &self,
        life: &mut Lifecycle,
        machine: &dyn Machine,
        host_id: &str,
    ) -> Result<Vec<Event>, String> {
        if self.is_empty() {
            return Ok(Vec::new());
        }
        let client = machine.providers(host_id, &self.sockets)?;
        Ok(life.attach_providers(client, self.wait_ms()))
    }
}
